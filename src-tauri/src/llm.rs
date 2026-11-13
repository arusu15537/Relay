//! Streaming client for OpenAI-compatible chat completions (llama-server, LM Studio, Ollama).

use crate::config::Profile;
use crate::reasoning::apply_request_params;
use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(c: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: c.into(),
        }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: c.into(),
        }
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: c.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Base URL including `/v1`.
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatOutcome {
    pub content: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    /// Generation speed: server-reported (llama-server `timings`) when available, else measured.
    pub tok_per_sec: Option<f64>,
    pub tok_per_sec_source: String,
    pub prompt_per_sec: Option<f64>,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveStats {
    pub tokens: u32,
    pub tok_per_sec: f64,
    pub elapsed_ms: u64,
}

pub enum Delta<'a> {
    Content(&'a str),
    Reasoning(&'a str),
}

/// Build the request body for a profile: sampling defaults + reasoning control.
pub fn build_body(profile: &Profile, model: &str, messages: &[ChatMessage], max_tokens: u32) -> Value {
    let s = &profile.sampling;
    let mut body = json!({
        "model": if model.is_empty() { "local" } else { model },
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
        "max_tokens": max_tokens,
        "temperature": s.temperature,
        "top_p": s.top_p,
        "min_p": s.min_p,
        "presence_penalty": s.presence_penalty,
    });
    if s.top_k > 0 {
        body["top_k"] = json!(s.top_k);
    }
    if (s.repeat_penalty - 1.0).abs() > f32::EPSILON {
        body["repeat_penalty"] = json!(s.repeat_penalty);
    }
    apply_request_params(&profile.reasoning, &mut body);
    body
}

/// Incremental Server-Sent-Events parser: feed raw text, get complete `data:` payloads.
#[derive(Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(&chunk.replace("\r\n", "\n"));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let event: String = self.buf.drain(..pos + 2).collect();
            let data: Vec<&str> = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:").map(|d| d.strip_prefix(' ').unwrap_or(d)))
                .collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }

    /// Whatever is left when the stream ends without a trailing blank line.
    pub fn finish(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.buf);
        if rest.trim().is_empty() {
            return Vec::new();
        }
        self.feed(&format!("{rest}\n\n"))
    }
}

fn friendly_error(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let err = v.get("error").cloned().unwrap_or(Value::Null);
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .map(String::from)
        .unwrap_or_else(|| body.chars().take(500).collect());
    let typ = err.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if typ == "exceed_context_size_error" || msg.contains("exceeds the available context") {
        let n_prompt = err.get("n_prompt_tokens").and_then(|n| n.as_u64());
        let n_ctx = err.get("n_ctx").and_then(|n| n.as_u64());
        return format!(
            "The prompt does not fit in the model's context{}. Split the job per file or use a profile with a larger context.",
            match (n_prompt, n_ctx) {
                (Some(p), Some(c)) => format!(" ({p} prompt tokens > {c} context)"),
                _ => String::new(),
            }
        );
    }
    format!("HTTP {status}: {msg}")
}

/// Stream a chat completion. `on_delta` receives every content/reasoning piece plus live stats.
pub async fn stream_chat(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    body: &Value,
    cancel: &CancellationToken,
    on_delta: &mut (dyn FnMut(Delta<'_>, LiveStats) + Send),
) -> Result<ChatOutcome> {
    let url = format!("{}/chat/completions", endpoint.base_url.trim_end_matches('/'));
    let mut req = http.post(&url).json(body);
    if let Some(k) = endpoint.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let started = Instant::now();
    let resp = tokio::select! {
        r = req.send() => r.map_err(|e| anyhow!("Could not reach the local model at {url}: {e}"))?,
        _ = cancel.cancelled() => return Ok(ChatOutcome { cancelled: true, ..Default::default() }),
    };
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!(friendly_error(status.as_u16(), &text)));
    }

    let mut out = ChatOutcome::default();
    let mut parser = SseParser::default();
    let mut stream = resp.bytes_stream();
    let mut first_token: Option<Instant> = None;
    let mut tokens: u32 = 0;
    let mut pending = Vec::<u8>::new();
    let mut done = false;

    let mut handle_payload = |payload: &str, out: &mut ChatOutcome, first_token: &mut Option<Instant>, tokens: &mut u32| -> Result<bool> {
        if payload.trim() == "[DONE]" {
            return Ok(true);
        }
        let v: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return Ok(false),
        };
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("stream error");
            return Err(anyhow!("Local model error: {msg}"));
        }
        if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
            for ch in choices {
                let delta = ch.get("delta").cloned().unwrap_or(Value::Null);
                let reasoning = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                let content = delta.get("content").and_then(|c| c.as_str()).unwrap_or("");
                for (piece, is_reasoning) in [(reasoning, true), (content, false)] {
                    if piece.is_empty() {
                        continue;
                    }
                    let now = Instant::now();
                    let ft = *first_token.get_or_insert(now);
                    *tokens += 1;
                    let el = now.duration_since(ft).as_secs_f64();
                    let stats = LiveStats {
                        tokens: *tokens,
                        tok_per_sec: if el > 0.2 { (*tokens as f64 - 1.0) / el } else { 0.0 },
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    };
                    if is_reasoning {
                        out.reasoning.push_str(piece);
                        on_delta(Delta::Reasoning(piece), stats);
                    } else {
                        out.content.push_str(piece);
                        on_delta(Delta::Content(piece), stats);
                    }
                }
                if let Some(fr) = ch.get("finish_reason").and_then(|f| f.as_str()) {
                    out.finish_reason = Some(fr.to_string());
                }
            }
        }
        if let Some(u) = v.get("usage") {
            out.prompt_tokens = u.get("prompt_tokens").and_then(|n| n.as_u64()).map(|n| n as u32);
            out.completion_tokens = u.get("completion_tokens").and_then(|n| n.as_u64()).map(|n| n as u32);
        }
        if let Some(t) = v.get("timings") {
            if let Some(tps) = t.get("predicted_per_second").and_then(|n| n.as_f64()) {
                out.tok_per_sec = Some(tps);
                out.tok_per_sec_source = "server".into();
            }
            out.prompt_per_sec = t.get("prompt_per_second").and_then(|n| n.as_f64());
        }
        Ok(false)
    };

    while !done {
        let next = tokio::select! {
            n = stream.next() => n,
            _ = cancel.cancelled() => { out.cancelled = true; break; }
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| anyhow!("Connection to the local model dropped: {e}"))?;
        pending.extend_from_slice(&chunk);
        // Only decode complete UTF-8 sequences.
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(e) => e.valid_up_to(),
        };
        let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
        pending.drain(..valid);
        for payload in parser.feed(&text) {
            if handle_payload(&payload, &mut out, &mut first_token, &mut tokens)? {
                done = true;
                break;
            }
        }
    }
    if !out.cancelled {
        for payload in parser.finish() {
            handle_payload(&payload, &mut out, &mut first_token, &mut tokens)?;
        }
    }
    out.duration_ms = started.elapsed().as_millis() as u64;
    out.ttft_ms = first_token.map(|t| t.duration_since(started).as_millis() as u64);
    if out.tok_per_sec.is_none()
        && let Some(ft) = first_token
    {
        let el = ft.elapsed().as_secs_f64();
        let n = out.completion_tokens.unwrap_or(tokens) as f64;
        if el > 0.0 && n > 1.0 {
            out.tok_per_sec = Some((n - 1.0) / el);
            out.tok_per_sec_source = "measured".into();
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_profiles;

    #[test]
    fn sse_parser_handles_split_chunks() {
        let mut p = SseParser::default();
        assert!(p.feed("data: {\"a\":").is_empty());
        let got = p.feed("1}\n\ndata: [DONE]\n\n");
        assert_eq!(got, vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);
        assert!(p.feed(": keep-alive comment\n\n").is_empty());
        let got = p.feed("data: x\r\n\r\n");
        assert_eq!(got, vec!["x"]);
        p.feed("data: tail");
        assert_eq!(p.finish(), vec!["tail"]);
    }

    #[test]
    fn body_for_qwen_and_gpt_oss() {
        let profiles = builtin_profiles();
        let msgs = vec![ChatMessage::user("hi")];
        let q = build_body(&profiles[0], "", &msgs, 1000);
        assert_eq!(q["chat_template_kwargs"]["enable_thinking"], json!(false));
        assert_eq!(q["top_k"], json!(20));
        assert_eq!(q["max_tokens"], json!(1000));
        assert_eq!(q["model"], json!("local"));
        let g = build_body(&profiles[2], "gpt-oss:20b", &msgs, 1000);
        assert_eq!(g["reasoning_effort"], json!("low"));
        assert!(g.get("top_k").is_none(), "top_k 0 means disabled and is omitted");
        assert_eq!(g["model"], json!("gpt-oss:20b"));
    }

    #[test]
    fn context_overflow_error_is_friendly() {
        let body = r#"{"error":{"code":400,"message":"the request exceeds the available context size, try increasing it","type":"exceed_context_size_error","n_prompt_tokens":40000,"n_ctx":32768}}"#;
        let m = friendly_error(400, body);
        assert!(m.contains("40000 prompt tokens > 32768 context"), "{m}");
    }
}
