//! fake-llama-server: a stand-in for llama-server used by Relay's tests and GPU-free demos.
//!
//! Accepts llama-server's arguments (-m, --port, --host, -c, …), prints similar log lines,
//! answers /health (503 while "loading", then 200), /v1/models, /tokenize, /apply-template,
//! /completion (prompt as text or token ids; `n_predict: 0` only fills the cache) and streaming
//! /v1/chat/completions.
//!
//! Tokens: 4 bytes each; a token id holds its bytes (little-endian), so ids can be decoded.
//!
//! Extra flags: `--fake-load-ms N` (default 1200), `--fake-fail` (print a CUDA error and exit 1),
//! `--fake-tps N` (simulated tokens/s, default 400), `--fake-reasoning` (stream reasoning_content
//! first, like gpt-oss).
//!
//! Answers: if env `FAKE_LLM_SCRIPT` points at a JSON array of
//! `{"contains": "...", "response": "...", "reasoning": "..."}`, the first rule whose `contains`
//! occurs in the last user message wins. Otherwise a synthetic answer is produced: for every
//! file under "# Files to edit" it emits a valid SEARCH/REPLACE block appending a marker comment
//! (or creates new files), so any spec flows through Relay's pipeline.
//!
//! Prompt cache: like llama-server with one slot, the part of a prompt that matches the previous
//! request (its prompt plus the generated answer) counts as cached: `timings.cache_n`, with
//! `timings.prompt_n` the rest (estimated as bytes / 4).
//!
//! `--fake-recurrent` simulates a hybrid/recurrent (or sliding-window) model: the cache can only
//! be reused in full when the new prompt extends everything cached; otherwise only up to a
//! checkpoint, and a request leaves a checkpoint shortly before the end of its prompt.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Opts {
    model: String,
    host: String,
    port: u16,
    ctx: u64,
    load_ms: u64,
    fail: bool,
    tps: f64,
    reasoning: bool,
    recurrent: bool,
}

fn parse_args() -> Opts {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut o = Opts {
        model: String::new(),
        host: "127.0.0.1".into(),
        port: 9931,
        ctx: 4096,
        load_ms: 1200,
        fail: false,
        tps: 400.0,
        reasoning: false,
        recurrent: false,
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let next = || args.get(i + 1).cloned().unwrap_or_default();
        match a {
            "-m" | "--model" => {
                o.model = next();
                i += 1;
            }
            "--host" => {
                o.host = next();
                i += 1;
            }
            "--port" => {
                o.port = next().parse().unwrap_or(9931);
                i += 1;
            }
            "-c" | "--ctx-size" => {
                o.ctx = next().parse().unwrap_or(4096);
                i += 1;
            }
            "--fake-load-ms" => {
                o.load_ms = next().parse().unwrap_or(1200);
                i += 1;
            }
            "--fake-tps" => {
                o.tps = next().parse().unwrap_or(400.0);
                i += 1;
            }
            "--fake-fail" => o.fail = true,
            "--fake-reasoning" => o.reasoning = true,
            "--fake-recurrent" => o.recurrent = true,
            _ => {}
        }
        i += 1;
    }
    o
}

fn log(start: Instant, level: &str, msg: &str) {
    let t = start.elapsed();
    eprintln!(
        "0.{:02}.{:03}.000 {level} {msg}",
        t.as_secs(),
        t.subsec_millis()
    );
}

fn main() {
    let o = Arc::new(parse_args());
    let start = Instant::now();
    log(start, "I", "srv  llama_server: initializing ... (fake)");
    log(
        start,
        "I",
        &format!("srv    load_model: loading model '{}'", o.model),
    );
    if o.fail {
        std::thread::sleep(Duration::from_millis(300));
        log(
            start,
            "E",
            "CUDA error: shared object initialization failed",
        );
        eprintln!("ggml-cuda.cu:113: CUDA error");
        std::process::exit(1);
    }
    let ready = Arc::new(AtomicBool::new(false));
    let listener = match TcpListener::bind((o.host.as_str(), o.port)) {
        Ok(l) => l,
        Err(e) => {
            log(
                start,
                "E",
                &format!(
                    "srv  couldn't bind HTTP server socket, hostname: {}, port: {}: {e}",
                    o.host, o.port
                ),
            );
            std::process::exit(1);
        }
    };
    {
        let ready = ready.clone();
        let o = o.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(o.load_ms * 8 / 10));
            log(
                start,
                "I",
                "cmn          init: llama threadpool init, n_threads = 16",
            );
            log(start, "I", &format!("srv    load_model: initializing, n_slots = 1, n_ctx_slot = {}, kv_unified = 'false'", o.ctx));
            std::thread::sleep(Duration::from_millis(o.load_ms * 2 / 10));
            log(start, "I", "srv  server_run_i: model loaded");
            ready.store(true, Ordering::SeqCst);
        });
    }
    log(
        start,
        "I",
        &format!(
            "srv  server_on_li: listening on http://{}:{}",
            o.host, o.port
        ),
    );
    for stream in listener.incoming().flatten() {
        let ready = ready.clone();
        let o = o.clone();
        std::thread::spawn(move || {
            let _ = handle(stream, &ready, &o, start);
        });
    }
}

fn respond(mut s: &TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "OK",
    };
    write!(s, "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn handle(stream: TcpStream, ready: &AtomicBool, o: &Opts, start: Instant) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h)?;
        if h == "\r\n" || h.is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let is_ready = ready.load(Ordering::SeqCst);
    match (method.as_str(), path.as_str()) {
        ("GET", "/health") | ("GET", "/v1/health") => {
            if is_ready {
                respond(&stream, 200, r#"{"status":"ok"}"#)
            } else {
                respond(
                    &stream,
                    503,
                    r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#,
                )
            }
        }
        ("GET", "/v1/models") => respond(
            &stream,
            200,
            &json!({"object":"list","data":[{"id": o.model, "object":"model"}]}).to_string(),
        ),
        ("POST", "/tokenize") => {
            let tokens = encode(body["content"].as_str().unwrap_or(""));
            respond(&stream, 200, &json!({ "tokens": tokens }).to_string())
        }
        ("POST", "/apply-template") => respond(
            &stream,
            200,
            &json!({ "prompt": render_prompt(&body) }).to_string(),
        ),
        ("POST", "/completion") => {
            if !is_ready {
                return respond(
                    &stream,
                    503,
                    r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#,
                );
            }
            let prompt = match &body["prompt"] {
                Value::String(t) => t.clone(),
                Value::Array(ids) => decode(ids),
                _ => String::new(),
            };
            if body["n_predict"].as_i64() != Some(0) {
                return respond(
                    &stream,
                    400,
                    r#"{"error":{"code":400,"message":"fake-llama-server: /completion only supports n_predict 0"}}"#,
                );
            }
            let cached = reuse(&prompt, o.recurrent);
            *SLOT.lock().unwrap() = prompt.clone();
            let (total, cache_n) = (prompt.len().div_ceil(4), cached / 4);
            log(
                start,
                "I",
                &format!("slot update_slots: prompt evaluated into cache, n_past = {cache_n} of {total} (fake, n_predict = 0)"),
            );
            respond(
                &stream,
                200,
                &json!({"content": "", "stop": true, "tokens_evaluated": total, "tokens_cached": cache_n,
                        "timings": {"cache_n": cache_n, "prompt_n": total - cache_n, "predicted_n": 0}})
                .to_string(),
            )
        }
        ("POST", "/v1/chat/completions") => {
            if !is_ready {
                return respond(
                    &stream,
                    503,
                    r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#,
                );
            }
            chat(stream, &body, o, start)
        }
        _ => respond(
            &stream,
            404,
            r#"{"error":{"code":404,"message":"not found"}}"#,
        ),
    }
}

fn last_user(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|x| x["role"] == "user"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string()
}

fn first_user(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.iter().find(|x| x["role"] == "user"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string()
}

/// Synthetic answer: one valid block per file in "# Files to edit".
fn synthetic(prompt: &str) -> String {
    let mut out = String::new();
    let Some(files_start) = prompt.find("# Files to edit") else {
        return "I could not find any files.".into();
    };
    let section = &prompt[files_start..];
    // The section ends at the next heading Relay writes (`# Specification`, `# Read-only context`);
    // other `# ` lines are file content, e.g. Python comments.
    let end = ["\n# Specification\n", "\n# Read-only context"]
        .iter()
        .filter_map(|h| section.find(h))
        .min()
        .unwrap_or(section.len());
    let section = &section[..end];
    let mut rest = section;
    while let Some(h) = rest.find("\n## `") {
        let after = &rest[h + 5..];
        let Some(end) = after.find('`') else { break };
        let path = after[..end].to_string();
        let body = &after[end + 1..];
        let comment = if path.ends_with(".py")
            || path.ends_with(".sh")
            || path.ends_with(".toml")
            || path.ends_with(".yml")
        {
            "#"
        } else {
            "//"
        };
        if body.trim_start().starts_with("(New file") {
            out.push_str(&format!("{path}\n<<<<<<< SEARCH\n=======\n{comment} created by fake-llama-server\n>>>>>>> REPLACE\n\n"));
        } else if let Some(fs) = body.find("\n```") {
            // Content is between the opening fence line and the closing fence.
            let fenced = &body[fs + 1..];
            let open_end = fenced.find('\n').unwrap_or(0);
            let fence: String = fenced.chars().take_while(|c| *c == '`').collect();
            let content = &fenced[open_end + 1..];
            let close = content.find(&format!("\n{fence}")).unwrap_or(content.len());
            let content = &content[..close];
            let last = content
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            if !last.is_empty() {
                out.push_str(&format!("{path}\n<<<<<<< SEARCH\n{last}\n=======\n{last}\n{comment} edited by fake-llama-server\n>>>>>>> REPLACE\n\n"));
            }
        }
        rest = &after[end..];
    }
    out
}

fn sse(mut s: &TcpStream, v: &Value) -> std::io::Result<()> {
    write!(s, "data: {v}\n\n")?;
    s.flush()
}

/// The previous request's prompt and answer, as the single slot's KV cache would hold them.
static SLOT: Mutex<String> = Mutex::new(String::new());
/// Byte positions in SLOT where a recurrent model could resume (`--fake-recurrent`).
static CHECKPOINTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Bytes of `prompt` that can be reused from the slot, and record this request's checkpoint.
fn reuse(prompt: &str, recurrent: bool) -> usize {
    let slot = SLOT.lock().unwrap();
    let mut cps = CHECKPOINTS.lock().unwrap();
    let common = slot
        .bytes()
        .zip(prompt.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let reused = if !recurrent || common == slot.len() {
        common
    } else {
        cps.iter().copied().filter(|&c| c <= common).max().unwrap_or(0)
    };
    // Whatever followed the reuse point is overwritten by this request.
    cps.retain(|&c| c <= reused);
    // llama-server checkpoints a few tokens before the end of the prompt.
    cps.push(prompt.len().saturating_sub(16));
    if cps.len() > 32 {
        cps.remove(0);
    }
    reused
}

/// 4 bytes per token; the id is the bytes, little-endian (zero-padded).
fn encode(text: &str) -> Vec<u32> {
    text.as_bytes()
        .chunks(4)
        .map(|c| {
            let mut b = [0u8; 4];
            b[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(b)
        })
        .collect()
}

fn decode(ids: &[Value]) -> String {
    let mut bytes = Vec::new();
    for id in ids {
        let b = (id.as_u64().unwrap_or(0) as u32).to_le_bytes();
        let n = b.iter().rposition(|x| *x != 0).map(|i| i + 1).unwrap_or(0);
        bytes.extend_from_slice(&b[..n]);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn render_prompt(body: &Value) -> String {
    let mut s = String::new();
    for m in body["messages"].as_array().into_iter().flatten() {
        s.push_str(m["role"].as_str().unwrap_or(""));
        s.push('\n');
        s.push_str(m["content"].as_str().unwrap_or(""));
        s.push('\n');
    }
    s
}

fn chat(stream: TcpStream, body: &Value, o: &Opts, start: Instant) -> std::io::Result<()> {
    let prompt = render_prompt(body);
    let cached_bytes = reuse(&prompt, o.recurrent);
    let user = last_user(body);
    let mut response = String::new();
    let mut reasoning = String::new();
    if let Ok(path) = std::env::var("FAKE_LLM_SCRIPT") {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(Value::Array(rules)) = serde_json::from_str::<Value>(&text) {
                if let Some(r) = rules
                    .iter()
                    .find(|r| user.contains(r["contains"].as_str().unwrap_or("\u{0}")))
                {
                    response = r["response"].as_str().unwrap_or("").to_string();
                    reasoning = r["reasoning"].as_str().unwrap_or("").to_string();
                }
            }
        }
    }
    if response.is_empty() {
        response = synthetic(&first_user(body));
    }
    if reasoning.is_empty() && o.reasoning {
        reasoning = "The user wants SEARCH/REPLACE blocks. I will copy the lines exactly.".into();
    }
    let thinking_disabled = body["chat_template_kwargs"]["enable_thinking"] == false;
    log(
        start,
        "I",
        &format!(
            "slot launch_slot_: processing task (enable_thinking={}, reasoning_effort={})",
            !thinking_disabled, body["reasoning_effort"]
        ),
    );
    let mut s = &stream;
    write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n")?;
    let delay = Duration::from_secs_f64(1.0 / o.tps.max(1.0));
    let t0 = Instant::now();
    let mut n = 0u64;
    let chunk = |field: &str, text: &str| json!({"choices":[{"index":0,"finish_reason":null,"delta":{field: text}}],"object":"chat.completion.chunk"});
    sse(
        s,
        &json!({"choices":[{"index":0,"finish_reason":null,"delta":{"role":"assistant","content":null}}]}),
    )?;
    for piece in split_tokens(&reasoning) {
        sse(s, &chunk("reasoning_content", &piece))?;
        n += 1;
        std::thread::sleep(delay);
    }
    let max = body["max_tokens"].as_u64().unwrap_or(u64::MAX);
    let mut finish = "stop";
    for piece in split_tokens(&response) {
        if n >= max {
            finish = "length";
            break;
        }
        sse(s, &chunk("content", &piece))?;
        n += 1;
        std::thread::sleep(delay);
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    sse(
        s,
        &json!({"choices":[{"index":0,"finish_reason":finish,"delta":{}}]}),
    )?;
    let total = prompt.len().div_ceil(4);
    let cache_n = cached_bytes / 4;
    let prompt_n = total - cache_n;
    *SLOT.lock().unwrap() = format!("{prompt}assistant\n{response}\n");
    log(
        start,
        "I",
        &format!("slot update_slots: n_past = {cache_n} of {total} prompt tokens (fake prompt cache)"),
    );
    sse(
        s,
        &json!({"choices":[],"usage":{"completion_tokens":n,"prompt_tokens":total,"total_tokens":n as usize+total},
        "timings":{"cache_n":cache_n,"prompt_n":prompt_n,"prompt_ms":5.0,"predicted_n":n,"predicted_ms":ms,"predicted_per_second": (n as f64)/(ms/1000.0).max(0.001)}}),
    )?;
    write!(s, "data: [DONE]\n\n")?;
    s.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Both);
    s = &stream;
    let _ = s;
    Ok(())
}

fn split_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        cur.push(c);
        if cur.len() >= 4 || c == '\n' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
