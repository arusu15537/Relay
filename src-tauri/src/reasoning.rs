//! Per-profile reasoning suppression (request parameters) and reasoning stripping (output).
//!
//! * Qwen3.6 / Qwen3.8: thinking is disabled with `chat_template_kwargs.enable_thinking=false`
//!   (the official switch; the `/no_think` soft switch is not supported by these models).
//! * gpt-oss: thinking cannot be disabled, so `reasoning_effort` is set to `low` both as a
//!   top-level request field and as a chat-template kwarg, and the reasoning is stripped.
//!
//! Stripping handles `<think>…</think>` (including an unterminated or orphan-closed block) and
//! raw gpt-oss "harmony" channel markup in case a server returns it in `content`.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    /// Send `chat_template_kwargs: {"enable_thinking": false}` (Qwen3.x).
    #[default]
    DisableThinking,
    /// Send `reasoning_effort: <effort>` (gpt-oss, or Qwen3.8 with thinking on).
    Effort,
    /// Send nothing reasoning-related.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReasoningControl {
    pub mode: ReasoningMode,
    /// Effort level for `ReasoningMode::Effort` (e.g. "low").
    pub effort: String,
    /// Strip `<think>…</think>` from content.
    pub strip_think_tags: bool,
    /// Strip gpt-oss harmony channel markup from content.
    pub strip_harmony: bool,
    /// Tokens to reserve in the context budget for reasoning output.
    pub reasoning_reserve: u32,
}

impl Default for ReasoningControl {
    fn default() -> Self {
        Self {
            mode: ReasoningMode::DisableThinking,
            effort: "low".into(),
            strip_think_tags: true,
            strip_harmony: true,
            reasoning_reserve: 0,
        }
    }
}

impl ReasoningControl {
    pub fn qwen() -> Self {
        Self {
            mode: ReasoningMode::DisableThinking,
            effort: String::new(),
            strip_think_tags: true,
            strip_harmony: false,
            reasoning_reserve: 0,
        }
    }
    pub fn gpt_oss() -> Self {
        Self {
            mode: ReasoningMode::Effort,
            effort: "low".into(),
            strip_think_tags: true,
            strip_harmony: true,
            reasoning_reserve: 2048,
        }
    }
}

fn merge_kwargs(body: &mut Value, key: &str, val: Value) {
    let obj = body.as_object_mut().expect("request body must be an object");
    let kw = obj.entry("chat_template_kwargs").or_insert_with(|| json!({}));
    if !kw.is_object() {
        *kw = json!({});
    }
    kw.as_object_mut().unwrap().insert(key.to_string(), val);
}

/// Add the reasoning-control fields for this profile to an OpenAI-style chat request body
/// (the coder role: as little reasoning as the model allows).
pub fn apply_request_params(ctrl: &ReasoningControl, body: &mut Value) {
    match ctrl.mode {
        ReasoningMode::DisableThinking => {
            merge_kwargs(body, "enable_thinking", json!(false));
        }
        ReasoningMode::Effort => {
            let effort = if ctrl.effort.trim().is_empty() { "low" } else { ctrl.effort.trim() };
            set_effort(body, effort);
        }
        ReasoningMode::None => {}
    }
}

fn set_effort(body: &mut Value, effort: &str) {
    body.as_object_mut().unwrap().insert("reasoning_effort".into(), json!(effort));
    merge_kwargs(body, "reasoning_effort", json!(effort));
}

/// Reasoning fields for the planner role, which thinks before it writes the spec or review:
/// Qwen3.x get `enable_thinking: true`, effort-controlled models (gpt-oss) get `effort`
/// (blank → "high"), and profiles that send nothing keep sending nothing.
pub fn apply_thinking_params(ctrl: &ReasoningControl, body: &mut Value, effort: &str) {
    match ctrl.mode {
        ReasoningMode::DisableThinking => merge_kwargs(body, "enable_thinking", json!(true)),
        ReasoningMode::Effort => set_effort(body, if effort.trim().is_empty() { "high" } else { effort.trim() }),
        ReasoningMode::None => {}
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stripped {
    pub content: String,
    pub reasoning: String,
}

static THINK_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<think>(.*?)</think>").unwrap());
static HARMONY_TOKENS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<\|(?:start|end|return|call|message|channel|constrain)\|>(?:assistant)?").unwrap());

/// Split raw harmony markup into `(channel, body)` segments plus the text outside any segment.
/// A segment is `<|channel|>NAME[ to=..][<|constrain|>..]<|message|>BODY` where BODY runs to the
/// first `<|end|>`, `<|return|>`, `<|call|>` or `<|start|>` (or the end of the text).
fn split_harmony(text: &str) -> (Vec<(String, String)>, String) {
    let mut segs = Vec::new();
    let mut leftover = String::new();
    let mut parts = text.split("<|channel|>");
    if let Some(first) = parts.next() {
        leftover.push_str(first);
    }
    for part in parts {
        let Some(mpos) = part.find("<|message|>") else {
            leftover.push_str(part);
            continue;
        };
        let name = part[..mpos]
            .trim()
            .split(|c: char| c.is_whitespace() || c == '<')
            .next()
            .unwrap_or("")
            .to_string();
        let after = &part[mpos + "<|message|>".len()..];
        let end = ["<|end|>", "<|return|>", "<|call|>", "<|start|>"]
            .iter()
            .filter_map(|t| after.find(t))
            .min()
            .unwrap_or(after.len());
        segs.push((name, after[..end].to_string()));
        leftover.push_str(&after[end..]);
    }
    (segs, HARMONY_TOKENS.replace_all(&leftover, "").to_string())
}

/// Remove reasoning from model content. Returns the cleaned content and the extracted reasoning.
pub fn strip_reasoning(ctrl: &ReasoningControl, text: &str) -> Stripped {
    let mut content = text.to_string();
    let mut reasoning = Vec::<String>::new();

    if ctrl.strip_harmony && content.contains("<|channel|>") {
        let (segs, leftover) = split_harmony(&content);
        let mut finals = Vec::new();
        for (channel, body) in segs {
            if channel == "final" {
                finals.push(body);
            } else {
                reasoning.push(body.trim().to_string());
            }
        }
        // No final channel: keep whatever text was outside the reasoning segments.
        content = if finals.is_empty() { leftover } else { finals.join("\n") };
    }

    if ctrl.strip_think_tags {
        for cap in THINK_BLOCK.captures_iter(&content) {
            reasoning.push(cap[1].trim().to_string());
        }
        content = THINK_BLOCK.replace_all(&content, "").to_string();
        let trimmed = content.trim_start();
        if let Some(rest) = trimmed.strip_prefix("<think>") {
            // Unterminated block: the model never finished thinking; nothing usable follows.
            reasoning.push(rest.trim().to_string());
            content = String::new();
        } else if let Some(pos) = content.find("</think>") {
            // Orphan closing tag (the template opened <think> in the prompt). Only treat it as a
            // reasoning boundary if it occurs before any edit block.
            let first_block = content.find("<<<<<<<").unwrap_or(usize::MAX);
            if pos < first_block {
                reasoning.push(content[..pos].trim().to_string());
                content = content[pos + "</think>".len()..].to_string();
            }
        }
    }

    Stripped {
        content: content.trim_start_matches(['\n', '\r']).to_string(),
        reasoning: reasoning.into_iter().filter(|r| !r.is_empty()).collect::<Vec<_>>().join("\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_request_disables_thinking() {
        let mut body = json!({"model": "x", "messages": []});
        apply_request_params(&ReasoningControl::qwen(), &mut body);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(false));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn qwen_merges_existing_kwargs() {
        let mut body = json!({"chat_template_kwargs": {"preserve_thinking": false}});
        apply_request_params(&ReasoningControl::qwen(), &mut body);
        assert_eq!(body["chat_template_kwargs"]["preserve_thinking"], json!(false));
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(false));
    }

    #[test]
    fn gpt_oss_request_sets_low_effort_twice() {
        let mut body = json!({});
        apply_request_params(&ReasoningControl::gpt_oss(), &mut body);
        assert_eq!(body["reasoning_effort"], json!("low"));
        assert_eq!(body["chat_template_kwargs"]["reasoning_effort"], json!("low"));
        assert!(body["chat_template_kwargs"].get("enable_thinking").is_none());
    }

    #[test]
    fn effort_defaults_to_low_when_blank() {
        let mut c = ReasoningControl::gpt_oss();
        c.effort = " ".into();
        let mut body = json!({});
        apply_request_params(&c, &mut body);
        assert_eq!(body["reasoning_effort"], json!("low"));
    }

    #[test]
    fn planner_role_turns_thinking_on() {
        let mut body = json!({"chat_template_kwargs": {"preserve_thinking": false}});
        apply_thinking_params(&ReasoningControl::qwen(), &mut body, "high");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(true));
        assert_eq!(body["chat_template_kwargs"]["preserve_thinking"], json!(false));
        assert!(body.get("reasoning_effort").is_none(), "Qwen has no effort switch");

        let mut body = json!({});
        apply_thinking_params(&ReasoningControl::gpt_oss(), &mut body, "medium");
        assert_eq!(body["reasoning_effort"], json!("medium"));
        assert_eq!(body["chat_template_kwargs"]["reasoning_effort"], json!("medium"));
        let mut body = json!({});
        apply_thinking_params(&ReasoningControl::gpt_oss(), &mut body, " ");
        assert_eq!(body["reasoning_effort"], json!("high"), "blank planner effort means high");

        let mut none = ReasoningControl::qwen();
        none.mode = ReasoningMode::None;
        let mut body = json!({"a": 1});
        apply_thinking_params(&none, &mut body, "high");
        assert_eq!(body, json!({"a": 1}));
    }

    #[test]
    fn none_mode_sends_nothing() {
        let mut c = ReasoningControl::qwen();
        c.mode = ReasoningMode::None;
        let mut body = json!({"a": 1});
        apply_request_params(&c, &mut body);
        assert_eq!(body, json!({"a": 1}));
    }

    #[test]
    fn strips_think_block() {
        let s = strip_reasoning(&ReasoningControl::qwen(), "<think>\nplan it\n</think>\n\na.txt\n<<<<<<< SEARCH");
        assert_eq!(s.reasoning, "plan it");
        assert_eq!(s.content, "a.txt\n<<<<<<< SEARCH");
    }

    #[test]
    fn strips_empty_think_block_from_disabled_template() {
        let s = strip_reasoning(&ReasoningControl::qwen(), "<think>\n\n</think>\n\nhello");
        assert_eq!(s.content, "hello");
        assert_eq!(s.reasoning, "");
    }

    #[test]
    fn unterminated_think_means_no_content() {
        let s = strip_reasoning(&ReasoningControl::qwen(), "<think>I was cut off while thinking");
        assert_eq!(s.content, "");
        assert_eq!(s.reasoning, "I was cut off while thinking");
    }

    #[test]
    fn orphan_close_tag_before_blocks() {
        let s = strip_reasoning(&ReasoningControl::qwen(), "some thoughts\n</think>\na.txt\n<<<<<<< SEARCH\n");
        assert_eq!(s.reasoning, "some thoughts");
        assert!(s.content.starts_with("a.txt"));
    }

    #[test]
    fn close_tag_inside_code_is_kept() {
        let text = "a.html\n<<<<<<< SEARCH\n=======\n<p></think></p>\n>>>>>>> REPLACE\n";
        let s = strip_reasoning(&ReasoningControl::qwen(), text);
        assert_eq!(s.content, text);
    }

    #[test]
    fn strips_harmony_channels() {
        let raw = "<|channel|>analysis<|message|>Need to edit a.txt.<|end|><|start|>assistant<|channel|>final<|message|>a.txt\n<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE<|return|>";
        let s = strip_reasoning(&ReasoningControl::gpt_oss(), raw);
        assert_eq!(s.reasoning, "Need to edit a.txt.");
        assert_eq!(s.content, "a.txt\n<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE");
    }

    #[test]
    fn harmony_without_final_drops_analysis() {
        let raw = "<|channel|>analysis<|message|>thinking only<|end|>leftover";
        let s = strip_reasoning(&ReasoningControl::gpt_oss(), raw);
        assert_eq!(s.content, "leftover");
        assert_eq!(s.reasoning, "thinking only");
    }

    #[test]
    fn clean_text_is_untouched() {
        let s = strip_reasoning(&ReasoningControl::gpt_oss(), "a.txt\n<<<<<<< SEARCH\n");
        assert_eq!(s.content, "a.txt\n<<<<<<< SEARCH\n");
        assert!(s.reasoning.is_empty());
    }

    #[test]
    fn qwen_profile_does_not_touch_harmony_like_text() {
        let raw = "<|channel|>analysis<|message|>x";
        let s = strip_reasoning(&ReasoningControl::qwen(), raw);
        assert_eq!(s.content, raw);
    }
}
