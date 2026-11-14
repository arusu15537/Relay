//! Prompt prewarming for llama-server: evaluate a prompt prefix into the server's cache before the
//! run needs it, so the run only processes what follows (the spec, or the next file).
//!
//! The prefix is cut at the token level so it is an exact prefix of the real request:
//! `/apply-template` renders the real messages, `/tokenize` turns the whole prompt and the prefix
//! into tokens, and the longest common token prefix goes to `/completion` with `n_predict: 0`
//! ("the prompt is evaluated into the cache"). The real request then extends the cached tokens,
//! which every model type can reuse — also hybrid (recurrent) and sliding-window models, which
//! cannot go back to an arbitrary earlier position, only to the end of what was processed or to a
//! checkpoint (llama-server leaves one at the end of each request's prompt).

use crate::config::Profile;
use crate::llm::{Endpoint, build_body};
use crate::pipeline::PrimePoint;
use crate::tokens::root_url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PrimeOutcome {
    pub label: String,
    /// Tokens of the primed prefix.
    pub tokens: u32,
    /// Of those, already in the server's cache before this request.
    pub cached: Option<u32>,
    /// Processed by this request.
    pub processed: Option<u32>,
    pub ms: u64,
}

#[derive(Debug)]
pub enum PrimeError {
    /// The endpoint has no `/apply-template`, `/tokenize` or `/completion` (not llama-server).
    Unsupported,
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for PrimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrimeError::Unsupported => write!(f, "the server cannot prewarm prompts"),
            PrimeError::Cancelled => write!(f, "cancelled"),
            PrimeError::Failed(m) => write!(f, "{m}"),
        }
    }
}

const SHORT: Duration = Duration::from_secs(15);

async fn post(
    http: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    body: &Value,
    timeout: Option<Duration>,
    cancel: &CancellationToken,
) -> Result<Value, PrimeError> {
    let mut req = http.post(url).json(body);
    if let Some(t) = timeout {
        req = req.timeout(t);
    }
    if let Some(k) = api_key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let resp = tokio::select! {
        r = req.send() => r.map_err(|e| PrimeError::Failed(format!("{url}: {e}")))?,
        _ = cancel.cancelled() => return Err(PrimeError::Cancelled),
    };
    match resp.status().as_u16() {
        200 => {}
        404 | 405 | 501 => return Err(PrimeError::Unsupported),
        s => {
            let text = resp.text().await.unwrap_or_default();
            return Err(PrimeError::Failed(format!("{url}: HTTP {s}: {}", text.chars().take(300).collect::<String>())));
        }
    }
    tokio::select! {
        v = resp.json::<Value>() => v.map_err(|e| PrimeError::Failed(format!("{url}: {e}"))),
        _ = cancel.cancelled() => Err(PrimeError::Cancelled),
    }
}

fn token_ids(v: &Value) -> Option<Vec<Value>> {
    v.get("tokens")?
        .as_array()?
        .iter()
        .map(|t| if t.is_object() { t.get("id").cloned() } else { Some(t.clone()) })
        .collect()
}

/// Length of the longest common prefix of two token sequences.
pub fn common_prefix(a: &[Value], b: &[Value]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Evaluate `point`'s prefix into the server's cache. `stream_http` must not have an overall
/// timeout: processing a long prefix can take minutes on a slow profile.
pub async fn prime(
    http: &reqwest::Client,
    stream_http: &reqwest::Client,
    endpoint: &Endpoint,
    profile: &Profile,
    point: &PrimePoint,
    cancel: &CancellationToken,
) -> Result<PrimeOutcome, PrimeError> {
    let started = Instant::now();
    let root = root_url(&endpoint.base_url);
    let key = endpoint.api_key.as_deref();
    let last = point
        .messages
        .last()
        .ok_or_else(|| PrimeError::Failed("nothing to prime".into()))?;

    // The prompt exactly as the chat request will render it (same reasoning switches).
    let mut body = build_body(profile, &endpoint.model, &point.messages, 1);
    if let Some(o) = body.as_object_mut() {
        o.remove("stream");
        o.remove("stream_options");
    }
    let rendered = post(http, &format!("{root}/apply-template"), key, &body, Some(SHORT), cancel).await?;
    let prompt = rendered["prompt"]
        .as_str()
        .ok_or(PrimeError::Unsupported)?
        .to_string();
    let at = prompt
        .rfind(&last.content)
        .ok_or_else(|| PrimeError::Failed("the chat template changes the message text; cannot find the prefix".into()))?;
    let prefix = &prompt[..at + point.cut.min(last.content.len())];

    // Tokenize as the server does for a chat request (special tokens added and parsed).
    let tok = |content: &str| json!({ "content": content, "add_special": true, "parse_special": true });
    let url = format!("{root}/tokenize");
    let full = post(http, &url, key, &tok(&prompt), Some(SHORT), cancel).await?;
    let pre = post(http, &url, key, &tok(prefix), Some(SHORT), cancel).await?;
    let (full, pre) = match (token_ids(&full), token_ids(&pre)) {
        (Some(f), Some(p)) => (f, p),
        _ => return Err(PrimeError::Unsupported),
    };
    // Tokens can merge across the cut; keep only what the real request will start with.
    let n = common_prefix(&full, &pre).min(full.len().saturating_sub(1));
    if n == 0 {
        return Err(PrimeError::Failed("empty prefix".into()));
    }

    let out = post(
        stream_http,
        &format!("{root}/completion"),
        key,
        &json!({ "prompt": &full[..n], "n_predict": 0, "cache_prompt": true, "stream": false }),
        None,
        cancel,
    )
    .await?;
    let num = |v: &Value| v.as_u64().map(|x| x as u32);
    Ok(PrimeOutcome {
        label: point.label.to_string(),
        tokens: n as u32,
        cached: num(&out["timings"]["cache_n"]).or_else(|| num(&out["tokens_cached"])),
        processed: num(&out["timings"]["prompt_n"]),
        ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_ids_accept_plain_and_piece_forms() {
        assert_eq!(token_ids(&json!({"tokens": [1, 2]})), Some(vec![json!(1), json!(2)]));
        assert_eq!(
            token_ids(&json!({"tokens": [{"id": 7, "piece": "a"}, {"id": 8, "piece": [195]}]})),
            Some(vec![json!(7), json!(8)])
        );
        assert_eq!(token_ids(&json!({"x": 1})), None);
    }

    #[test]
    fn common_prefix_counts_matching_tokens() {
        let a = [json!(1), json!(2), json!(3)];
        assert_eq!(common_prefix(&a, &[json!(1), json!(2), json!(9)]), 2);
        assert_eq!(common_prefix(&a, &a[..1]), 1);
        assert_eq!(common_prefix(&a, &[]), 0);
    }
}
