//! Event sink abstraction so the backend can run inside Tauri (emits to the webview) or headless
//! (the end-to-end test binary prints events).

use serde::Serialize;
use std::sync::Arc;

pub trait EventSink: Send + Sync {
    fn emit_json(&self, event: &str, payload: serde_json::Value);
}

pub type Sink = Arc<dyn EventSink>;

pub fn emit<T: Serialize>(sink: &Sink, event: &str, payload: &T) {
    if let Ok(v) = serde_json::to_value(payload) {
        sink.emit_json(event, v);
    }
}

/// Discards everything (tests).
pub struct NullSink;
impl EventSink for NullSink {
    fn emit_json(&self, _event: &str, _payload: serde_json::Value) {}
}

/// Prints selected events to stdout (headless e2e runs).
pub struct PrintSink {
    pub verbose_deltas: bool,
}
impl EventSink for PrintSink {
    fn emit_json(&self, event: &str, payload: serde_json::Value) {
        match event {
            "server-log" => {}
            "run-event" | "claude-event" => {
                let kind = payload.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                if kind == "delta" || kind == "reasoningDelta" {
                    if self.verbose_deltas {
                        print!("{}", payload.get("text").and_then(|t| t.as_str()).unwrap_or(""));
                    }
                } else if kind != "stats" {
                    println!("[{event}] {payload}");
                }
            }
            _ => println!("[{event}] {payload}"),
        }
    }
}
