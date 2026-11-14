//! Exact token counts from the loaded model's tokenizer (llama-server `POST /tokenize`), cached
//! per text. The budget uses them instead of the length estimate when the endpoint offers them;
//! servers without `/tokenize` (LM Studio, Ollama) keep the estimate.

use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::Duration;

/// Identifies one tokenizer: the profile, the model file (or external model name) and the
/// endpoint serving it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenizerKey {
    pub id: String,
    /// Server root URL (without `/v1`).
    pub root_url: String,
    pub api_key: Option<String>,
}

impl TokenizerKey {
    pub fn new(profile_id: &str, model: &str, base_url: &str, api_key: Option<String>) -> Self {
        let root_url = root_url(base_url);
        Self {
            id: format!("{profile_id}|{model}|{root_url}"),
            root_url,
            api_key,
        }
    }
}

/// `http://host:port/v1` → `http://host:port` (llama-server serves `/tokenize` at the root).
pub fn root_url(base_url: &str) -> String {
    let b = base_url.trim().trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

const MAX_ENTRIES: usize = 8192;
const CONCURRENCY: usize = 8;
const TIMEOUT: Duration = Duration::from_secs(5);

fn text_key(id: &str, text: &str) -> (String, u64, usize) {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    (id.to_string(), h.finish(), text.len())
}

#[derive(Default)]
pub struct TokenCounter {
    counts: Mutex<HashMap<(String, u64, usize), u32>>,
    /// Endpoints that answered `/tokenize` with "not found" (no tokenizer endpoint).
    unsupported: Mutex<HashSet<String>>,
}

enum Outcome {
    Count(u32),
    Unsupported,
    Failed,
}

impl TokenCounter {
    pub fn cached(&self, key: &TokenizerKey, text: &str) -> Option<u32> {
        if text.is_empty() {
            return Some(0);
        }
        self.counts.lock().unwrap().get(&text_key(&key.id, text)).copied()
    }

    pub fn is_unsupported(&self, key: &TokenizerKey) -> bool {
        self.unsupported.lock().unwrap().contains(&key.root_url)
    }

    /// Tokenize the texts that are not cached yet. Returns false if the endpoint cannot
    /// tokenize or a request failed; callers then fall back to the estimate.
    pub async fn fill(&self, http: &reqwest::Client, key: &TokenizerKey, texts: Vec<String>) -> bool {
        if self.is_unsupported(key) {
            return false;
        }
        let mut todo: Vec<String> = texts.into_iter().filter(|t| self.cached(key, t).is_none()).collect();
        todo.sort();
        todo.dedup();
        if todo.is_empty() {
            return true;
        }
        let url = format!("{}/tokenize", key.root_url);
        let results: Vec<(String, Outcome)> = futures_util::stream::iter(todo)
            .map(|text| {
                let url = url.clone();
                async move {
                    let out = tokenize(http, &url, key.api_key.as_deref(), &text).await;
                    (text, out)
                }
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
        let mut ok = true;
        let mut counts = self.counts.lock().unwrap();
        if counts.len() + results.len() > MAX_ENTRIES {
            counts.clear();
        }
        for (text, out) in results {
            match out {
                Outcome::Count(n) => {
                    counts.insert(text_key(&key.id, &text), n);
                }
                Outcome::Unsupported => {
                    self.unsupported.lock().unwrap().insert(key.root_url.clone());
                    ok = false;
                }
                Outcome::Failed => ok = false,
            }
        }
        ok
    }
}

async fn tokenize(http: &reqwest::Client, url: &str, api_key: Option<&str>, text: &str) -> Outcome {
    let mut req = http.post(url).timeout(TIMEOUT).json(&json!({ "content": text }));
    if let Some(k) = api_key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let Ok(resp) = req.send().await else {
        return Outcome::Failed;
    };
    match resp.status().as_u16() {
        200 => {}
        404 | 405 | 501 => return Outcome::Unsupported,
        _ => return Outcome::Failed,
    }
    match resp.json::<Value>().await {
        Ok(v) => match v.get("tokens").and_then(|t| t.as_array()) {
            Some(t) => Outcome::Count(t.len() as u32),
            None => Outcome::Unsupported,
        },
        Err(_) => Outcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_url_strips_v1() {
        assert_eq!(root_url("http://127.0.0.1:39281/v1"), "http://127.0.0.1:39281");
        assert_eq!(root_url("http://localhost:1234/v1/"), "http://localhost:1234");
        assert_eq!(root_url("http://h:1"), "http://h:1");
    }

    #[test]
    fn keys_separate_models_and_endpoints() {
        let a = TokenizerKey::new("p", "a.gguf", "http://h:1/v1", None);
        let b = TokenizerKey::new("p", "b.gguf", "http://h:1/v1", None);
        assert_ne!(a.id, b.id);
        let c = TokenCounter::default();
        c.counts.lock().unwrap().insert(text_key(&a.id, "hello"), 2);
        assert_eq!(c.cached(&a, "hello"), Some(2));
        assert_eq!(c.cached(&b, "hello"), None, "another model's tokenizer");
        assert_eq!(c.cached(&b, ""), Some(0));
    }

    /// One-shot HTTP server answering every request with `status` and `body`.
    fn serve(status: u16, body: &'static str, requests: usize) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().take(requests).flatten() {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut b = vec![0; len];
                r.read_exact(&mut b).unwrap();
                let mut s = s;
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn fill_counts_and_caches() {
        let base = serve(200, r#"{"tokens":[1,2,3]}"#, 2);
        let key = TokenizerKey::new("p", "m", &base, None);
        let c = TokenCounter::default();
        let http = reqwest::Client::new();
        assert!(c.fill(&http, &key, vec!["a".into(), "b".into(), "a".into(), String::new()]).await);
        assert_eq!((c.cached(&key, "a"), c.cached(&key, "b")), (Some(3), Some(3)));
        // Everything cached: no request needed (the test server only answers two).
        assert!(c.fill(&http, &key, vec!["a".into(), "b".into()]).await);
    }

    #[tokio::test]
    async fn endpoints_without_tokenize_are_remembered() {
        let base = serve(404, r#"{"error":"not found"}"#, 1);
        let key = TokenizerKey::new("p", "m", &base, None);
        let c = TokenCounter::default();
        let http = reqwest::Client::new();
        assert!(!c.fill(&http, &key, vec!["x".into()]).await);
        assert!(c.is_unsupported(&key));
        assert!(!c.fill(&http, &key, vec!["y".into()]).await, "not asked again");
    }
}
