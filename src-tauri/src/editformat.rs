//! Aider-style SEARCH/REPLACE edit blocks: parsing, validation and application.
//!
//! Format produced by the local model:
//!
//! ```text
//! path/to/file.ext
//! <<<<<<< SEARCH
//! exact lines from the current file
//! =======
//! replacement lines
//! >>>>>>> REPLACE
//! ```
//!
//! * An empty SEARCH section creates a new file whose whole content is the REPLACE section.
//! * Matching is exact and line-anchored. The only normalisation is line endings (CRLF vs LF),
//!   because the model cannot see them. Whitespace, indentation and blank lines must match.
//! * A SEARCH that matches more than one place is rejected (ambiguous).
//! * Blocks are applied in order; later blocks see the result of earlier ones.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

static SEARCH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*<{5,9} ?SEARCH\s*$").unwrap());
static DIVIDER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*={5,9}\s*$").unwrap());
static REPLACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*>{5,9} ?REPLACE\s*$").unwrap());
static FENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(`{3,}|~{3,})").unwrap());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditBlock {
    /// Path as written by the model, normalised to forward slashes without a leading `./`.
    pub path: String,
    pub search: String,
    pub replace: String,
    /// 0-based ordinal of the block in the output.
    pub index: usize,
    /// 1-based line in the model output where `<<<<<<< SEARCH` appeared.
    pub line: usize,
}

impl EditBlock {
    pub fn is_create(&self) -> bool {
        self.search.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseIssue {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseResult {
    pub blocks: Vec<EditBlock>,
    pub issues: Vec<ParseIssue>,
}

/// Clean a candidate header line and return it if it plausibly names a file path.
pub fn clean_path_line(line: &str) -> Option<String> {
    let mut s = line.trim();
    if s.is_empty() || FENCE_RE.is_match(s) {
        return None;
    }
    // Strip markdown heading markers, list bullets and common labels.
    s = s.trim_start_matches('#').trim();
    for prefix in ["- ", "* "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim();
        }
    }
    let lower = s.to_ascii_lowercase();
    for label in ["file:", "path:", "filename:", "file path:", "new file:"] {
        if lower.starts_with(label) {
            s = s[label.len()..].trim();
            break;
        }
    }
    let s = s.trim_matches(|c| c == '`' || c == '*' || c == '"' || c == '\'' || c == ':' || c == ' ');
    if s.is_empty() || s.len() > 260 {
        return None;
    }
    // Reject prose: a path has no spaces around words like "the", and usually has a dot or slash.
    let looks_like_path = (s.contains('.') || s.contains('/') || s.contains('\\') || is_known_extensionless(s))
        && !s.contains("  ")
        && s.split_whitespace().count() <= 2
        && !s.ends_with('.')
        && !s.starts_with("<<<")
        && !s.starts_with(">>>")
        && !s.starts_with("===");
    if !looks_like_path {
        return None;
    }
    Some(normalize_block_path(s))
}

fn is_known_extensionless(s: &str) -> bool {
    let name = s.rsplit(['/', '\\']).next().unwrap_or(s);
    matches!(
        name,
        "Makefile" | "Dockerfile" | "LICENSE" | "Procfile" | "Gemfile" | "Rakefile" | "Justfile" | "justfile" | "CODEOWNERS"
    )
}

pub fn normalize_block_path(p: &str) -> String {
    let mut s = p.trim().replace('\\', "/");
    while let Some(rest) = s.strip_prefix("./") {
        s = rest.to_string();
    }
    s
}

/// Parse SEARCH/REPLACE blocks out of raw model output (after reasoning has been stripped).
pub fn parse_blocks(output: &str) -> ParseResult {
    let text = output.replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let mut result = ParseResult::default();
    let mut current_path: Option<String> = None;
    // Lines seen since the end of the previous block (header candidates).
    let mut pending: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if SEARCH_RE.is_match(line) {
            let start_line = i + 1;
            let header = pending.iter().rev().find_map(|l| clean_path_line(l));
            let path = match header.or_else(|| current_path.clone()) {
                Some(p) => p,
                None => {
                    result.issues.push(ParseIssue {
                        line: start_line,
                        message: "SEARCH block has no file path line before it".into(),
                    });
                    String::new()
                }
            };
            // Collect SEARCH section.
            let mut search = Vec::new();
            let mut j = i + 1;
            let mut found_divider = false;
            while j < lines.len() {
                if DIVIDER_RE.is_match(lines[j]) {
                    found_divider = true;
                    break;
                }
                if SEARCH_RE.is_match(lines[j]) || REPLACE_RE.is_match(lines[j]) {
                    break;
                }
                search.push(lines[j]);
                j += 1;
            }
            if !found_divider {
                result.issues.push(ParseIssue {
                    line: start_line,
                    message: format!("SEARCH block for `{path}` is missing the ======= divider"),
                });
                pending.clear();
                i = j.max(i + 1);
                continue;
            }
            // Collect REPLACE section.
            let mut replace = Vec::new();
            let mut k = j + 1;
            let mut found_end = false;
            while k < lines.len() {
                if REPLACE_RE.is_match(lines[k]) {
                    found_end = true;
                    break;
                }
                if SEARCH_RE.is_match(lines[k]) {
                    break;
                }
                replace.push(lines[k]);
                k += 1;
            }
            if !found_end {
                result.issues.push(ParseIssue {
                    line: start_line,
                    message: format!("SEARCH/REPLACE block for `{path}` is missing the >>>>>>> REPLACE marker"),
                });
                pending.clear();
                i = k;
                continue;
            }
            if !path.is_empty() {
                result.blocks.push(EditBlock {
                    path: path.clone(),
                    search: search.join("\n"),
                    replace: replace.join("\n"),
                    index: result.blocks.len(),
                    line: start_line,
                });
                current_path = Some(path);
            }
            pending.clear();
            i = k + 1;
            continue;
        }
        if FENCE_RE.is_match(line) {
            // A closing fence ends the scope of an inherited path only if a new header follows,
            // so we simply keep it out of the header candidates.
        } else if !line.trim().is_empty() {
            pending.push(line);
            if pending.len() > 8 {
                pending.remove(0);
            }
        }
        i += 1;
    }
    result
}

// ---------------------------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------------------------

/// A file's text split into lines with its line-ending style remembered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Doc {
    lines: Vec<String>,
    crlf: bool,
    trailing_newline: bool,
}

impl Doc {
    fn parse(text: &str) -> Doc {
        let crlf = text.contains("\r\n");
        let norm = text.replace("\r\n", "\n");
        let trailing_newline = norm.ends_with('\n');
        let body = if trailing_newline { &norm[..norm.len() - 1] } else { &norm[..] };
        let lines = if body.is_empty() && !trailing_newline {
            Vec::new()
        } else {
            body.split('\n').map(str::to_string).collect()
        };
        Doc {
            lines,
            crlf,
            trailing_newline,
        }
    }

    fn render(&self) -> String {
        let nl = if self.crlf { "\r\n" } else { "\n" };
        let mut s = self.lines.join(nl);
        if self.trailing_newline && !self.lines.is_empty() {
            s.push_str(nl);
        }
        s
    }
}

fn split_block_lines(text: &str) -> Vec<String> {
    let norm = text.replace("\r\n", "\n");
    if norm.is_empty() {
        return Vec::new();
    }
    norm.split('\n').map(str::to_string).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockErrorKind {
    NotFound,
    Ambiguous,
    FileMissing,
    FileExists,
    PathRejected,
    NotAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockOutcome {
    pub index: usize,
    pub path: String,
    pub ok: bool,
    pub kind: Option<BlockErrorKind>,
    pub message: Option<String>,
    /// For NotFound: the closest region of the current file (1-based start line, text).
    pub closest: Option<(usize, String)>,
    pub search_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedFile {
    pub path: String,
    /// Content when the run started; `None` if the file did not exist.
    pub original: Option<String>,
    pub proposed: String,
}

/// Source of file contents for the applier. Returns `Ok(None)` when the file does not exist and
/// `Err(msg)` when the path is not allowed (outside the project, etc.).
pub trait FileSource {
    fn read(&self, path: &str) -> Result<Option<String>, String>;
}

impl FileSource for BTreeMap<String, String> {
    fn read(&self, path: &str) -> Result<Option<String>, String> {
        Ok(self.get(path).cloned())
    }
}

/// Working set of files being edited during one run. Keeps the original content for diffs.
#[derive(Debug, Clone, Default)]
pub struct Workspace {
    files: BTreeMap<String, (Option<String>, Option<Doc>)>,
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    fn load<'a>(&'a mut self, path: &str, src: &dyn FileSource) -> Result<&'a mut (Option<String>, Option<Doc>), String> {
        if !self.files.contains_key(path) {
            let content = src.read(path)?;
            let doc = content.as_deref().map(Doc::parse);
            self.files.insert(path.to_string(), (content, doc));
        }
        Ok(self.files.get_mut(path).unwrap())
    }

    /// Current (edited) text of a file, if it exists in the workspace.
    pub fn current_text(&self, path: &str) -> Option<String> {
        self.files.get(path).and_then(|(_, d)| d.as_ref().map(Doc::render))
    }

    /// Files whose content changed (or were created).
    pub fn proposed(&self) -> Vec<ProposedFile> {
        self.files
            .iter()
            .filter_map(|(path, (orig, doc))| {
                let doc = doc.as_ref()?;
                let text = doc.render();
                if orig.as_deref() == Some(text.as_str()) {
                    return None;
                }
                Some(ProposedFile {
                    path: path.clone(),
                    original: orig.clone(),
                    proposed: text,
                })
            })
            .collect()
    }

    /// Apply blocks in order. `allowed` optionally restricts which paths may be edited.
    pub fn apply(&mut self, blocks: &[EditBlock], src: &dyn FileSource, allowed: Option<&[String]>) -> Vec<BlockOutcome> {
        blocks.iter().map(|b| self.apply_one(b, src, allowed)).collect()
    }

    fn apply_one(&mut self, block: &EditBlock, src: &dyn FileSource, allowed: Option<&[String]>) -> BlockOutcome {
        let preview: String = block.search.lines().take(3).collect::<Vec<_>>().join("\n");
        let fail = |kind: BlockErrorKind, msg: String, closest: Option<(usize, String)>| BlockOutcome {
            index: block.index,
            path: block.path.clone(),
            ok: false,
            kind: Some(kind),
            message: Some(msg),
            closest,
            search_preview: preview.clone(),
        };
        if let Some(allowed) = allowed
            && !allowed.iter().any(|a| a.eq_ignore_ascii_case(&block.path))
        {
            return fail(
                BlockErrorKind::NotAllowed,
                format!(
                    "`{}` is not one of the files this call may edit ({}).",
                    block.path,
                    allowed.join(", ")
                ),
                None,
            );
        }
        let entry = match self.load(&block.path, src) {
            Ok(e) => e,
            Err(msg) => return fail(BlockErrorKind::PathRejected, msg, None),
        };
        let search_lines = split_block_lines(&block.search);
        let replace_lines = split_block_lines(&block.replace);

        if block.is_create() {
            let has_content = entry.1.as_ref().is_some_and(|d| d.lines.iter().any(|l| !l.trim().is_empty()));
            if has_content {
                return fail(
                    BlockErrorKind::FileExists,
                    format!(
                        "SEARCH is empty, which means \"create a new file\", but `{}` already exists. \
                         To edit an existing file, put the exact lines to change in SEARCH.",
                        block.path
                    ),
                    None,
                );
            }
            let crlf = entry.1.as_ref().is_some_and(|d| d.crlf);
            entry.1 = Some(Doc {
                lines: replace_lines,
                crlf,
                trailing_newline: true,
            });
            return ok_outcome(block, preview);
        }

        let Some(doc) = &mut entry.1 else {
            return fail(
                BlockErrorKind::FileMissing,
                format!(
                    "`{}` does not exist. To create it, use an empty SEARCH section and put the whole file in REPLACE.",
                    block.path
                ),
                None,
            );
        };
        // Leading/trailing blank lines in SEARCH are part of the match. Find all exact matches.
        let matches = find_all(&doc.lines, &search_lines);
        match matches.len() {
            1 => {
                let at = matches[0];
                doc.lines.splice(at..at + search_lines.len(), replace_lines);
                ok_outcome(block, preview)
            }
            0 => {
                let closest = closest_region(&doc.lines, &search_lines);
                let mut hint = match &closest {
                    Some((start, _)) => format!(
                        "SEARCH text was not found in `{}`. The most similar lines start at line {start}; \
                         copy them exactly (including whitespace and indentation).",
                        block.path
                    ),
                    None => format!("SEARCH text was not found in `{}`.", block.path),
                };
                // Diagnose (never apply) the two most common near-misses so the retry is precise.
                if let Some(d) = diagnose_near_miss(&doc.lines, &search_lines) {
                    hint.push(' ');
                    hint.push_str(&d);
                }
                fail(BlockErrorKind::NotFound, hint, closest)
            }
            n => {
                let at: Vec<String> = matches.iter().take(5).map(|m| (m + 1).to_string()).collect();
                fail(
                    BlockErrorKind::Ambiguous,
                    format!(
                        "SEARCH text matches {n} places in `{}` (lines {}). Include more surrounding lines so it matches exactly one place.",
                        block.path,
                        at.join(", ")
                    ),
                    None,
                )
            }
        }
    }
}

/// Explain why an exact match failed when the SEARCH would match uniquely after trimming blank
/// lines at its edges, or after ignoring leading/trailing whitespace on each line. Used only for
/// the error message; matching itself stays exact.
fn diagnose_near_miss(hay: &[String], needle: &[String]) -> Option<String> {
    let is_blank = |l: &String| l.trim().is_empty();
    let lead = needle.iter().take_while(|l| is_blank(l)).count();
    let trail = needle.iter().rev().take_while(|l| is_blank(l)).count();
    if lead + trail > 0 && lead + trail < needle.len() {
        let core = &needle[lead..needle.len() - trail];
        let m = find_all(hay, core);
        if m.len() == 1 {
            let at = m[0];
            let file_lead = hay[..at].iter().rev().take_while(|l| is_blank(l)).count();
            let file_trail = hay[at + core.len()..].iter().take_while(|l| is_blank(l)).count();
            let mut parts = Vec::new();
            if lead > 0 {
                parts.push(format!(
                    "starts with {lead} blank line(s) but the file has {file_lead} blank line(s) before line {}",
                    at + 1
                ));
            }
            if trail > 0 {
                parts.push(format!(
                    "ends with {trail} blank line(s) but the file has {file_trail} after line {}",
                    at + core.len()
                ));
            }
            return Some(format!(
                "Your SEARCH {}. Start and end SEARCH on a non-blank line, or copy the blank lines exactly.",
                parts.join(" and ")
            ));
        }
    }
    let trimmed = |v: &[String]| v.iter().map(|l| l.trim().to_string()).collect::<Vec<_>>();
    let m = find_all(&trimmed(hay), &trimmed(needle));
    if m.len() == 1 {
        return Some(format!(
            "Your SEARCH matches lines {}-{} only if indentation/whitespace is ignored: copy the indentation exactly as it appears in the file.",
            m[0] + 1,
            m[0] + needle.len()
        ));
    }
    None
}

fn ok_outcome(block: &EditBlock, preview: String) -> BlockOutcome {
    BlockOutcome {
        index: block.index,
        path: block.path.clone(),
        ok: true,
        kind: None,
        message: None,
        closest: None,
        search_preview: preview,
    }
}

fn find_all(hay: &[String], needle: &[String]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return Vec::new();
    }
    (0..=hay.len() - needle.len())
        .filter(|&i| hay[i..i + needle.len()] == *needle)
        .collect()
}

/// Find the window of the file most similar to the SEARCH lines (whitespace-insensitive line
/// equality count). Used only to build a helpful error message, never to apply.
fn closest_region(hay: &[String], needle: &[String]) -> Option<(usize, String)> {
    if needle.is_empty() || hay.is_empty() {
        return None;
    }
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let hay_n: Vec<String> = hay.iter().map(|l| norm(l)).collect();
    let needle_n: Vec<String> = needle.iter().map(|l| norm(l)).collect();
    let n = needle.len().min(hay.len());
    let significant = needle_n.iter().filter(|l| !l.is_empty()).count().max(1) as f64;
    // Fuzzy per-line similarity is O(file × search) char diffs; fall back to exact (normalised)
    // equality for very large inputs.
    let fuzzy = hay.len() * n <= 200_000;
    let line_score = |a: &str, b: &str| -> f64 {
        if b.is_empty() {
            0.0
        } else if a == b {
            1.0
        } else if fuzzy && !a.is_empty() {
            let r = similar::TextDiff::from_chars(a, b).ratio() as f64;
            if r >= 0.6 { r } else { 0.0 }
        } else {
            0.0
        }
    };
    let mut best: Option<(f64, usize)> = None; // (score, start)
    for start in 0..=hay.len() - n {
        let score: f64 = (0..n).map(|k| line_score(&hay_n[start + k], &needle_n[k])).sum();
        if score > 0.0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, start));
        }
    }
    let (score, start) = best?;
    if score / significant < 0.5 {
        return None; // too dissimilar to be a useful hint
    }
    let end = (start + n).min(hay.len());
    Some((start + 1, hay[start..end].join("\n")))
}

/// Build the follow-up message sent to the local model when some blocks failed.
pub fn retry_message(
    outcomes: &[BlockOutcome],
    blocks: &[EditBlock],
    parse_issues: &[ParseIssue],
    ws: &Workspace,
    include_files: bool,
) -> String {
    let mut msg = String::from("Some of your SEARCH/REPLACE blocks could not be applied.\n\n");
    for issue in parse_issues {
        msg.push_str(&format!("- Format problem near line {}: {}\n", issue.line, issue.message));
    }
    let failed: Vec<&BlockOutcome> = outcomes.iter().filter(|o| !o.ok).collect();
    for o in &failed {
        let block = blocks.iter().find(|b| b.index == o.index);
        msg.push_str(&format!("\n## Failed block {} for `{}`\n", o.index + 1, o.path));
        msg.push_str(o.message.as_deref().unwrap_or("failed"));
        msg.push('\n');
        if let Some(b) = block
            && !b.search.is_empty()
        {
            msg.push_str(&format!("\nYour SEARCH section was:\n{}\n", fenced(&b.search)));
        }
        if let Some((line, text)) = &o.closest {
            msg.push_str(&format!(
                "\nThe closest lines in the current file (starting at line {line}) are:\n{}\n",
                fenced(text)
            ));
        }
    }
    let ok_count = outcomes.iter().filter(|o| o.ok).count();
    if ok_count > 0 {
        msg.push_str(&format!(
            "\nThe other {ok_count} block(s) were applied successfully and are kept. Do NOT repeat them.\n"
        ));
    }
    if include_files {
        let mut paths: Vec<&str> = failed.iter().map(|o| o.path.as_str()).collect();
        paths.sort();
        paths.dedup();
        for p in paths {
            if let Some(text) = ws.current_text(p) {
                msg.push_str(&format!(
                    "\nCurrent content of `{p}` (after the successful edits):\n{}\n",
                    fenced(&text)
                ));
            }
        }
    }
    msg.push_str(
        "\nReply with corrected SEARCH/REPLACE blocks for the failed edits only, in the same format \
         (file path line, <<<<<<< SEARCH, =======, >>>>>>> REPLACE). Copy SEARCH lines exactly from the current file. \
         Output nothing else.",
    );
    msg
}

/// Wrap text in a markdown fence that is longer than any backtick run inside it.
pub fn fenced(text: &str) -> String {
    fenced_lang(text, "")
}

pub fn fenced_lang(text: &str, lang: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    let body = text.strip_suffix('\n').unwrap_or(text);
    format!("{fence}{lang}\n{body}\n{fence}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(files: &[(&str, &str)]) -> BTreeMap<String, String> {
        files.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect()
    }

    #[test]
    fn parses_basic_block() {
        let out = "src/a.py\n<<<<<<< SEARCH\nx = 1\n=======\nx = 2\n>>>>>>> REPLACE\n";
        let r = parse_blocks(out);
        assert!(r.issues.is_empty());
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].path, "src/a.py");
        assert_eq!(r.blocks[0].search, "x = 1");
        assert_eq!(r.blocks[0].replace, "x = 2");
        assert_eq!(r.blocks[0].line, 2);
    }

    #[test]
    fn parses_fenced_blocks_and_header_variants() {
        let out = "Here are the changes.\n\n**`src/http/client.ts`**\n```ts\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n```\n\n### File: .\\lib\\util.rs\n```rust\n<<<<<<< SEARCH\nc\n=======\nd\n>>>>>>> REPLACE\n```\n";
        let r = parse_blocks(out);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        assert_eq!(
            r.blocks.iter().map(|b| b.path.as_str()).collect::<Vec<_>>(),
            vec!["src/http/client.ts", "lib/util.rs"]
        );
    }

    #[test]
    fn inherits_path_for_consecutive_blocks() {
        let out = "a.txt\n<<<<<<< SEARCH\n1\n=======\n2\n>>>>>>> REPLACE\n<<<<<<< SEARCH\n3\n=======\n4\n>>>>>>> REPLACE\n";
        let r = parse_blocks(out);
        assert_eq!(r.blocks.len(), 2);
        assert_eq!(r.blocks[1].path, "a.txt");
        assert_eq!(r.blocks[1].index, 1);
    }

    #[test]
    fn handles_crlf_output_and_marker_lengths() {
        let out = "a.txt\r\n<<<<<<<< SEARCH\r\nold\r\n========\r\nnew\r\n>>>>>>>> REPLACE\r\n";
        let r = parse_blocks(out);
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].search, "old");
    }

    #[test]
    fn reports_missing_path_and_unterminated() {
        let r = parse_blocks("<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n");
        assert_eq!(r.blocks.len(), 0);
        assert_eq!(r.issues.len(), 1);
        assert!(r.issues[0].message.contains("no file path"));

        let r = parse_blocks("a.txt\n<<<<<<< SEARCH\na\n=======\nb\n");
        assert_eq!(r.blocks.len(), 0);
        assert!(r.issues[0].message.contains("REPLACE marker"));

        let r = parse_blocks("a.txt\n<<<<<<< SEARCH\na\nb\n");
        assert!(r.issues[0].message.contains("divider"));
    }

    #[test]
    fn prose_is_not_a_path() {
        assert_eq!(clean_path_line("Here is the updated code for the client."), None);
        assert_eq!(clean_path_line("```python"), None);
        assert_eq!(clean_path_line("Makefile"), Some("Makefile".into()));
        assert_eq!(clean_path_line("## `src/main.rs`:"), Some("src/main.rs".into()));
    }

    #[test]
    fn replace_section_may_contain_divider_like_lines() {
        let out = "README.md\n<<<<<<< SEARCH\nTitle\n=======\nTitle\n=======\nMore\n>>>>>>> REPLACE\n";
        let r = parse_blocks(out);
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].replace, "Title\n=======\nMore");
    }

    #[test]
    fn applies_exact_match_preserving_crlf_and_trailing_newline() {
        let files = src(&[("a.py", "def f():\r\n    return 1\r\n\r\nprint(f())\r\n")]);
        let blocks = parse_blocks("a.py\n<<<<<<< SEARCH\n    return 1\n=======\n    return 2\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        let out = ws.apply(&blocks, &files, None);
        assert!(out[0].ok, "{:?}", out[0]);
        let p = ws.proposed();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].proposed, "def f():\r\n    return 2\r\n\r\nprint(f())\r\n");
        assert_eq!(p[0].original.as_deref(), Some("def f():\r\n    return 1\r\n\r\nprint(f())\r\n"));
    }

    #[test]
    fn whitespace_must_match_exactly() {
        let files = src(&[("a.py", "if x:\n    y()\n")]);
        let blocks = parse_blocks("a.py\n<<<<<<< SEARCH\nif x:\n  y()\n=======\nif x:\n  z()\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        let out = ws.apply(&blocks, &files, None);
        assert!(!out[0].ok);
        assert_eq!(out[0].kind, Some(BlockErrorKind::NotFound));
        let (line, text) = out[0].closest.clone().expect("closest hint");
        assert_eq!(line, 1);
        assert_eq!(text, "if x:\n    y()");
        assert!(ws.proposed().is_empty());
    }

    #[test]
    fn closest_hint_is_fuzzy() {
        let files = src(&[(
            "c.py",
            "class C:
    def get(self, path, params=None):
        return 1
",
        )]);
        let blocks = parse_blocks(
            "c.py
<<<<<<< SEARCH
  def get(self, path):
=======
    def get(self, path, h=None):
>>>>>>> REPLACE
",
        )
        .blocks;
        let mut ws = Workspace::new();
        let out = ws.apply(&blocks, &files, None);
        let (line, text) = out[0].closest.clone().expect("fuzzy closest hint");
        assert_eq!(line, 2);
        assert_eq!(text, "    def get(self, path, params=None):");
    }

    #[test]
    fn near_miss_diagnostics_explain_blank_lines_and_indentation() {
        // Real gpt-oss failure: SEARCH began with 3 blank lines, the file has 2 before `class`.
        let file = "import json\nimport urllib.request\n\n\nclass HttpClient:\n    def get(self):\n        return 1\n";
        let files = src(&[("c.py", file)]);
        let out = "c.py\n<<<<<<< SEARCH\n\n\n\nclass HttpClient:\n=======\nX = 1\n\n\nclass HttpClient:\n>>>>>>> REPLACE\n";
        let mut ws = Workspace::new();
        let res = ws.apply(&parse_blocks(out).blocks, &files, None);
        let msg = res[0].message.clone().unwrap();
        assert!(!res[0].ok, "matching stays exact");
        assert!(
            msg.contains("starts with 3 blank line(s) but the file has 2 blank line(s) before line 5"),
            "{msg}"
        );

        let out =
            "c.py\n<<<<<<< SEARCH\n  def get(self):\n      return 1\n=======\n    def get(self):\n        return 2\n>>>>>>> REPLACE\n";
        let mut ws = Workspace::new();
        let res = ws.apply(&parse_blocks(out).blocks, &files, None);
        let msg = res[0].message.clone().unwrap();
        assert!(!res[0].ok);
        assert!(msg.contains("matches lines 6-7 only if indentation/whitespace is ignored"), "{msg}");
    }

    #[test]
    fn rejects_ambiguous_match() {
        let files = src(&[("a.txt", "x\ny\nx\ny\n")]);
        let blocks = parse_blocks("a.txt\n<<<<<<< SEARCH\nx\n=======\nz\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        let out = ws.apply(&blocks, &files, None);
        assert_eq!(out[0].kind, Some(BlockErrorKind::Ambiguous));
        assert!(out[0].message.as_ref().unwrap().contains("lines 1, 3"));
    }

    #[test]
    fn sequential_blocks_see_previous_edits() {
        let files = src(&[("a.txt", "one\ntwo\nthree\n")]);
        let out = "a.txt\n<<<<<<< SEARCH\ntwo\n=======\n2\n>>>>>>> REPLACE\n<<<<<<< SEARCH\n2\nthree\n=======\n2\n3\n>>>>>>> REPLACE\n";
        let blocks = parse_blocks(out).blocks;
        let mut ws = Workspace::new();
        let res = ws.apply(&blocks, &files, None);
        assert!(res.iter().all(|o| o.ok), "{res:?}");
        assert_eq!(ws.current_text("a.txt").unwrap(), "one\n2\n3\n");
    }

    #[test]
    fn creates_new_file_with_empty_search() {
        let files = src(&[]);
        let blocks = parse_blocks("src/new.rs\n<<<<<<< SEARCH\n=======\nfn main() {}\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        let res = ws.apply(&blocks, &files, None);
        assert!(res[0].ok);
        let p = ws.proposed();
        assert_eq!(p[0].original, None);
        assert_eq!(p[0].proposed, "fn main() {}\n");
    }

    #[test]
    fn empty_search_on_existing_file_is_rejected() {
        let files = src(&[("a.txt", "hello\n")]);
        let blocks = parse_blocks("a.txt\n<<<<<<< SEARCH\n=======\nbye\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        let res = ws.apply(&blocks, &files, None);
        assert_eq!(res[0].kind, Some(BlockErrorKind::FileExists));
    }

    #[test]
    fn search_on_missing_file_is_rejected() {
        let blocks = parse_blocks("nope.txt\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        let res = ws.apply(&blocks, &src(&[]), None);
        assert_eq!(res[0].kind, Some(BlockErrorKind::FileMissing));
    }

    #[test]
    fn deletion_with_empty_replace() {
        let files = src(&[("a.txt", "keep\ndrop\nkeep2\n")]);
        let blocks = parse_blocks("a.txt\n<<<<<<< SEARCH\ndrop\n=======\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        assert!(ws.apply(&blocks, &files, None)[0].ok);
        assert_eq!(ws.current_text("a.txt").unwrap(), "keep\nkeep2\n");
    }

    #[test]
    fn file_without_trailing_newline_stays_without() {
        let files = src(&[("a.txt", "a\nb")]);
        let blocks = parse_blocks("a.txt\n<<<<<<< SEARCH\nb\n=======\nc\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        assert!(ws.apply(&blocks, &files, None)[0].ok);
        assert_eq!(ws.current_text("a.txt").unwrap(), "a\nc");
    }

    #[test]
    fn allowed_list_restricts_targets() {
        let files = src(&[("a.txt", "x\n"), ("b.txt", "y\n")]);
        let blocks = parse_blocks("b.txt\n<<<<<<< SEARCH\ny\n=======\nz\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        let res = ws.apply(&blocks, &files, Some(&["a.txt".to_string()]));
        assert_eq!(res[0].kind, Some(BlockErrorKind::NotAllowed));
    }

    #[test]
    fn path_rejection_from_source_is_reported() {
        struct Deny;
        impl FileSource for Deny {
            fn read(&self, _p: &str) -> Result<Option<String>, String> {
                Err("outside project".into())
            }
        }
        let blocks = parse_blocks("../evil.txt\n<<<<<<< SEARCH\n=======\nx\n>>>>>>> REPLACE\n").blocks;
        let mut ws = Workspace::new();
        let res = ws.apply(&blocks, &Deny, None);
        assert_eq!(res[0].kind, Some(BlockErrorKind::PathRejected));
        assert!(ws.proposed().is_empty());
    }

    #[test]
    fn retry_message_lists_failures_and_current_content() {
        let files = src(&[("a.txt", "one\ntwo\n")]);
        let out = "a.txt\n<<<<<<< SEARCH\none\n=======\n1\n>>>>>>> REPLACE\n<<<<<<< SEARCH\nTWO\n=======\n2\n>>>>>>> REPLACE\n";
        let parsed = parse_blocks(out);
        let mut ws = Workspace::new();
        let res = ws.apply(&parsed.blocks, &files, None);
        let msg = retry_message(&res, &parsed.blocks, &parsed.issues, &ws, true);
        assert!(msg.contains("Failed block 2"));
        assert!(msg.contains("Do NOT repeat them"));
        assert!(msg.contains("Current content of `a.txt`"));
        assert!(msg.contains("1\ntwo"));
    }

    #[test]
    fn fence_is_longer_than_inner_backticks() {
        let f = fenced("a ```` b");
        assert!(f.starts_with("`````\n"));
    }
}
