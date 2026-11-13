//! Parsing of the Markdown spec Claude produces.
//!
//! Expected structure (headings are matched case-insensitively, `#` or `##`):
//!
//! ```text
//! ## Summary
//! ## Files
//! - `src/a.ts` (modify) — why
//! - `src/b.ts` (create) — why
//! ## Changes
//! ### `src/a.ts`
//! ...
//! ## Edge cases
//! ## Acceptance criteria
//! ```

use crate::pathsafe::normalize_rel;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileAction {
    Modify,
    Create,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecFile {
    pub path: String,
    pub action: FileAction,
    pub note: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedSpec {
    pub summary: String,
    pub files: Vec<SpecFile>,
    /// (path, section text) for each `### path` subsection under Changes, in order.
    pub changes: Vec<(String, String)>,
    pub edge_cases: String,
    pub acceptance: String,
    /// Other backticked paths mentioned anywhere in the spec (deduplicated, excluding `files`).
    pub mentioned_paths: Vec<String>,
    pub warnings: Vec<String>,
}

static BACKTICK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static ACTION_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\((?:modify|create|new|edit|delete|remove|update|new file)\)").unwrap());
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.+?)\s*#*\s*$").unwrap());

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Preamble,
    Summary,
    Files,
    Changes,
    EdgeCases,
    Acceptance,
    Other,
}

fn classify(title: &str) -> Section {
    let t = title.trim().trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
    if t.starts_with("summary") || t == "overview" || t == "goal" {
        Section::Summary
    } else if t.starts_with("files") || t == "file list" || t.starts_with("files to") {
        Section::Files
    } else if t.starts_with("changes") || t.starts_with("implementation") || t.starts_with("step") {
        Section::Changes
    } else if t.starts_with("edge case") {
        Section::EdgeCases
    } else if t.starts_with("acceptance") {
        Section::Acceptance
    } else {
        Section::Other
    }
}

/// Heuristic: does this string look like a repository-relative file path?
pub fn looks_like_path(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.contains(' ') || s.contains('(') || s.contains('=') || s.starts_with('-') || s.ends_with('/') {
        return false;
    }
    let name = s.rsplit('/').next().unwrap_or(s);
    let has_ext = name.rfind('.').is_some_and(|i| {
        i > 0
            && i < name.len() - 1
            && name[i + 1..].chars().all(|c| c.is_ascii_alphanumeric())
            && name[i + 1..].chars().any(|c| c.is_ascii_alphabetic())
    });
    let known = matches!(
        name,
        "Makefile" | "Dockerfile" | "Justfile" | "justfile" | "LICENSE" | "Procfile" | "Gemfile" | "Rakefile"
    );
    let dotfile = name.starts_with('.') && name.len() > 1;
    (has_ext || known || dotfile) && normalize_rel(s).is_ok()
}

/// Extract a path from a heading or list-item text.
pub fn extract_path(text: &str) -> Option<String> {
    for cap in BACKTICK.captures_iter(text) {
        let c = cap[1].trim();
        if looks_like_path(c) {
            return normalize_rel(c).ok();
        }
    }
    // Fall back to the first token, stripped of markdown punctuation.
    let first = text
        .split_whitespace()
        .next()?
        .trim_matches(|c: char| c == '*' || c == '`' || c == '"' || c == '\'' || c == ':' || c == ',');
    if looks_like_path(first) {
        return normalize_rel(first).ok();
    }
    None
}

fn detect_action(text: &str) -> FileAction {
    let t = text.to_ascii_lowercase();
    if t.contains("(create)")
        || t.contains("(new)")
        || t.contains("new file")
        || t.contains("— create")
        || t.contains("- create")
        || t.contains(": create")
    {
        FileAction::Create
    } else if t.contains("(delete)") || t.contains("(remove)") {
        FileAction::Delete
    } else {
        FileAction::Modify
    }
}

pub fn parse_spec(md: &str) -> ParsedSpec {
    let text = md.replace("\r\n", "\n");
    let mut spec = ParsedSpec::default();
    let mut section = Section::Preamble;
    let mut top_level: Option<usize> = None; // heading level used for top sections
    let mut summary = Vec::new();
    let mut edge = Vec::new();
    let mut acc = Vec::new();
    let mut current_change: Option<(String, Vec<String>)> = None;
    let mut in_fence = false;

    let flush_change = |spec: &mut ParsedSpec, cur: &mut Option<(String, Vec<String>)>| {
        if let Some((p, body)) = cur.take() {
            spec.changes.push((p, body.join("\n").trim().to_string()));
        }
    };

    for line in text.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            in_fence = !in_fence;
        }
        if !in_fence && let Some(cap) = HEADING.captures(line) {
            let level = cap[1].len();
            let title = cap[2].to_string();
            let class = classify(&title);
            let is_top = match top_level {
                Some(l) => level <= l,
                None => class != Section::Other && level <= 2,
            };
            if is_top && (class != Section::Other || top_level.is_some()) {
                if top_level.is_none() {
                    top_level = Some(level);
                }
                flush_change(&mut spec, &mut current_change);
                section = class;
                continue;
            }
            if section == Section::Changes
                && let Some(p) = extract_path(&title)
            {
                flush_change(&mut spec, &mut current_change);
                current_change = Some((p, Vec::new()));
                continue;
            }
        }
        match section {
            Section::Summary => summary.push(line),
            Section::Files => {
                let t = line.trim_start();
                let item = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| {
                    // numbered list "1. "
                    let d = t.find(". ")?;
                    t[..d].chars().all(|c| c.is_ascii_digit()).then(|| &t[d + 2..])
                });
                if let Some(item) = item {
                    match extract_path(item) {
                        Some(path) => {
                            if !spec.files.iter().any(|f| f.path.eq_ignore_ascii_case(&path)) {
                                let no_path = BACKTICK.replace(item, "");
                                let note = ACTION_TAG
                                    .replace_all(&no_path, "")
                                    .trim()
                                    .trim_start_matches(['—', '-', ':', ' '])
                                    .trim()
                                    .to_string();
                                spec.files.push(SpecFile {
                                    path,
                                    action: detect_action(item),
                                    note,
                                });
                            }
                        }
                        None => spec
                            .warnings
                            .push(format!("Could not find a file path in Files entry: {}", item.trim())),
                    }
                }
            }
            Section::Changes => {
                if let Some((_, body)) = current_change.as_mut() {
                    body.push(line.to_string());
                }
            }
            Section::EdgeCases => edge.push(line),
            Section::Acceptance => acc.push(line),
            _ => {}
        }
    }
    flush_change(&mut spec, &mut current_change);
    spec.summary = summary.join("\n").trim().to_string();
    spec.edge_cases = edge.join("\n").trim().to_string();
    spec.acceptance = acc.join("\n").trim().to_string();

    // Paths mentioned elsewhere (e.g. "see `src/types.ts`").
    let mut mentioned = Vec::new();
    for cap in BACKTICK.captures_iter(&text) {
        let c = cap[1].trim();
        if looks_like_path(c)
            && let Ok(p) = normalize_rel(c)
            && !spec.files.iter().any(|f| f.path.eq_ignore_ascii_case(&p))
            && !mentioned.iter().any(|m: &String| m.eq_ignore_ascii_case(&p))
        {
            mentioned.push(p);
        }
    }
    spec.mentioned_paths = mentioned;

    if spec.files.is_empty() {
        spec.warnings
            .push("The spec has no `## Files` list, so the files to edit are unknown.".into());
    }
    for f in &spec.files {
        if f.action != FileAction::Delete && !spec.changes.iter().any(|(p, _)| p.eq_ignore_ascii_case(&f.path)) {
            spec.warnings.push(format!(
                "`{}` is listed under Files but has no `### {}` section under Changes.",
                f.path, f.path
            ));
        }
    }
    spec
}

/// Build the self-contained spec for one file (used in per-file split mode).
pub fn per_file_spec(spec: &ParsedSpec, path: &str) -> String {
    let mut out = String::new();
    if !spec.summary.is_empty() {
        out.push_str("## Summary\n");
        out.push_str(&spec.summary);
        out.push_str("\n\n");
    }
    let others: Vec<String> = spec
        .files
        .iter()
        .filter(|f| !f.path.eq_ignore_ascii_case(path))
        .map(|f| format!("- `{}` ({}) — handled in a separate step", f.path, action_word(f.action)))
        .collect();
    if let Some(f) = spec.files.iter().find(|f| f.path.eq_ignore_ascii_case(path)) {
        out.push_str(&format!(
            "## File for this step\n- `{}` ({}){}\n",
            f.path,
            action_word(f.action),
            if f.note.is_empty() {
                String::new()
            } else {
                format!(" — {}", f.note)
            }
        ));
    }
    if !others.is_empty() {
        out.push_str("\nOther files in this change (do NOT edit them now):\n");
        out.push_str(&others.join("\n"));
        out.push('\n');
    }
    out.push_str(&format!("\n## Changes for `{path}`\n"));
    match spec.changes.iter().find(|(p, _)| p.eq_ignore_ascii_case(path)) {
        Some((_, body)) => out.push_str(body),
        None => out.push_str("(No dedicated section; follow the summary.)"),
    }
    out.push('\n');
    if !spec.edge_cases.is_empty() {
        out.push_str("\n## Edge cases\n");
        out.push_str(&spec.edge_cases);
        out.push('\n');
    }
    if !spec.acceptance.is_empty() {
        out.push_str("\n## Acceptance criteria\n");
        out.push_str(&spec.acceptance);
        out.push('\n');
    }
    out
}

pub fn action_word(a: FileAction) -> &'static str {
    match a {
        FileAction::Modify => "modify",
        FileAction::Create => "create",
        FileAction::Delete => "delete",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"# Spec: retry with backoff

## Summary
Add retries with exponential backoff to the HTTP client.

## Files
- `src/http/client.ts` (modify) — wrap fetch in retry loop
- `src/http/retry.ts` (create) — new helper
* src/http/index.ts (modify): re-export

## Changes

### `src/http/client.ts`
1. Import `retry` from `./retry`.
2. In `request()`, call `withRetry(...)`.

```ts
// example with a heading-like line inside a fence
## not a heading
```

### src/http/retry.ts
Create `withRetry<T>(fn: () => Promise<T>, opts)`; see `src/http/types.ts` for `RetryOptions`.

### `src/http/index.ts`
Export it.

## Edge cases
- Do not retry on 4xx.

## Acceptance criteria
- 3 attempts max.
"#;

    #[test]
    fn parses_all_sections() {
        let s = parse_spec(SPEC);
        assert_eq!(s.summary, "Add retries with exponential backoff to the HTTP client.");
        let paths: Vec<_> = s.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/http/client.ts", "src/http/retry.ts", "src/http/index.ts"]);
        assert_eq!(s.files[1].action, FileAction::Create);
        assert_eq!(s.files[0].action, FileAction::Modify);
        assert_eq!(s.files[0].note, "wrap fetch in retry loop");
        assert_eq!(s.changes.len(), 3);
        assert!(
            s.changes[0].1.contains("## not a heading"),
            "fenced heading must stay inside the section"
        );
        assert_eq!(s.changes[1].0, "src/http/retry.ts");
        assert_eq!(s.edge_cases, "- Do not retry on 4xx.");
        assert_eq!(s.acceptance, "- 3 attempts max.");
        assert_eq!(s.mentioned_paths, vec!["src/http/types.ts"]);
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
    }

    #[test]
    fn warns_on_missing_sections() {
        let s = parse_spec("## Summary\nx\n## Files\n- `a.py`\n- something without a path\n");
        assert_eq!(s.files.len(), 1);
        assert!(s.warnings.iter().any(|w| w.contains("without a path")));
        assert!(s.warnings.iter().any(|w| w.contains("no `### a.py` section")));
        let empty = parse_spec("just text");
        assert!(empty.warnings.iter().any(|w| w.contains("no `## Files` list")));
    }

    #[test]
    fn rejects_unsafe_paths_in_files_list() {
        let s = parse_spec("## Files\n- `../etc/passwd`\n- `C:/x.txt`\n- `ok/file.rs`\n");
        assert_eq!(s.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["ok/file.rs"]);
    }

    #[test]
    fn path_heuristics() {
        assert!(looks_like_path("src/a.ts"));
        assert!(looks_like_path(".env.example"));
        assert!(looks_like_path("Makefile"));
        assert!(!looks_like_path("withRetry()"));
        assert!(!looks_like_path("x = 1"));
        assert!(!looks_like_path("v1.2"));
        assert!(!looks_like_path("src/"));
    }

    #[test]
    fn per_file_spec_is_self_contained() {
        let s = parse_spec(SPEC);
        let p = per_file_spec(&s, "src/http/retry.ts");
        assert!(p.contains("## Summary"));
        assert!(p.contains("## Changes for `src/http/retry.ts`"));
        assert!(p.contains("withRetry<T>"));
        assert!(!p.contains("Import `retry`"), "other files' changes must not leak in");
        assert!(p.contains("`src/http/client.ts` (modify) — handled in a separate step"));
        assert!(p.contains("## Edge cases"));
        assert!(p.contains("## Acceptance criteria"));
    }

    #[test]
    fn level_one_headings_work_too() {
        let s = parse_spec("# Summary\nS\n# Files\n- `a.rs` (new)\n# Changes\n## a.rs\nbody\n# Acceptance criteria\nok\n");
        assert_eq!(s.files[0].action, FileAction::Create);
        assert_eq!(s.changes, vec![("a.rs".to_string(), "body".to_string())]);
        assert_eq!(s.acceptance, "ok");
    }
}
