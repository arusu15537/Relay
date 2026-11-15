//! Parsing of the Markdown spec Claude produces.
//!
//! Expected structure (headings are matched case-insensitively, `#` or `##`):
//!
//! ```text
//! ## Summary
//! ## Design            (types, responsibilities, signatures, relationships, decisions)
//! ## Files
//! - `src/a.ts` (modify) — why
//! - `src/b.ts` (create) — why
//! ## Changes
//! ### `src/a.ts`
//! ...
//! ## Edge cases
//! ## Acceptance criteria
//! ## Verification      (optional: `- build: `cargo build``, `- test: …`, `- run: … (timeout 10s)`)
//! ```

use crate::pathsafe::normalize_rel;
use crate::verify::{VerifyCommand, parse_verification_item};
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
    /// The class-diagram-level design (shared by every per-file call).
    #[serde(default)]
    pub design: String,
    pub files: Vec<SpecFile>,
    /// (path, section text) for each `### path` subsection under Changes, in order.
    pub changes: Vec<(String, String)>,
    pub edge_cases: String,
    pub acceptance: String,
    /// Other backticked paths mentioned anywhere in the spec (deduplicated, excluding `files`).
    pub mentioned_paths: Vec<String>,
    /// Commands from `## Verification`, in order.
    #[serde(default)]
    pub verification: Vec<VerifyCommand>,
    /// The spec has a `## Verification` section (possibly saying there is nothing to run). Without
    /// one, the commands detected from the project are used.
    #[serde(default)]
    pub verification_declared: bool,
    pub warnings: Vec<String>,
}

/// A fenced block with at least this many non-blank lines counts as code (a one- or two-line
/// fence holding a signature or a literal value is fine). Mermaid diagrams are not code.
pub const CODE_BLOCK_MIN_LINES: usize = 3;

static BACKTICK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static ACTION_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\((?:modify|create|new|edit|delete|remove|update|new file)\)").unwrap());
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.+?)\s*#*\s*$").unwrap());
static CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_][\w.]*\s*\(").unwrap());
static IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_][\w.]*").unwrap());

/// Is this inline code span an implementation (an expression or call chain) rather than a name,
/// signature, type or literal value? Flags calls nested in calls, calls combined with arithmetic,
/// `**`, and arithmetic between two or more names. A quantity such as `max_retries + 1` and
/// signatures with type annotations or defaults are fine.
pub fn is_code_expression(span: &str) -> bool {
    let s = span.trim();
    let annotated = s.contains("->") || s.contains(": ");
    let arithmetic = [" * ", " + ", " - ", " / ", " % "].iter().any(|op| s.contains(op));
    let nested_call = !annotated && s.matches('(').count() >= 2;
    let names = IDENT.find_iter(s).count();
    nested_call || s.contains("**") || (!annotated && arithmetic && (CALL.is_match(s) || names >= 2))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Preamble,
    Summary,
    Design,
    Files,
    Changes,
    EdgeCases,
    Acceptance,
    Verification,
    Other,
}

fn classify(title: &str) -> Section {
    let t = title.trim().trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
    if t.starts_with("summary") || t == "overview" || t == "goal" {
        Section::Summary
    } else if t.starts_with("design") || t.starts_with("architecture") {
        Section::Design
    } else if t.starts_with("files") || t == "file list" || t.starts_with("files to") {
        Section::Files
    } else if t.starts_with("changes") || t.starts_with("implementation") || t.starts_with("step") {
        Section::Changes
    } else if t.starts_with("edge case") {
        Section::EdgeCases
    } else if t.starts_with("acceptance") {
        Section::Acceptance
    } else if t.starts_with("verification") || t.starts_with("verify") || t == "commands" {
        Section::Verification
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
    let mut design = Vec::new();
    let mut edge = Vec::new();
    let mut acc = Vec::new();
    let mut current_change: Option<(String, Vec<String>)> = None;
    let mut in_fence = false;
    let mut fence_is_diagram = false;
    // Code in the spec: (blocks with at least CODE_BLOCK_MIN_LINES lines, their total lines).
    let mut fence_lines = 0usize;
    let mut code_blocks = 0usize;
    let mut code_lines = 0usize;
    let mut expressions: Vec<String> = Vec::new();

    let flush_change = |spec: &mut ParsedSpec, cur: &mut Option<(String, Vec<String>)>| {
        if let Some((p, body)) = cur.take() {
            spec.changes.push((p, body.join("\n").trim().to_string()));
        }
    };

    for line in text.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            if in_fence && fence_lines >= CODE_BLOCK_MIN_LINES && !fence_is_diagram {
                code_blocks += 1;
                code_lines += fence_lines;
            }
            in_fence = !in_fence;
            fence_is_diagram = in_fence && line.trim_start()[3..].trim().eq_ignore_ascii_case("mermaid");
            fence_lines = 0;
        } else if in_fence && !line.trim().is_empty() {
            fence_lines += 1;
        } else if !in_fence && section != Section::Verification {
            for cap in BACKTICK.captures_iter(line) {
                if is_code_expression(&cap[1]) && !expressions.iter().any(|e| e == &cap[1]) {
                    expressions.push(cap[1].to_string());
                }
            }
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
                if class == Section::Verification {
                    spec.verification_declared = true;
                }
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
            Section::Design => design.push(line),
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
            Section::Verification => {
                spec.verification_declared = true;
                let t = line.trim_start();
                if t.starts_with("- ")
                    || t.starts_with("* ")
                    || t.split_once(". ")
                        .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                {
                    match parse_verification_item(line) {
                        Ok(Some(cmd)) => {
                            if !spec.verification.contains(&cmd) {
                                spec.verification.push(cmd);
                            }
                        }
                        Ok(None) => {}
                        Err(w) => spec.warnings.push(w),
                    }
                }
            }
            _ => {}
        }
    }
    flush_change(&mut spec, &mut current_change);
    spec.summary = summary.join("\n").trim().to_string();
    spec.design = design.join("\n").trim().to_string();
    spec.edge_cases = edge.join("\n").trim().to_string();
    spec.acceptance = acc.join("\n").trim().to_string();

    // Paths mentioned elsewhere (e.g. "see `src/types.ts`"), outside the Verification commands.
    let mut mentioned = Vec::new();
    let commands: String = spec.verification.iter().map(|v| format!("`{}`", v.command)).collect();
    let prose = BACKTICK.replace_all(&text, |c: &regex::Captures| {
        if commands.contains(&format!("`{}`", &c[1])) {
            String::new()
        } else {
            c[0].to_string()
        }
    });
    for cap in BACKTICK.captures_iter(&prose) {
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

    if code_blocks > 0 {
        spec.warnings.push(format!(
            "The spec contains code ({code_lines} lines in {code_blocks} block(s)). Specs describe the design (types, responsibilities, signatures, \
             relationships, contracts); the local model writes all of the code. Remove the code or regenerate the spec."
        ));
    }
    if !expressions.is_empty() {
        let shown: Vec<String> = expressions.iter().take(3).map(|e| format!("`{e}`")).collect();
        spec.warnings.push(format!(
            "The spec writes {} expression(s) in code syntax ({}{}). Specs state rules in words; the local model writes the code.",
            expressions.len(),
            shown.join(", "),
            if expressions.len() > 3 { ", …" } else { "" }
        ));
    }
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
    if !spec.design.is_empty() {
        out.push_str("## Design\n");
        out.push_str(&spec.design);
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
    fn code_in_spec_is_flagged_but_short_signatures_are_fine() {
        let with_code = "## Files\n- `a.py` (modify)\n## Changes\n### `a.py`\nAdd:\n```python\ndef f(x):\n    if x:\n        return 1\n    return 2\n```\n";
        let s = parse_spec(with_code);
        assert!(
            s.warnings.iter().any(|w| w.contains("contains code (4 lines in 1 block(s))")),
            "{:?}",
            s.warnings
        );

        let signature_only = "## Files\n- `a.py` (modify)\n## Changes\n### `a.py`\nAdd a function:\n```python\ndef f(x: int) -> int\n```\nthat returns 1 for truthy `x`, else 2.\n";
        let s = parse_spec(signature_only);
        assert!(!s.warnings.iter().any(|w| w.contains("contains code")), "{:?}", s.warnings);
    }

    const DESIGN_SPEC: &str = "## Summary\nRetries for the client.\n\n## Design\n### Elements\n- `RetryPolicy` (new, frozen dataclass): `max_attempts: int = 3`, `delay(attempt: int) -> float`.\n- `HttpClient` (changed): owns a `RetryPolicy`; `fetch(path: str) -> Response` raises `HttpError` after the last attempt.\n### Decisions\n- Policy object instead of loose arguments, so tests can inject one.\n\n```mermaid\nclassDiagram\n  HttpClient *-- RetryPolicy\n  class RetryPolicy {\n    +max_attempts: int\n    +delay(attempt: int) float\n  }\n```\n\n## Files\n- `client.py` (modify) — use the policy\n- `retry.py` (create) — the policy\n\n## Changes\n### `client.py`\n`HttpClient.__init__` takes `policy: RetryPolicy | None = None`.\n### `retry.py`\nDefines `RetryPolicy` as in the Design.\n\n## Edge cases\n- `max_attempts` below 1 is rejected with `ValueError`.\n\n## Acceptance criteria\n- Three attempts by default.\n";

    #[test]
    fn design_section_is_parsed_and_shared_by_every_per_file_call() {
        let s = parse_spec(DESIGN_SPEC);
        assert!(s.design.starts_with("### Elements"), "{}", s.design);
        assert!(s.design.contains("classDiagram") && s.design.contains("Policy object instead of loose arguments"));
        assert!(!s.summary.contains("RetryPolicy"), "the design must not leak into the summary");
        assert_eq!(s.files.len(), 2);
        assert!(s.warnings.is_empty(), "a Mermaid class diagram is not code: {:?}", s.warnings);
        for f in ["client.py", "retry.py"] {
            let p = per_file_spec(&s, f);
            let (summary, design, changes) = (
                p.find("## Summary").unwrap(),
                p.find("## Design").unwrap(),
                p.find("## Changes for").unwrap(),
            );
            assert!(summary < design && design < changes, "{p}");
            assert!(p.contains("`delay(attempt: int) -> float`"), "{f} must see the shared signatures");
        }
        // Without a Design section nothing changes.
        assert!(!per_file_spec(&parse_spec(SPEC), "src/http/retry.ts").contains("## Design"));
        // Code is still flagged, also inside the Design section.
        let with_code = DESIGN_SPEC.replace("```mermaid\nclassDiagram", "```python\nclass RetryPolicy:");
        assert!(parse_spec(&with_code).warnings.iter().any(|w| w.contains("contains code")));
    }

    #[test]
    fn expressions_in_code_syntax_are_flagged() {
        // Real leaks from a Claude spec (2026-11-15) ...
        for e in [
            "min(self.max_backoff, self.backoff_factor * 2 ** attempt)",
            "self._sleep(self._backoff_delay(attempt))",
            "sleep(base_delay * 2 ** counter)",
            "delay * factor",
            "len(items) - 1",
        ] {
            assert!(is_code_expression(e), "{e}");
        }
        // ... and what a design spec legitimately writes inline.
        for ok in [
            "max_retries + 1",
            "__init__(self, base_url: str, timeout: float = 10, sleep: Callable[[float], None] = time.sleep) -> None",
            "retry_with_backoff(fn, retries=3, base_delay=0.5, should_retry=lambda exc: True, sleep=time.sleep)",
            "frozenset({429, 500, 502, 503, 504})",
            "RETRY_STATUSES: frozenset[int]",
            "urllib.error.HTTPError",
            "_backoff_delay(self, retry_number: int) -> float",
            "Content-Type: application/json",
            "0.5, 1.0, 2.0",
        ] {
            assert!(!is_code_expression(ok), "{ok}");
        }
        let s = parse_spec(
            "## Files\n- `a.py` (modify)\n## Changes\n### `a.py`\nWait `self._sleep(self._backoff_delay(n))` between at most `max_retries + 1` attempts.\n",
        );
        assert!(
            s.warnings
                .iter()
                .any(|w| w.contains("1 expression(s) in code syntax (`self._sleep(self._backoff_delay(n))`)")),
            "{:?}",
            s.warnings
        );
    }

    #[test]
    fn verification_section_lists_commands() {
        let s = parse_spec(&format!(
            "{SPEC}\n## Verification\n- build: `cargo build`\n- **test**: `cargo test -- --nocapture`\n- run: `cargo run -- --help` (timeout 20s, expect \"Usage\")\n- lint — `cargo clippy`\n- deploy: `rm -rf /`\n- run the game and look at it\n"
        ));
        assert!(s.verification_declared);
        let got: Vec<(&str, &str, Option<u64>, Option<&str>)> = s
            .verification
            .iter()
            .map(|v| (v.kind.word(), v.command.as_str(), v.timeout_secs, v.expect.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("build", "cargo build", None, None),
                ("test", "cargo test -- --nocapture", None, None),
                ("run", "cargo run -- --help", Some(20), Some("Usage")),
                ("build", "cargo clippy", None, None),
            ]
        );
        assert!(
            s.warnings.iter().any(|w| w.contains("Unknown verification step `deploy`")),
            "{:?}",
            s.warnings
        );
        assert!(
            s.warnings
                .iter()
                .any(|w| w.contains("Could not read a command") && w.contains("look at it"))
        );
        assert!(
            !s.warnings.iter().any(|w| w.contains("expression")),
            "commands are not code expressions: {:?}",
            s.warnings
        );
        assert!(!s.mentioned_paths.iter().any(|p| p.contains("cargo")), "{:?}", s.mentioned_paths);
        // The other sections are unaffected.
        assert_eq!(s.acceptance, "- 3 attempts max.");
        assert_eq!(s.files.len(), 3);
    }

    #[test]
    fn verification_can_be_declared_empty_or_left_out() {
        let none = parse_spec(&format!("{SPEC}\n## Verification\n- None (static page).\n"));
        assert!(none.verification_declared && none.verification.is_empty());
        assert!(none.warnings.is_empty(), "{:?}", none.warnings);
        let empty = parse_spec(&format!("{SPEC}\n## Verification\n"));
        assert!(empty.verification_declared && empty.verification.is_empty());
        let absent = parse_spec(SPEC);
        assert!(!absent.verification_declared && absent.verification.is_empty());
        // A command that runs a project file still mentions nothing as a path.
        let py = parse_spec(&format!(
            "{SPEC}\n## Verification\n- test: `python -m pytest tests/test_retry.py`\n"
        ));
        assert_eq!(py.verification[0].command, "python -m pytest tests/test_retry.py");
    }

    #[test]
    fn level_one_headings_work_too() {
        let s = parse_spec("# Summary\nS\n# Files\n- `a.rs` (new)\n# Changes\n## a.rs\nbody\n# Acceptance criteria\nok\n");
        assert_eq!(s.files[0].action, FileAction::Create);
        assert_eq!(s.changes, vec![("a.rs".to_string(), "body".to_string())]);
        assert_eq!(s.acceptance, "ok");
    }
}
