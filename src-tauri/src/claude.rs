//! Claude backends for the expand and review steps.
//!
//! * CLI (default): `claude -p --output-format stream-json --verbose --include-partial-messages`,
//!   prompt on stdin (avoids the 32K Windows command-line limit), run in the project folder with
//!   optional read-only tools so Claude can look around before writing the spec.
//! * API: `POST https://api.anthropic.com/v1/messages` with SSE streaming, adaptive thinking,
//!   `output_config.effort`, and server-side refusal fallbacks.

use crate::budget::estimate_tokens;
use crate::config::{ClaudeBackend, Profile, Settings};
use crate::editformat::fenced_lang;
use crate::events::{Sink, emit};
use crate::llm::SseParser;
use crate::pipeline::lang_for;
use crate::project::{list_files, read_text, tree_text};
use crate::templates::{self, Template};
use crate::winproc;
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
pub const API_VERSION: &str = "2026-10-21";
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
pub const KEYRING_SERVICE: &str = "Relay";
pub const KEYRING_ANTHROPIC: &str = "anthropic-api-key";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeResult {
    pub text: String,
    pub backend: String,
    pub model: String,
    pub duration_ms: u64,
    pub cost_usd: Option<f64>,
    pub stop_reason: Option<String>,
    pub cancelled: bool,
    /// Prompt tokens read from / written to Claude's prompt cache.
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    /// Local planner: generation speed and the tool calls it made.
    #[serde(default)]
    pub tok_per_sec: Option<f64>,
    #[serde(default)]
    pub tool_calls: Vec<String>,
}

pub struct ClaudeCall<'a> {
    pub job_id: &'a str,
    /// Stable instructions and project context, sent as (appended) system prompt so Claude's
    /// prompt cache can reuse them across expansions; `None` sends everything as the prompt.
    pub system: Option<String>,
    pub prompt: String,
    /// Working directory for the CLI (the project root). Tools are only enabled when set.
    pub cwd: Option<PathBuf>,
    pub allow_read_tools: bool,
}

fn ev(sink: &Sink, job_id: &str, kind: &str, text: impl Into<String>) {
    emit(sink, "claude-event", &json!({ "jobId": job_id, "kind": kind, "text": text.into() }));
}

// ---------------------------------------------------------------------------------------------
// Credentials (Windows Credential Manager)
// ---------------------------------------------------------------------------------------------

pub fn get_secret(account: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, account)
        .ok()?
        .get_password()
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn set_secret(account: &str, value: &str) -> Result<()> {
    let e = keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| anyhow!("credential store: {e}"))?;
    if value.is_empty() {
        let _ = e.delete_credential();
        return Ok(());
    }
    e.set_password(value)
        .map_err(|e| anyhow!("saving to Windows Credential Manager: {e}"))
}

// ---------------------------------------------------------------------------------------------
// CLI backend
// ---------------------------------------------------------------------------------------------

pub fn locate_cli() -> Option<PathBuf> {
    which::which("claude").ok()
}

pub const CLI_MISSING: &str = "The Claude Code CLI (`claude`) was not found on PATH. Install Claude Code (https://claude.com/claude-code), sign in once with `claude` in a terminal, then restart Relay — or switch the Claude backend to the Anthropic API in Settings.";

pub fn cli_args(model: &str, effort: &str, tools: bool) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--no-session-persistence",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if !model.trim().is_empty() {
        a.push("--model".into());
        a.push(model.trim().into());
    }
    if !effort.trim().is_empty() {
        a.push("--effort".into());
        a.push(effort.trim().into());
    }
    if tools {
        a.extend([
            "--tools".into(),
            "Read,Glob,Grep".into(),
            "--allowedTools".into(),
            "Read,Glob,Grep".into(),
        ]);
    } else {
        a.extend(["--tools".into(), String::new()]);
    }
    a
}

/// Parse one stream-json line from the CLI. Returns (event kind, text) pairs for the UI and the
/// final result when present.
pub fn parse_cli_line(line: &str) -> (Vec<(&'static str, String)>, Option<Value>) {
    let mut out = Vec::new();
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return (out, None);
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("stream_event") => {
            let e = &v["event"];
            if e["type"] == "content_block_delta" {
                match e["delta"]["type"].as_str() {
                    Some("text_delta") => out.push(("delta", e["delta"]["text"].as_str().unwrap_or("").to_string())),
                    Some("thinking_delta") => out.push(("thinking", e["delta"]["thinking"].as_str().unwrap_or("").to_string())),
                    _ => {}
                }
            }
        }
        Some("assistant") => {
            if let Some(blocks) = v["message"]["content"].as_array() {
                for b in blocks {
                    if b["type"] == "tool_use" {
                        let name = b["name"].as_str().unwrap_or("tool");
                        let input = &b["input"];
                        let detail = input
                            .get("file_path")
                            .or_else(|| input.get("pattern"))
                            .or_else(|| input.get("path"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        out.push(("tool", format!("{name} {detail}").trim().to_string()));
                        // Text produced before a tool call is exploration chatter, not the spec.
                        out.push(("segment", String::new()));
                    }
                }
            }
        }
        Some("result") => return (out, Some(v)),
        _ => {}
    }
    (out, None)
}

async fn run_cli(settings: &Settings, call: &ClaudeCall<'_>, sink: &Sink, cancel: &CancellationToken) -> Result<ClaudeResult> {
    let exe = locate_cli().ok_or_else(|| anyhow!(CLI_MISSING))?;
    let tools = call.allow_read_tools && call.cwd.is_some();
    let mut args = cli_args(&settings.claude_model, &settings.claude_effort, tools);
    // A file rather than an argument: the system part can exceed the 32K command-line limit.
    let system_file = match call.system.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(sys) => Some(SystemPromptFile::write(call.job_id, sys)?),
        None => None,
    };
    if let Some(f) = &system_file {
        args.push("--append-system-prompt-file".into());
        args.push(f.path.to_string_lossy().into_owned());
    }
    let ext = exe.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut cmd = if ext == "cmd" || ext == "bat" {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(&exe).args(&args);
        c
    } else {
        let mut c = tokio::process::Command::new(&exe);
        c.args(&args);
        c
    };
    if let Some(dir) = &call.cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    winproc::hide_window(&mut cmd);
    let started = Instant::now();
    let mut child = cmd.spawn().with_context(|| format!("starting {}", exe.display()))?;
    let job = winproc::TreeJob::new();
    if let (Some(j), Some(pid)) = (&job, child.id()) {
        j.assign_pid(pid);
    }
    ev(
        sink,
        call.job_id,
        "status",
        if tools {
            "Claude CLI started (read-only tools enabled)"
        } else {
            "Claude CLI started"
        },
    );

    let mut stdin = child.stdin.take().unwrap();
    let prompt = call.prompt.clone();
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let mut stderr = child.stderr.take().unwrap();
    let err_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let mut final_result: Option<Value> = None;
    let mut streamed = String::new();
    loop {
        let next = tokio::select! {
            l = lines.next_line() => l,
            _ = cancel.cancelled() => {
                if let Some(j) = &job { j.terminate(); }
                let _ = child.start_kill();
                return Ok(ClaudeResult { text: streamed, backend: "cli".into(), cancelled: true, duration_ms: started.elapsed().as_millis() as u64, ..Default::default() });
            }
        };
        let Ok(Some(line)) = next else { break };
        let (events, result) = parse_cli_line(&line);
        for (kind, text) in events {
            if kind == "delta" {
                streamed.push_str(&text);
            } else if kind == "segment" {
                streamed.clear();
            }
            ev(sink, call.job_id, kind, text);
        }
        if result.is_some() {
            final_result = result;
        }
    }
    let status = child.wait().await.ok();
    let stderr_text = err_task.await.unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as u64;
    let Some(r) = final_result else {
        let hint = if stderr_text.to_lowercase().contains("login") || stderr_text.to_lowercase().contains("auth") {
            " Run `claude` once in a terminal to sign in."
        } else {
            ""
        };
        bail!(
            "Claude CLI exited{} without a result. {}{}",
            status.and_then(|s| s.code()).map(|c| format!(" with code {c}")).unwrap_or_default(),
            stderr_text.trim().chars().take(800).collect::<String>(),
            hint
        );
    };
    if r["is_error"].as_bool() == Some(true) {
        bail!(
            "Claude CLI error: {}",
            r["result"].as_str().or(r["subtype"].as_str()).unwrap_or("unknown error")
        );
    }
    drop(system_file);
    Ok(ClaudeResult {
        text: r["result"].as_str().map(String::from).unwrap_or(streamed),
        backend: "cli".into(),
        model: settings.claude_model.clone(),
        duration_ms,
        cost_usd: r["total_cost_usd"].as_f64(),
        stop_reason: r["subtype"].as_str().map(String::from),
        cancelled: false,
        cache_read_tokens: r["usage"]["cache_read_input_tokens"].as_u64(),
        cache_write_tokens: r["usage"]["cache_creation_input_tokens"].as_u64(),
        ..Default::default()
    })
}

/// The system part of a CLI call, written to `%LOCALAPPDATA%\Relay\tmp` and removed on drop.
struct SystemPromptFile {
    path: PathBuf,
}

impl SystemPromptFile {
    fn write(job_id: &str, text: &str) -> Result<Self> {
        let dir = crate::config::app_dir().join("tmp");
        std::fs::create_dir_all(&dir)?;
        let safe: String = job_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        let path = dir.join(format!("system-{safe}-{}.md", uuid::Uuid::new_v4().simple()));
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for SystemPromptFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------------------------
// API backend
// ---------------------------------------------------------------------------------------------

pub fn api_body(model: &str, effort: &str, system: Option<&str>, prompt: &str) -> Value {
    let mut body = json!({
        "model": if model.trim().is_empty() { "claude-opus-5-5" } else { model.trim() },
        "max_tokens": 64000,
        "stream": true,
        "thinking": { "type": "adaptive", "display": "summarized" },
        "output_config": { "effort": if effort.trim().is_empty() { "high" } else { effort.trim() } },
        "fallbacks": "default",
        "messages": [{ "role": "user", "content": prompt }],
    });
    // The system part is shared by every expansion of the same project and profile; an explicit
    // breakpoint at its end lets the next expansion read it from the cache. (Automatic caching
    // would put the breakpoint after the instruction, which never repeats.)
    if let Some(sys) = system.filter(|s| !s.trim().is_empty()) {
        body["system"] = json!([{ "type": "text", "text": sys, "cache_control": { "type": "ephemeral" } }]);
    }
    body
}

fn api_error_message(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = v["error"]["message"].as_str().unwrap_or(body).chars().take(600).collect::<String>();
    match status {
        401 => format!("Anthropic API key rejected (401). Update it in Settings. {msg}"),
        403 => format!("This API key isn't allowed to use that model (403). {msg}"),
        404 => format!("Model not found or not available to your organization (404). Check the model ID in Settings. {msg}"),
        429 => format!("Rate limited by the Anthropic API (429). Try again shortly. {msg}"),
        529 => format!("The Anthropic API is overloaded (529). Try again shortly. {msg}"),
        _ => format!("Anthropic API error {status}: {msg}"),
    }
}

async fn run_api(
    settings: &Settings,
    call: &ClaudeCall<'_>,
    sink: &Sink,
    cancel: &CancellationToken,
    http: &reqwest::Client,
) -> Result<ClaudeResult> {
    let key = get_secret(KEYRING_ANTHROPIC)
        .ok_or_else(|| anyhow!("No Anthropic API key saved. Add one in Settings → Claude (it is stored in Windows Credential Manager)."))?;
    let body = api_body(
        &settings.claude_model,
        &settings.claude_effort,
        call.system.as_deref(),
        &call.prompt,
    );
    let started = Instant::now();
    let mut attempt = 0;
    let resp = loop {
        attempt += 1;
        let req = http
            .post(API_URL)
            .header("x-api-key", &key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", FALLBACK_BETA)
            .header("content-type", "application/json")
            .json(&body)
            .send();
        let resp = tokio::select! {
            r = req => r.map_err(|e| anyhow!("Could not reach the Anthropic API: {e}"))?,
            _ = cancel.cancelled() => return Ok(ClaudeResult { backend: "api".into(), cancelled: true, ..Default::default() }),
        };
        let s = resp.status().as_u16();
        if resp.status().is_success() {
            break resp;
        }
        let retryable = matches!(s, 429 | 500 | 529);
        let text = resp.text().await.unwrap_or_default();
        if retryable && attempt <= 2 {
            ev(sink, call.job_id, "status", format!("API returned {s}; retrying…"));
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
            continue;
        }
        bail!(api_error_message(s, &text));
    };
    ev(sink, call.job_id, "status", "Streaming from the Anthropic API");
    let mut parser = SseParser::default();
    let mut stream = resp.bytes_stream();
    let mut text = String::new();
    let mut stop_reason = None;
    let mut model = String::new();
    let mut cache_read_tokens = None;
    let mut cache_write_tokens = None;
    let mut pending = Vec::<u8>::new();
    'outer: loop {
        let next = tokio::select! {
            n = stream.next() => n,
            _ = cancel.cancelled() => {
                return Ok(ClaudeResult { text, backend: "api".into(), cancelled: true, duration_ms: started.elapsed().as_millis() as u64, ..Default::default() });
            }
        };
        let Some(chunk) = next else { break };
        pending.extend_from_slice(&chunk.map_err(|e| anyhow!("API stream dropped: {e}"))?);
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(e) => e.valid_up_to(),
        };
        let s = String::from_utf8_lossy(&pending[..valid]).into_owned();
        pending.drain(..valid);
        for payload in parser.feed(&s) {
            let Ok(v) = serde_json::from_str::<Value>(&payload) else { continue };
            match v["type"].as_str() {
                Some("message_start") => {
                    model = v["message"]["model"].as_str().unwrap_or("").to_string();
                    cache_read_tokens = v["message"]["usage"]["cache_read_input_tokens"].as_u64();
                    cache_write_tokens = v["message"]["usage"]["cache_creation_input_tokens"].as_u64();
                }
                Some("content_block_start") if v["content_block"]["type"] == "fallback" => {
                    ev(
                        sink,
                        call.job_id,
                        "status",
                        format!(
                            "Request was re-routed to fallback model {}",
                            v["content_block"]["to"]["model"].as_str().unwrap_or("?")
                        ),
                    );
                }
                Some("content_block_delta") => match v["delta"]["type"].as_str() {
                    Some("text_delta") => {
                        let t = v["delta"]["text"].as_str().unwrap_or("");
                        text.push_str(t);
                        ev(sink, call.job_id, "delta", t);
                    }
                    Some("thinking_delta") => ev(sink, call.job_id, "thinking", v["delta"]["thinking"].as_str().unwrap_or("")),
                    _ => {}
                },
                Some("message_delta") => {
                    if let Some(sr) = v["delta"]["stop_reason"].as_str() {
                        stop_reason = Some(sr.to_string());
                    }
                }
                Some("error") => bail!(
                    "Anthropic API stream error: {}",
                    v["error"]["message"].as_str().unwrap_or("unknown")
                ),
                Some("message_stop") => break 'outer,
                _ => {}
            }
        }
    }
    match stop_reason.as_deref() {
        Some("refusal") => bail!("Claude declined this request (stop_reason: refusal). Rephrase the instruction or try the CLI backend."),
        Some("max_tokens") => ev(
            sink,
            call.job_id,
            "status",
            "Warning: the response hit max_tokens and may be truncated",
        ),
        _ => {}
    }
    Ok(ClaudeResult {
        text,
        backend: "api".into(),
        model,
        duration_ms: started.elapsed().as_millis() as u64,
        cost_usd: None,
        stop_reason,
        cancelled: false,
        cache_read_tokens,
        cache_write_tokens,
        ..Default::default()
    })
}

pub async fn run(
    settings: &Settings,
    call: ClaudeCall<'_>,
    sink: &Sink,
    cancel: &CancellationToken,
    http: &reqwest::Client,
) -> Result<ClaudeResult> {
    let r = match settings.claude_backend {
        ClaudeBackend::Cli => run_cli(settings, &call, sink, cancel).await,
        ClaudeBackend::Api => run_api(settings, &call, sink, cancel, http).await,
    };
    match &r {
        Ok(res) => ev(sink, call.job_id, if res.cancelled { "cancelled" } else { "done" }, ""),
        Err(e) => ev(sink, call.job_id, "error", e.to_string()),
    }
    r
}

/// Quick connectivity test for Settings → "Test".
pub async fn test_backend(settings: &Settings, sink: &Sink, http: &reqwest::Client) -> Result<String> {
    let call = ClaudeCall {
        job_id: "test",
        system: None,
        prompt: "Reply with exactly: OK".into(),
        cwd: None,
        allow_read_tools: false,
    };
    let mut s = settings.clone();
    if s.claude_effort.is_empty() || s.claude_backend == ClaudeBackend::Api {
        s.claude_effort = "low".into();
    }
    let r = run(&s, call, sink, &CancellationToken::new(), http).await?;
    Ok(format!(
        "{} replied \"{}\" in {:.1}s{}",
        if r.backend == "cli" { "Claude CLI" } else { "Anthropic API" },
        r.text.trim().chars().take(40).collect::<String>(),
        r.duration_ms as f64 / 1000.0,
        r.cost_usd.map(|c| format!(" (${c:.4})")).unwrap_or_default()
    ))
}

// ---------------------------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------------------------

/// Budget guidance for the expansion prompt, derived from the active profile.
pub fn budget_guidance(profile: &Profile, context_file_tokens: u32) -> String {
    let ctx = profile.context_limit;
    let spec_target = (ctx / 10).clamp(1500, 6000);
    let per_call_files = ctx.saturating_sub(profile.max_output_tokens + profile.reasoning.reasoning_reserve + spec_target + 1500);
    let mut g = format!(
        "The implementing model has a {ctx}-token context window. Each call contains its instructions, the spec, the full current content of the files it edits, and room for up to {} output tokens. That leaves roughly {per_call_files} tokens for file contents per call.\n",
        profile.max_output_tokens
    );
    g.push_str(&format!(
        "- Keep the whole spec under about {spec_target} tokens; be precise rather than long.\n"
    ));
    if context_file_tokens > per_call_files / 2 || ctx <= 32768 {
        g.push_str("- The job may be split into one call per file. In that case each call sees only the Summary, the Design, its own `### path` subsection under Changes, Edge cases and Acceptance criteria, plus that single file. Put every cross-file name and signature in the Design so all calls agree on them, and make each file's subsection self-contained otherwise.\n");
        g.push_str("- Prefer touching few files. Avoid sweeping changes to very large files; target the specific functions.\n");
    }
    if context_file_tokens > per_call_files {
        g.push_str(&format!("- The selected files total about {context_file_tokens} tokens, more than fits in one call. Splitting per file is expected; keep each file's edits localized.\n"));
    }
    g
}

/// How the spec writer can look at the project beyond what the prompt contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Explore {
    /// Only the prompt.
    None,
    /// The Claude CLI's Read/Glob/Grep tools.
    ClaudeTools,
    /// The local planner's read_file/list_files/search tools.
    LocalTools,
}

pub struct ExpandInputs<'a> {
    pub instruction: &'a str,
    pub root: &'a Path,
    pub context_files: &'a [String],
    /// The implementing (coder) profile: its context limit shapes the spec.
    pub profile: &'a Profile,
    pub explore: Explore,
    /// Tokens of source overview (small files in full, outlines of the rest) to add; 0 = none.
    pub outline_budget: u32,
}

/// Max characters of ticked-file content to inline in the expansion prompt.
const MAX_CONTEXT_CHARS: usize = 400_000;

/// The expansion prompt, split for Claude's prompt cache: `system` holds everything before the
/// instruction (role, project tree, selected files, budget, output rules), which stays the same
/// across expansions of a project; `prompt` holds the instruction section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandPrompt {
    pub system: Option<String>,
    pub prompt: String,
}

const INSTRUCTION_SENTINEL: &str = "\u{1}RELAY_INSTRUCTION\u{1}";

/// Split a rendered template at the instruction placeholder. The heading line right above the
/// placeholder (e.g. `# Instruction`) goes with the instruction. Without the placeholder, or with
/// nothing before it, everything is sent as the prompt.
pub fn split_at_instruction(rendered: &str, instruction: &str) -> ExpandPrompt {
    let Some(pos) = rendered.find(INSTRUCTION_SENTINEL) else {
        return ExpandPrompt {
            system: None,
            prompt: rendered.to_string(),
        };
    };
    let line_start = rendered[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let above = rendered[..line_start].trim_end_matches(['\n', '\r', ' ']);
    let heading_start = above.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let split = if above[heading_start..].starts_with('#') {
        heading_start
    } else {
        line_start
    };
    let system = rendered[..split].trim_end().to_string();
    let prompt = rendered[split..].replace(INSTRUCTION_SENTINEL, instruction).trim().to_string();
    ExpandPrompt {
        system: (!system.trim().is_empty()).then_some(system),
        prompt,
    }
}

pub fn build_expand_prompt(inp: &ExpandInputs<'_>) -> Result<(ExpandPrompt, Vec<String>)> {
    build_expand_prompt_from(&templates::load(Template::Expand), inp)
}

pub fn build_expand_prompt_from(template: &str, inp: &ExpandInputs<'_>) -> Result<(ExpandPrompt, Vec<String>)> {
    // Templates edited on Windows (or checked out with core.autocrlf) use CRLF; normalize so the
    // split and the cached system part don't depend on the editor.
    let template = &template.replace("\r\n", "\n");
    let mut warnings = Vec::new();
    let list = list_files(inp.root, 3000)?;
    let tree = tree_text(&list, 600);
    let mut ctx = String::new();
    let mut ctx_tokens = 0u32;
    let mut total_chars = 0usize;
    if !inp.context_files.is_empty() {
        ctx.push_str("# Selected files (current content)\n");
        for f in inp.context_files {
            match read_text(inp.root, f) {
                Ok(Some(c)) => {
                    if total_chars + c.len() > MAX_CONTEXT_CHARS {
                        warnings.push(format!(
                            "`{f}` was not inlined in the expansion prompt (selected files exceed {} KB).",
                            MAX_CONTEXT_CHARS / 1000
                        ));
                        continue;
                    }
                    total_chars += c.len();
                    ctx_tokens += estimate_tokens(&c);
                    ctx.push_str(&format!("\n## `{f}`\n{}\n", fenced_lang(&c, lang_for(f))));
                }
                Ok(None) => warnings.push(format!("`{f}` does not exist")),
                Err(e) => warnings.push(format!("`{f}`: {e}")),
            }
        }
    }
    let explore_note = match inp.explore {
        Explore::ClaudeTools => {
            "You may use the Read, Glob and Grep tools to inspect the project (read-only) before writing the spec. Read every file you plan to change so your instructions match the real code. Do not modify anything."
        }
        Explore::LocalTools => {
            "You can call the tools `read_file`, `list_files` and `search` to inspect the project (read-only) before writing the spec. Read every file you plan to change so the spec matches the real code; never guess what a file contains. When you have what you need, stop calling tools and write the spec."
        }
        Explore::None => "",
    };
    let outline = if inp.outline_budget > 0 {
        crate::planner::project_outline(inp.root, inp.outline_budget, inp.context_files)
    } else {
        String::new()
    };
    let detected = crate::verify::detect(inp.root);
    let verification_defaults = if detected.commands.is_empty() {
        "(none detected)".to_string()
    } else {
        detected.commands.iter().map(|c| c.spec_line()).collect::<Vec<_>>().join("\n")
    };
    let project_name = inp
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| inp.root.display().to_string());
    let ctx_limit = inp.profile.context_limit.to_string();
    let guidance = budget_guidance(inp.profile, ctx_tokens);
    let rendered = templates::render(
        template,
        &[
            ("instruction", INSTRUCTION_SENTINEL),
            ("model_name", &inp.profile.name),
            ("context_limit", &ctx_limit),
            ("project_name", &project_name),
            ("file_tree", &fenced_lang(&tree, "")),
            ("context_files", &ctx),
            ("project_outline", &outline),
            ("explore_note", explore_note),
            ("budget_guidance", &guidance),
            ("verification_defaults", &verification_defaults),
        ],
    );
    Ok((split_at_instruction(&rendered, inp.instruction.trim()), warnings))
}

/// Strip anything before the first top-level heading (models sometimes add a preamble) and a
/// wrapping ```markdown fence.
pub fn clean_spec(text: &str) -> String {
    let mut t = text.trim();
    if let Some(rest) = t.strip_prefix("```markdown").or_else(|| t.strip_prefix("```md")) {
        t = rest.trim_end().strip_suffix("```").unwrap_or(rest).trim();
    }
    if let Some(pos) = t.find("\n## ").map(|p| p + 1).or_else(|| t.starts_with("## ").then_some(0)) {
        let head = &t[..pos];
        // Keep a leading `# Title` line, drop chatty preamble.
        if !head.trim().is_empty() && !head.trim_start().starts_with('#') {
            return t[pos..].trim().to_string();
        }
    }
    t.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub verdict: String,
    pub passed: bool,
    pub report: String,
    pub follow_up_spec: Option<String>,
}

pub const FOLLOW_UP_MARKER: &str = "=== FOLLOW-UP SPEC ===";

pub fn parse_review(text: &str) -> Review {
    let (report, follow) = match text.find(FOLLOW_UP_MARKER) {
        Some(p) => (
            text[..p].trim().to_string(),
            Some(text[p + FOLLOW_UP_MARKER.len()..].trim().to_string()).filter(|s| !s.is_empty()),
        ),
        None => (text.trim().to_string(), None),
    };
    let verdict_line = report
        .lines()
        .skip_while(|l| !l.to_lowercase().contains("verdict"))
        .skip(1)
        .find(|l| !l.trim().is_empty())
        .or_else(|| report.lines().find(|l| l.contains("PASS") || l.contains("NEEDS_FIXES")))
        .unwrap_or("")
        .trim()
        .to_string();
    let passed = verdict_line.contains("PASS") && !verdict_line.contains("NEEDS_FIXES");
    Review {
        verdict: verdict_line,
        passed,
        report,
        follow_up_spec: follow,
    }
}

pub fn build_review_prompt(spec: &str, diff: &str, files_after: &[(String, String)], verification: &str) -> String {
    build_review_prompt_from(&templates::load(Template::Review), spec, diff, files_after, verification)
}

pub fn build_review_prompt_from(template: &str, spec: &str, diff: &str, files_after: &[(String, String)], verification: &str) -> String {
    let mut after = String::new();
    if !files_after.is_empty() {
        after.push_str("# Changed files after the edit (full content)\n");
        for (p, c) in files_after {
            after.push_str(&format!("\n## `{p}`\n{}\n", fenced_lang(c, lang_for(p))));
        }
    }
    templates::render(
        &template.replace("\r\n", "\n"),
        &[
            ("spec", spec.trim()),
            ("diff", &fenced_lang(diff, "diff")),
            ("files_after", &after),
            (
                "verification",
                if verification.trim().is_empty() {
                    "Not run."
                } else {
                    verification.trim()
                },
            ),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_profiles;

    #[test]
    fn cli_args_disable_or_restrict_tools() {
        let a = cli_args("claude-opus-5-5", "high", false);
        assert!(a.windows(2).any(|w| w[0] == "--tools" && w[1].is_empty()));
        assert!(a.windows(2).any(|w| w == ["--model", "claude-opus-5-5"]));
        let b = cli_args("", "", true);
        assert!(b.windows(2).any(|w| w == ["--tools", "Read,Glob,Grep"]));
        assert!(!b.contains(&"--model".to_string()));
    }

    #[test]
    fn parses_cli_stream_lines() {
        let (e, r) = parse_cli_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#,
        );
        assert_eq!(e, vec![("delta", "hi".to_string())]);
        assert!(r.is_none());
        let (e, _) = parse_cli_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/a.ts"}}]}}"#,
        );
        assert_eq!(e[0], ("tool", "Read src/a.ts".to_string()));
        let (_, r) = parse_cli_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"SPEC","total_cost_usd":0.02}"#);
        assert_eq!(r.unwrap()["result"], "SPEC");
        assert!(parse_cli_line("not json").0.is_empty());
    }

    #[test]
    fn api_body_uses_adaptive_thinking_effort_and_fallbacks() {
        let b = api_body("", "", None, "x");
        assert_eq!(b["model"], "claude-opus-5-5");
        assert!(b.get("system").is_none());
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["output_config"]["effort"], "high");
        assert_eq!(b["fallbacks"], "default");
        assert!(b.get("temperature").is_none());
    }

    #[test]
    fn api_body_caches_the_system_part() {
        let b = api_body("", "", Some("STABLE"), "INSTRUCTION");
        assert_eq!(b["system"][0]["text"], "STABLE");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(b.get("cache_control").is_none(), "no automatic breakpoint after the instruction");
        assert_eq!(b["messages"][0]["content"], "INSTRUCTION");
    }

    #[test]
    fn split_keeps_the_heading_with_the_instruction() {
        let r = format!("Role\n\n# Project\ntree\n\n# Instruction\n{INSTRUCTION_SENTINEL}\n");
        let p = split_at_instruction(&r, "add retries");
        assert_eq!(p.system.as_deref(), Some("Role\n\n# Project\ntree"));
        assert_eq!(p.prompt, "# Instruction\nadd retries");
        // Placeholder inline in a paragraph: split at its line.
        let r = format!("Role\n\nDo this: {INSTRUCTION_SENTINEL}\nThen stop.");
        let p = split_at_instruction(&r, "X");
        assert_eq!(p.system.as_deref(), Some("Role"));
        assert_eq!(p.prompt, "Do this: X\nThen stop.");
        // No placeholder (edited template) or placeholder first: one prompt, no system part.
        let p = split_at_instruction("Just text", "X");
        assert_eq!((p.system, p.prompt.as_str()), (None, "Just text"));
        let p = split_at_instruction(&format!("# Instruction\n{INSTRUCTION_SENTINEL}\nrules"), "X");
        assert_eq!((p.system, p.prompt.as_str()), (None, "# Instruction\nX\nrules"));
    }

    #[test]
    fn expand_prompt_puts_stable_context_in_the_system_part() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.py"), "def a():\n    return 1\n").unwrap();
        let profile = &builtin_profiles()[0];
        let build = |instruction: &str| {
            build_expand_prompt_from(
                Template::Expand.default_text(),
                &ExpandInputs {
                    instruction,
                    root: d.path(),
                    context_files: &["a.py".to_string()],
                    profile,
                    explore: Explore::None,
                    outline_budget: 0,
                },
            )
            .unwrap()
            .0
        };
        let (one, two) = (build("add retries"), build("rename a"));
        let sys = one.system.clone().expect("system part");
        assert!(sys.contains("a.py") && sys.contains("def a():") && sys.contains("## Acceptance criteria"));
        assert!(!sys.contains("add retries"));
        assert_eq!(one.system, two.system, "the cacheable part must not depend on the instruction");
        assert_eq!(one.prompt, "# Instruction\nadd retries");
        // A CRLF template (Windows editor, core.autocrlf) splits the same way.
        let crlf = Template::Expand.default_text().replace("\r\n", "\n").replace('\n', "\r\n");
        let three = build_expand_prompt_from(
            &crlf,
            &ExpandInputs {
                instruction: "add retries",
                root: d.path(),
                context_files: &["a.py".to_string()],
                profile,
                explore: Explore::None,
                outline_budget: 0,
            },
        )
        .unwrap()
        .0;
        assert_eq!(three, one);
    }

    #[test]
    fn expand_prompt_lists_detected_commands_and_the_source_overview() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        let (p, _) = build_expand_prompt_from(
            Template::Expand.default_text(),
            &ExpandInputs {
                instruction: "greet",
                root: d.path(),
                context_files: &[],
                profile: &builtin_profiles()[1],
                explore: Explore::LocalTools,
                outline_budget: 4000,
            },
        )
        .unwrap();
        let sys = p.system.unwrap();
        assert!(sys.contains("## Verification") && sys.contains("- build: `cargo build`"), "{sys}");
        assert!(
            sys.contains("# Source overview") && sys.contains("println!(\"hi\");"),
            "small files are inlined in full"
        );
        assert!(sys.contains("`read_file`"), "the local tools are explained");
        assert!(!sys.contains("{{"), "every placeholder is filled");
    }

    #[test]
    fn review_prompt_includes_verification() {
        // A CRLF copy (Windows checkout or editor) renders the same.
        let t = Template::Review.default_text().replace("\r\n", "\n").replace('\n', "\r\n");
        let r = build_review_prompt_from(&t, "## Summary\nS", "+a", &[], "- build `cargo build` failed (exit code 101)");
        assert!(r.contains("# Verification results\n- build `cargo build` failed"), "{r}");
        assert!(build_review_prompt_from(&t, "S", "+a", &[], "").contains("# Verification results\nNot run."));
    }

    #[test]
    fn guidance_mentions_split_for_small_context() {
        let g = budget_guidance(&builtin_profiles()[0], 1000);
        assert!(g.contains("32768-token context"));
        assert!(g.contains("one call per file"));
        let big = budget_guidance(&builtin_profiles()[1], 1000);
        assert!(!big.contains("one call per file"));
    }

    #[test]
    fn clean_spec_strips_preamble_and_fence() {
        assert_eq!(clean_spec("Sure! Here's the spec:\n\n## Summary\nX"), "## Summary\nX");
        assert_eq!(clean_spec("```markdown\n## Summary\nX\n```"), "## Summary\nX");
        assert_eq!(clean_spec("# Title\n\n## Summary\nX"), "# Title\n\n## Summary\nX");
    }

    #[test]
    fn review_parsing() {
        let r = parse_review("## Verdict\nPASS — all good.\n\n## Problems\nNone.");
        assert!(r.passed);
        assert!(r.follow_up_spec.is_none());
        let r = parse_review(
            "## Verdict\nNEEDS_FIXES — missing retry cap.\n## Problems\n- x\n\n=== FOLLOW-UP SPEC ===\n## Summary\nFix it\n## Files\n- `a.ts` (modify) — cap",
        );
        assert!(!r.passed);
        assert!(r.follow_up_spec.unwrap().starts_with("## Summary"));
    }
}
