//! The local planner role: a local model with thinking on writes the spec (and later reviews the
//! verified change), optionally reading the project through read-only tools first.
//!
//! Tools (OpenAI function calling, served by llama-server's Jinja chat templates):
//! `read_file(path, start_line?, end_line?)`, `list_files(dir?)`, `search(pattern, path_glob?)`.
//! All of them resolve paths through `pathsafe` and never leave the given root.

use crate::config::Profile;
use crate::llm::{ChatMessage, Delta, Endpoint, LiveStats, ToolCall, build_planner_body, stream_chat};
use crate::outline;
use crate::project::{list_files, read_text, tree_text};
use crate::reasoning::strip_reasoning;
use anyhow::{Result, bail};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Tool rounds before the planner must answer.
pub const MAX_TOOL_ROUNDS: usize = 16;
/// Output tokens per planner call (thinking included). Qwen3.6 recommends 32k for most tasks.
pub const MAX_OUTPUT: u32 = 32768;
/// Stop offering tools once the conversation uses this share of the context.
const TOOL_CONTEXT_SHARE: f64 = 0.7;
/// Limits on what one tool call returns.
const READ_MAX_LINES: usize = 600;
const READ_MAX_CHARS: usize = 40_000;
const SEARCH_MAX_MATCHES: usize = 80;
const LIST_MAX_LINES: usize = 400;

pub fn tool_definitions() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a text file of the project. Returns up to 600 lines; use start_line/end_line (1-based, inclusive) for other parts of long files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the project root, with forward slashes." },
                        "start_line": { "type": "integer" },
                        "end_line": { "type": "integer" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List the project's files (or those under a directory) as an indented tree. Ignored files (.gitignore) are left out.",
                "parameters": {
                    "type": "object",
                    "properties": { "dir": { "type": "string", "description": "Directory relative to the project root; empty for the whole project." } }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search the project's text files for a regular expression (case-insensitive). Returns path:line: text for each match.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path_glob": { "type": "string", "description": "Optional glob limiting the files, e.g. src/**/*.rs" }
                    },
                    "required": ["pattern"]
                }
            }
        }
    ])
}

/// A short description of a tool call for the activity list ("read_file src/main.rs").
pub fn describe_call(call: &ToolCall) -> String {
    let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
    let detail = ["path", "pattern", "dir"]
        .iter()
        .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
        .unwrap_or("");
    format!("{} {detail}", call.function.name).trim().to_string()
}

/// Run one read-only tool call against `root`. Errors come back as text for the model.
pub fn execute_tool(root: &Path, call: &ToolCall) -> String {
    let args: Value = match serde_json::from_str(if call.function.arguments.trim().is_empty() {
        "{}"
    } else {
        &call.function.arguments
    }) {
        Ok(v) => v,
        Err(e) => return format!("Error: the arguments are not valid JSON ({e})."),
    };
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let n = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|t| t.parse().ok())))
            .map(|v| v as usize)
    };
    match call.function.name.as_str() {
        "read_file" => read_file_tool(root, &s("path"), n("start_line"), n("end_line")),
        "list_files" => list_files_tool(root, &s("dir")),
        "search" => search_tool(root, &s("pattern"), &s("path_glob")),
        other => format!("Error: unknown tool `{other}`. Available: read_file, list_files, search."),
    }
}

fn read_file_tool(root: &Path, path: &str, start: Option<usize>, end: Option<usize>) -> String {
    if path.is_empty() {
        return "Error: `path` is required.".into();
    }
    let text = match read_text(root, path) {
        Ok(Some(t)) => t,
        Ok(None) => return format!("Error: `{path}` does not exist."),
        Err(e) => return format!("Error: {e}"),
    };
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let from = start.unwrap_or(1).max(1);
    if from > total.max(1) {
        return format!("`{path}` has only {total} lines.");
    }
    let to = end
        .unwrap_or(from + READ_MAX_LINES - 1)
        .min(total)
        .min(from + READ_MAX_LINES - 1)
        .max(from);
    let mut body = String::new();
    let mut last = from - 1;
    for (i, l) in lines.iter().enumerate().take(to).skip(from - 1) {
        if body.len() + l.len() > READ_MAX_CHARS {
            break;
        }
        body.push_str(l);
        body.push('\n');
        last = i + 1;
    }
    let range = if from == 1 && last == total {
        format!("all {total} lines")
    } else {
        format!(
            "lines {from}-{last} of {total}; call read_file again with start_line {} for more",
            last + 1
        )
    };
    format!("`{path}` ({range}):\n{body}")
}

fn list_files_tool(root: &Path, dir: &str) -> String {
    let list = match list_files(root, 5000) {
        Ok(l) => l,
        Err(e) => return format!("Error: {e}"),
    };
    let prefix = dir.trim_matches('/').replace('\\', "/");
    let mut sub = list.clone();
    if !prefix.is_empty() && prefix != "." {
        sub.files.retain(|f| f.path.starts_with(&format!("{prefix}/")));
        if sub.files.is_empty() {
            return format!("No files under `{prefix}`.");
        }
    }
    format!("{} files:\n{}", sub.files.len(), tree_text(&sub, LIST_MAX_LINES))
}

fn search_tool(root: &Path, pattern: &str, glob: &str) -> String {
    if pattern.is_empty() {
        return "Error: `pattern` is required.".into();
    }
    let re = RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .or_else(|_| RegexBuilder::new(&regex::escape(pattern)).case_insensitive(true).build());
    let Ok(re) = re else {
        return "Error: invalid pattern.".into();
    };
    let matcher = (!glob.is_empty()).then(|| globset_like(glob));
    let Ok(list) = list_files(root, 5000) else {
        return "Error: cannot list the project.".into();
    };
    let mut out = Vec::new();
    let mut files_hit = 0;
    for f in &list.files {
        if f.size > 1_000_000 || matcher.as_ref().is_some_and(|m| !m.is_match(&f.path)) {
            continue;
        }
        let Ok(Some(text)) = read_text(root, &f.path) else { continue };
        let mut hit = false;
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                hit = true;
                let shown: String = line.trim().chars().take(200).collect();
                out.push(format!("{}:{}: {shown}", f.path, i + 1));
                if out.len() >= SEARCH_MAX_MATCHES {
                    break;
                }
            }
        }
        files_hit += hit as usize;
        if out.len() >= SEARCH_MAX_MATCHES {
            out.push(format!(
                "… (stopped at {SEARCH_MAX_MATCHES} matches; narrow the pattern or path_glob)"
            ));
            break;
        }
    }
    if out.is_empty() {
        format!("No matches for `{pattern}`.")
    } else {
        format!("{} match(es) in {files_hit} file(s):\n{}", out.len(), out.join("\n"))
    }
}

/// A glob (`*`, `**`, `?`) as a regex over forward-slash paths.
fn globset_like(glob: &str) -> regex::Regex {
    let mut re = String::from("(?i)^");
    let g = glob.trim().trim_start_matches("./").replace('\\', "/");
    let mut chars = g.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    regex::Regex::new(&re).unwrap_or_else(|_| regex::Regex::new("^$").unwrap())
}

/// The project's source files for the planner's prompt, within about `budget_tokens`: small
/// files in full, larger ones as outlines (declarations and signatures), skipping what
/// `skip` already contains (files inlined elsewhere in the prompt).
pub fn project_outline(root: &Path, budget_tokens: u32, skip: &[String]) -> String {
    let Ok(list) = list_files(root, 3000) else { return String::new() };
    let mut out = String::new();
    let mut used = 0u32;
    let mut left_out = 0usize;
    let mut files: Vec<_> = list
        .files
        .iter()
        .filter(|f| outline::supports(&f.path) && f.size < 400_000 && !skip.iter().any(|s| s.eq_ignore_ascii_case(&f.path)))
        .collect();
    // Shallow files first: entry points and top-level modules say most about the design.
    files.sort_by_key(|f| (f.path.matches('/').count(), f.path.to_lowercase()));
    for f in files {
        let Ok(Some(text)) = read_text(root, &f.path) else { continue };
        let full = crate::budget::estimate_tokens(&text);
        let (body, label) = if full <= 400 {
            (text, "")
        } else {
            match outline::outline(&f.path, &text) {
                Some(o) => (o.text, " (outline)"),
                None => (text, ""), // nothing to omit
            }
        };
        let t = crate::budget::estimate_tokens(&body);
        if used + t > budget_tokens {
            left_out += 1;
            continue;
        }
        used += t;
        out.push_str(&format!(
            "\n## `{}`{label}\n{}\n",
            f.path,
            crate::editformat::fenced_lang(&body, crate::pipeline::lang_for(&f.path))
        ));
    }
    if out.is_empty() {
        return String::new();
    }
    let mut head = String::from(
        "# Source overview\nSmall files in full; files marked (outline) show declarations, signatures and doc comments, with `… (N lines omitted)` for bodies.\n",
    );
    if left_out > 0 {
        head.push_str(&format!(
            "{left_out} more source file(s) did not fit; read them with the tools if needed.\n"
        ));
    }
    head + &out
}

/// What the planner streams while it works.
pub enum PlannerEvent<'a> {
    Content(&'a str),
    Reasoning(&'a str),
    /// A tool call it made (described) — also starts a new answer segment.
    Tool(String),
    Status(String),
    Stats(LiveStats),
}

pub struct PlannerCall<'a> {
    pub profile: &'a Profile,
    pub endpoint: &'a Endpoint,
    pub system: String,
    pub prompt: String,
    /// Project root for the tools; `None` → no tools.
    pub tools_root: Option<&'a Path>,
    pub effort: &'a str,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannerOutcome {
    pub text: String,
    pub reasoning_chars: usize,
    pub tool_calls: Vec<String>,
    pub requests: usize,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: u32,
    pub tok_per_sec: Option<f64>,
    pub duration_ms: u64,
    pub cancelled: bool,
    pub finish_reason: Option<String>,
    /// Tools were offered but the server rejected them; the planner worked without.
    pub tools_unsupported: bool,
}

fn estimate_messages(msgs: &[ChatMessage]) -> u32 {
    msgs.iter()
        .map(|m| {
            crate::budget::estimate_tokens(&m.content)
                + m.tool_calls
                    .as_ref()
                    .map(|c| c.iter().map(|t| crate::budget::estimate_tokens(&t.function.arguments) + 10).sum())
                    .unwrap_or(0)
                + 8
        })
        .sum()
}

/// Run the planner until it answers without calling a tool. The answer has reasoning removed.
pub async fn run(
    http: &reqwest::Client,
    call: PlannerCall<'_>,
    cancel: &CancellationToken,
    on_event: &mut (dyn FnMut(PlannerEvent<'_>) + Send),
) -> Result<PlannerOutcome> {
    let started = Instant::now();
    let profile = call.profile;
    let ctx = profile.context_limit;
    let tools = call.tools_root.map(|_| tool_definitions());
    let mut out = PlannerOutcome::default();
    let mut messages = vec![ChatMessage::system(call.system.clone()), ChatMessage::user(call.prompt.clone())];
    let mut offer_tools = tools.is_some();
    let mut speeds = Vec::new();
    let mut used_tokens: u32 = 0;
    for round in 0..=MAX_TOOL_ROUNDS {
        if cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }
        let prompt_est = used_tokens.max(estimate_messages(&messages)) + if offer_tools { 600 } else { 0 };
        if prompt_est + 2048 > ctx {
            bail!(
                "The planner's prompt (≈{prompt_est} tokens) does not fit {}'s {ctx}-token context. Untick some context files.",
                profile.name
            );
        }
        let max_tokens = (ctx - prompt_est - 512).clamp(1024, MAX_OUTPUT);
        let last_round = round == MAX_TOOL_ROUNDS;
        let body = build_planner_body(
            profile,
            &call.endpoint.model,
            &messages,
            max_tokens,
            call.effort,
            if offer_tools && !last_round { tools.as_ref() } else { None },
        );
        let mut answer_started = false;
        let mut cb = |d: Delta<'_>, s: LiveStats| {
            match d {
                Delta::Content(t) => {
                    answer_started = true;
                    on_event(PlannerEvent::Content(t))
                }
                Delta::Reasoning(t) => on_event(PlannerEvent::Reasoning(t)),
            }
            if s.tokens % 32 == 0 {
                on_event(PlannerEvent::Stats(s));
            }
        };
        let res = stream_chat(http, call.endpoint, &body, cancel, &mut cb).await;
        let o = match res {
            Ok(o) => o,
            Err(e) if offer_tools && round == 0 && e.to_string().contains("HTTP 4") => {
                // Server without tool support (no Jinja templates, or an external endpoint).
                on_event(PlannerEvent::Status(format!(
                    "The server rejected tools ({e}); planning without them."
                )));
                out.tools_unsupported = true;
                offer_tools = false;
                continue;
            }
            Err(e) => return Err(e),
        };
        out.requests += 1;
        out.completion_tokens += o.completion_tokens.unwrap_or(0);
        out.prompt_tokens = o.prompt_tokens.or(out.prompt_tokens);
        out.finish_reason = o.finish_reason.clone();
        if let Some(t) = o.tok_per_sec {
            speeds.push(t);
        }
        out.reasoning_chars += o.reasoning.len();
        if o.cancelled {
            out.cancelled = true;
            out.text = strip_reasoning(&profile.reasoning, &o.content).content;
            break;
        }
        used_tokens = o.prompt_tokens.unwrap_or(0) + o.completion_tokens.unwrap_or(0);
        if !o.tool_calls.is_empty() && offer_tools && !last_round {
            let root = call.tools_root.unwrap_or(Path::new("."));
            let stripped = strip_reasoning(&profile.reasoning, &o.content).content;
            let mut assistant = ChatMessage::assistant_tool_calls(stripped, o.tool_calls.clone());
            if !o.reasoning.is_empty() {
                // Qwen3.x keeps the reasoning of the current turn between tool calls.
                assistant.reasoning_content = Some(o.reasoning.clone());
            }
            messages.push(assistant);
            for tc in &o.tool_calls {
                let what = describe_call(tc);
                on_event(PlannerEvent::Tool(what.clone()));
                out.tool_calls.push(what);
                messages.push(ChatMessage::tool_result(&tc.id, execute_tool(root, tc)));
            }
            let est = used_tokens.max(estimate_messages(&messages));
            if f64::from(est) > f64::from(ctx) * TOOL_CONTEXT_SHARE {
                offer_tools = false;
                messages.push(ChatMessage::user(
                    "The context window is nearly full. Do not call more tools; write your complete answer now, in the required format.",
                ));
            }
            continue;
        }
        let stripped = strip_reasoning(&profile.reasoning, &o.content);
        out.reasoning_chars += stripped.reasoning.len();
        out.text = stripped.content.trim().to_string();
        if out.text.is_empty() && round < MAX_TOOL_ROUNDS {
            if o.finish_reason.as_deref() == Some("length") {
                bail!(
                    "{} used all {max_tokens} output tokens without writing an answer (thinking too long). Try again, or lower the planner effort.",
                    profile.name
                );
            }
            // Thought, but said nothing: ask once more, without tools.
            offer_tools = false;
            messages.push(ChatMessage::assistant(""));
            messages.push(ChatMessage::user("Write your complete answer now, in the required format."));
            continue;
        }
        break;
    }
    out.duration_ms = started.elapsed().as_millis() as u64;
    out.tok_per_sec = (!speeds.is_empty()).then(|| speeds.iter().sum::<f64>() / speeds.len() as f64);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::FunctionCall;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
        }
    }

    fn project() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        let long: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(d.path().join("src/long.txt"), long).unwrap();
        std::fs::write(d.path().join("src/main.rs"), "fn main() {\n    run_game();\n}\n").unwrap();
        std::fs::write(d.path().join("README.md"), "Run the game\n").unwrap();
        d
    }

    #[test]
    fn read_file_pages_long_files_and_stays_in_the_project() {
        let d = project();
        let r = execute_tool(d.path(), &call("read_file", json!({"path": "src/long.txt"})));
        assert!(
            r.starts_with("`src/long.txt` (lines 1-600 of 1000; call read_file again with start_line 601"),
            "{}",
            &r[..120]
        );
        assert!(r.contains("line 600\n") && !r.contains("line 601\n"));
        let r = execute_tool(
            d.path(),
            &call("read_file", json!({"path": "src/long.txt", "start_line": "990", "end_line": 2000})),
        );
        assert!(r.contains("lines 990-1000 of 1000") && r.ends_with("line 1000\n"), "{r}");
        let all = execute_tool(d.path(), &call("read_file", json!({"path": "src/main.rs"})));
        assert!(all.contains("(all 3 lines)") && all.contains("run_game();"));
        for bad in ["../outside.txt", "C:/Windows/win.ini", "missing.rs"] {
            let r = execute_tool(d.path(), &call("read_file", json!({ "path": bad })));
            assert!(r.starts_with("Error"), "{bad}: {r}");
        }
        assert!(
            execute_tool(
                d.path(),
                &ToolCall {
                    function: FunctionCall {
                        name: "read_file".into(),
                        arguments: "{not json".into()
                    },
                    ..call("x", json!({}))
                }
            )
            .contains("not valid JSON")
        );
    }

    #[test]
    fn list_and_search() {
        let d = project();
        let l = execute_tool(d.path(), &call("list_files", json!({})));
        assert!(l.starts_with("3 files:") && l.contains("main.rs"), "{l}");
        let l = execute_tool(d.path(), &call("list_files", json!({"dir": "src/"})));
        assert!(l.starts_with("2 files:") && !l.contains("README"), "{l}");
        let s = execute_tool(d.path(), &call("search", json!({"pattern": "RUN_GAME|run the"})));
        assert!(
            s.contains("src/main.rs:2: run_game();") && s.contains("README.md:1: Run the game"),
            "{s}"
        );
        let s = execute_tool(d.path(), &call("search", json!({"pattern": "run", "path_glob": "src/**/*.rs"})));
        assert!(s.contains("src/main.rs") && !s.contains("README"), "{s}");
        let s = execute_tool(d.path(), &call("search", json!({"pattern": "fn main("})));
        assert!(s.contains("src/main.rs:1"), "an invalid regex is searched literally: {s}");
        assert!(execute_tool(d.path(), &call("delete_file", json!({}))).contains("unknown tool"));
        assert_eq!(describe_call(&call("read_file", json!({"path": "src/a.rs"}))), "read_file src/a.rs");
    }

    #[test]
    fn globs() {
        assert!(globset_like("src/**/*.rs").is_match("src/a/b/c.rs"));
        assert!(globset_like("src/**/*.rs").is_match("src/c.rs"));
        assert!(!globset_like("src/*.rs").is_match("src/a/c.rs"));
        assert!(globset_like("*.md").is_match("README.md"));
    }

    #[test]
    fn outline_fits_the_budget_and_skips_inlined_files() {
        let d = project();
        let mut big = String::new();
        for i in 0..200 {
            big.push_str(&format!(
                "pub fn helper_{i}(x: u32) -> u32 {{\n    let y = x + {i};\n    let z = y + 1;\n    z * 2\n}}\n\n"
            ));
        }
        std::fs::write(d.path().join("src/lib.rs"), big).unwrap();
        let o = project_outline(d.path(), 100_000, &[]);
        assert!(o.contains("## `src/main.rs`\n```rust\nfn main()"), "small file in full: {o}");
        assert!(
            o.contains("## `src/lib.rs` (outline)") && o.contains("pub fn helper_7(x: u32) -> u32"),
            "{}",
            &o[..o.len().min(1500)]
        );
        assert!(!o.contains("z * 2"), "bodies are omitted");
        let skipped = project_outline(d.path(), 100_000, &["src/main.rs".into()]);
        assert!(!skipped.contains("## `src/main.rs`"));
        let tiny = project_outline(d.path(), 30, &[]);
        assert!(tiny.contains("did not fit"), "{tiny}");
    }
}
