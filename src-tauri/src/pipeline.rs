//! The implement step: spec → local model → validated SEARCH/REPLACE edits → proposed files.

use crate::budget::{self, BudgetInput, BudgetPlan, FileTokens, SplitMode, estimate_tokens};
use crate::config::Profile;
use crate::editformat::{BlockOutcome, ParseIssue, ProposedFile, Workspace, fenced_lang, parse_blocks, retry_message};
use crate::events::{Sink, emit};
use crate::llm::{ChatMessage, Delta, Endpoint, LiveStats, build_body, stream_chat};
use crate::pathsafe::normalize_rel;
use crate::project::{ProjectSource, read_text};
use crate::reasoning::strip_reasoning;
use crate::spec::{FileAction, ParsedSpec, parse_spec, per_file_spec};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplementRequest {
    pub run_id: String,
    pub project_root: String,
    pub spec: String,
    /// Files ticked in the file tree (read-only context unless the spec edits them).
    #[serde(default)]
    pub context_files: Vec<String>,
    /// `None` → follow the budget recommendation.
    #[serde(default)]
    pub mode: Option<SplitMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedFile {
    pub path: String,
    pub exists: bool,
    pub target: bool,
    pub action: Option<FileAction>,
    pub tokens: u32,
    pub error: Option<String>,
}

/// Everything known before the local model runs: parsed spec, files, and the budget plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prepared {
    pub spec: ParsedSpec,
    pub files: Vec<PreparedFile>,
    pub budget: BudgetPlan,
    pub warnings: Vec<String>,
    #[serde(skip)]
    contents: Vec<(String, Option<String>)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptReport {
    pub attempt: u32,
    pub raw_output: String,
    pub reasoning: String,
    pub blocks: usize,
    pub outcomes: Vec<BlockOutcome>,
    pub parse_issues: Vec<ParseIssue>,
    pub missing_targets: Vec<String>,
    pub ok: bool,
    pub tok_per_sec: Option<f64>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub duration_ms: u64,
    pub finish_reason: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallReport {
    pub index: usize,
    pub target: Option<String>,
    pub attempts: Vec<AttemptReport>,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplementResult {
    pub run_id: String,
    pub profile_id: String,
    pub profile_name: String,
    pub mode: SplitMode,
    pub budget: BudgetPlan,
    pub files: Vec<ProposedFile>,
    pub calls: Vec<CallReport>,
    pub validation_ok: bool,
    pub cancelled: bool,
    pub error: Option<String>,
    pub warnings: Vec<String>,
    pub duration_ms: u64,
    pub avg_tok_per_sec: Option<f64>,
}

pub fn lang_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "cs" => "csharp",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" | "cxx" => "cpp",
        "json" => "json",
        "toml" => "toml",
        "yml" | "yaml" => "yaml",
        "md" => "markdown",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "sh" | "bash" => "bash",
        "ps1" => "powershell",
        "sql" => "sql",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        _ => "",
    }
}

/// Parse the spec, read the target and context files (safely), and compute the budget plan.
pub fn prepare(root: &Path, spec_text: &str, context_files: &[String], profile: &Profile, system_prompt: &str) -> Prepared {
    let spec = parse_spec(spec_text);
    let mut warnings = spec.warnings.clone();
    let mut files: Vec<PreparedFile> = Vec::new();
    let mut contents: Vec<(String, Option<String>)> = Vec::new();

    let mut add = |path: &str, target: bool, action: Option<FileAction>, files: &mut Vec<PreparedFile>, warnings: &mut Vec<String>| {
        let Ok(p) = normalize_rel(path) else {
            warnings.push(format!("Ignoring unsafe path `{path}`"));
            return;
        };
        if files.iter().any(|f| f.path.eq_ignore_ascii_case(&p)) {
            return;
        }
        match read_text(root, &p) {
            Ok(content) => {
                let exists = content.is_some();
                if target && !exists && action == Some(FileAction::Modify) {
                    warnings.push(format!(
                        "`{p}` is marked (modify) but does not exist; it will be treated as a new file."
                    ));
                }
                if !target && !exists {
                    return; // mentioned path that doesn't exist — not useful as context
                }
                let tokens = content.as_deref().map(estimate_tokens).unwrap_or(0);
                files.push(PreparedFile {
                    path: p.clone(),
                    exists,
                    target,
                    action,
                    tokens,
                    error: None,
                });
                contents.push((p, content));
            }
            Err(e) => {
                warnings.push(format!("Cannot read `{p}`: {e}"));
                files.push(PreparedFile {
                    path: p,
                    exists: false,
                    target,
                    action,
                    tokens: 0,
                    error: Some(e.to_string()),
                });
            }
        }
    };

    for f in &spec.files {
        if f.action == FileAction::Delete {
            warnings.push(format!(
                "The spec asks to delete `{}`. Relay doesn't delete files; remove it yourself if wanted.",
                f.path
            ));
            continue;
        }
        add(&f.path, true, Some(f.action), &mut files, &mut warnings);
    }
    for c in context_files {
        add(c, false, None, &mut files, &mut warnings);
    }
    // Auto-include other files the spec mentions, as read-only context.
    for m in &spec.mentioned_paths {
        add(m, false, None, &mut files, &mut warnings);
    }

    let per_file_spec_tokens = spec
        .files
        .iter()
        .map(|f| (f.path.clone(), estimate_tokens(&per_file_spec(&spec, &f.path))))
        .collect();
    let budget = budget::plan(&BudgetInput {
        context_limit: profile.context_limit,
        max_output: profile.max_output_tokens,
        reasoning_reserve: profile.reasoning.reasoning_reserve,
        system_tokens: estimate_tokens(system_prompt),
        spec_tokens: estimate_tokens(spec_text),
        files: files
            .iter()
            .filter(|f| f.error.is_none())
            .map(|f| FileTokens {
                path: f.path.clone(),
                tokens: f.tokens,
                target: f.target,
            })
            .collect(),
        per_file_spec_tokens,
    });
    warnings.extend(budget.warnings.iter().cloned());
    Prepared {
        spec,
        files,
        budget,
        warnings,
        contents,
    }
}

fn content_of<'a>(prep: &'a Prepared, path: &str) -> Option<&'a Option<String>> {
    prep.contents.iter().find(|(p, _)| p.eq_ignore_ascii_case(path)).map(|(_, c)| c)
}

/// The user message for one local-model call.
pub fn build_user_message(spec_text: &str, targets: &[(String, Option<String>)], context: &[(String, String)]) -> String {
    let mut m = String::from("# Specification\n\n");
    m.push_str(spec_text.trim());
    m.push_str("\n\n# Files to edit\n");
    for (path, content) in targets {
        match content {
            Some(c) => {
                m.push_str(&format!("\n## `{path}`\n{}\n", fenced_lang(c, lang_for(path))));
            }
            None => m.push_str(&format!(
                "\n## `{path}`\n(New file — it does not exist yet. Create it with an empty SEARCH section.)\n"
            )),
        }
    }
    if !context.is_empty() {
        m.push_str("\n# Read-only context (do NOT edit these files)\n");
        for (path, c) in context {
            m.push_str(&format!("\n## `{path}`\n{}\n", fenced_lang(c, lang_for(path))));
        }
    }
    m.push_str("\nNow output the SEARCH/REPLACE blocks that implement the specification.");
    m
}

struct CallPlan {
    target: Option<String>,
    spec_text: String,
    targets: Vec<(String, Option<String>)>,
    context: Vec<(String, String)>,
    allowed: Option<Vec<String>>,
}

fn plan_calls(prep: &Prepared, spec_text: &str, mode: SplitMode) -> Vec<CallPlan> {
    let target_files: Vec<&PreparedFile> = prep.files.iter().filter(|f| f.target && f.error.is_none()).collect();
    let ctx_of = |names: &[String]| -> Vec<(String, String)> {
        names
            .iter()
            .filter_map(|n| content_of(prep, n).and_then(|c| c.clone()).map(|c| (n.clone(), c)))
            .collect()
    };
    match mode {
        SplitMode::Single => {
            let targets: Vec<(String, Option<String>)> = target_files
                .iter()
                .map(|f| (f.path.clone(), content_of(prep, &f.path).cloned().flatten()))
                .collect();
            let allowed = if targets.is_empty() {
                None
            } else {
                Some(targets.iter().map(|(p, _)| p.clone()).collect())
            };
            vec![CallPlan {
                target: None,
                spec_text: spec_text.to_string(),
                targets,
                context: ctx_of(&prep.budget.single.context_files),
                allowed,
            }]
        }
        SplitMode::PerFile => target_files
            .iter()
            .map(|f| {
                let cb = prep.budget.per_file.iter().find(|c| c.target.as_deref() == Some(&f.path));
                CallPlan {
                    target: Some(f.path.clone()),
                    spec_text: per_file_spec(&prep.spec, &f.path),
                    targets: vec![(f.path.clone(), content_of(prep, &f.path).cloned().flatten())],
                    context: cb.map(|c| ctx_of(&c.context_files)).unwrap_or_default(),
                    allowed: Some(vec![f.path.clone()]),
                }
            })
            .collect(),
    }
}

pub struct RunContext<'a> {
    pub sink: Sink,
    pub http: reqwest::Client,
    pub endpoint: Endpoint,
    pub profile: &'a Profile,
    pub system_prompt: String,
    pub cancel: CancellationToken,
}

fn ev(sink: &Sink, run_id: &str, kind: &str, extra: serde_json::Value) {
    let mut v = json!({ "runId": run_id, "kind": kind });
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in e {
            o.insert(k.clone(), val.clone());
        }
    }
    emit(sink, "run-event", &v);
}

/// Run the implement step end to end.
pub async fn implement(ctx: &RunContext<'_>, req: &ImplementRequest) -> ImplementResult {
    let started = Instant::now();
    let root = PathBuf::from(&req.project_root);
    let prep = prepare(&root, &req.spec, &req.context_files, ctx.profile, &ctx.system_prompt);
    let mode = req.mode.unwrap_or(prep.budget.recommended);
    let mut result = ImplementResult {
        run_id: req.run_id.clone(),
        profile_id: ctx.profile.id.clone(),
        profile_name: ctx.profile.name.clone(),
        mode,
        budget: prep.budget.clone(),
        files: vec![],
        calls: vec![],
        validation_ok: false,
        cancelled: false,
        error: None,
        warnings: prep.warnings.clone(),
        duration_ms: 0,
        avg_tok_per_sec: None,
    };
    let calls = plan_calls(&prep, &req.spec, mode);
    if calls.iter().all(|c| c.targets.is_empty()) && prep.spec.files.is_empty() {
        result
            .warnings
            .push("No target files found in the spec; the model may edit any file it names.".into());
    }
    ev(
        &ctx.sink,
        &req.run_id,
        "start",
        json!({ "mode": mode, "calls": calls.len(), "targets": calls.iter().map(|c| c.target.clone()).collect::<Vec<_>>() }),
    );

    let source = ProjectSource { root: root.clone() };
    let mut ws = Workspace::new();
    let mut all_ok = true;
    let mut speeds = Vec::new();

    for (i, call) in calls.iter().enumerate() {
        if ctx.cancel.is_cancelled() {
            result.cancelled = true;
            break;
        }
        if mode == SplitMode::PerFile
            && let Some(cb) = prep.budget.per_file.iter().find(|c| c.target == call.target)
            && !cb.fits
        {
            let msg = format!(
                "Skipped `{}`: it does not fit in the context window even on its own.",
                call.target.clone().unwrap_or_default()
            );
            result.calls.push(CallReport {
                index: i,
                target: call.target.clone(),
                attempts: vec![AttemptReport {
                    error: Some(msg.clone()),
                    ..Default::default()
                }],
                ok: false,
            });
            ev(&ctx.sink, &req.run_id, "callEnd", json!({ "call": i, "ok": false, "error": msg }));
            all_ok = false;
            continue;
        }
        let report = run_call(ctx, req, i, call, &source, &mut ws).await;
        for a in &report.attempts {
            if let Some(t) = a.tok_per_sec {
                speeds.push(t);
            }
        }
        if report.attempts.iter().any(|a| a.error.as_deref() == Some("cancelled")) {
            result.cancelled = true;
        }
        all_ok &= report.ok;
        result.calls.push(report);
        if result.cancelled {
            break;
        }
    }

    result.files = ws.proposed();
    result.validation_ok = all_ok && !result.cancelled && !result.calls.is_empty();
    result.duration_ms = started.elapsed().as_millis() as u64;
    result.avg_tok_per_sec = if speeds.is_empty() {
        None
    } else {
        Some(speeds.iter().sum::<f64>() / speeds.len() as f64)
    };
    if let Some(e) = result
        .calls
        .iter()
        .flat_map(|c| c.attempts.iter())
        .filter_map(|a| a.error.clone())
        .find(|e| e != "cancelled")
    {
        result.error = Some(e);
    }
    ev(
        &ctx.sink,
        &req.run_id,
        "done",
        json!({ "ok": result.validation_ok, "cancelled": result.cancelled, "files": result.files.len(), "durationMs": result.duration_ms }),
    );
    result
}

async fn run_call(
    ctx: &RunContext<'_>,
    req: &ImplementRequest,
    index: usize,
    call: &CallPlan,
    source: &ProjectSource,
    ws: &mut Workspace,
) -> CallReport {
    let mut report = CallReport {
        index,
        target: call.target.clone(),
        attempts: vec![],
        ok: false,
    };
    let mut messages = vec![
        ChatMessage::system(ctx.system_prompt.clone()),
        ChatMessage::user(build_user_message(&call.spec_text, &call.targets, &call.context)),
    ];
    let required: Vec<String> = call.targets.iter().map(|(p, _)| p.clone()).collect();

    for attempt in 1..=2u32 {
        ev(
            &ctx.sink,
            &req.run_id,
            "callStart",
            json!({ "call": index, "target": call.target, "attempt": attempt }),
        );
        let body = build_body(ctx.profile, &ctx.endpoint.model, &messages, ctx.profile.max_output_tokens);
        let sink = ctx.sink.clone();
        let run_id = req.run_id.clone();
        let mut buf = String::new();
        let mut rbuf = String::new();
        let mut last_flush = Instant::now();
        let mut last_stats = Instant::now();
        let mut on_delta = |d: Delta<'_>, stats: LiveStats| {
            match d {
                Delta::Content(t) => buf.push_str(t),
                Delta::Reasoning(t) => rbuf.push_str(t),
            }
            if last_flush.elapsed().as_millis() >= 50 || buf.len() + rbuf.len() > 400 {
                if !buf.is_empty() {
                    ev(
                        &sink,
                        &run_id,
                        "delta",
                        json!({ "call": index, "attempt": attempt, "text": std::mem::take(&mut buf) }),
                    );
                }
                if !rbuf.is_empty() {
                    ev(
                        &sink,
                        &run_id,
                        "reasoningDelta",
                        json!({ "call": index, "attempt": attempt, "text": std::mem::take(&mut rbuf) }),
                    );
                }
                last_flush = Instant::now();
            }
            if last_stats.elapsed().as_millis() >= 300 {
                ev(
                    &sink,
                    &run_id,
                    "stats",
                    json!({ "call": index, "tokens": stats.tokens, "tokPerSec": stats.tok_per_sec, "elapsedMs": stats.elapsed_ms }),
                );
                last_stats = Instant::now();
            }
        };
        let outcome = stream_chat(&ctx.http, &ctx.endpoint, &body, &ctx.cancel, &mut on_delta).await;
        if !buf.is_empty() {
            ev(
                &ctx.sink,
                &req.run_id,
                "delta",
                json!({ "call": index, "attempt": attempt, "text": buf }),
            );
        }
        if !rbuf.is_empty() {
            ev(
                &ctx.sink,
                &req.run_id,
                "reasoningDelta",
                json!({ "call": index, "attempt": attempt, "text": rbuf }),
            );
        }
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                let msg = e.to_string();
                ev(
                    &ctx.sink,
                    &req.run_id,
                    "callEnd",
                    json!({ "call": index, "attempt": attempt, "ok": false, "error": msg }),
                );
                report.attempts.push(AttemptReport {
                    attempt,
                    error: Some(msg),
                    ..Default::default()
                });
                return report;
            }
        };
        if outcome.cancelled {
            report.attempts.push(AttemptReport {
                attempt,
                raw_output: outcome.content,
                error: Some("cancelled".into()),
                ..Default::default()
            });
            ev(
                &ctx.sink,
                &req.run_id,
                "callEnd",
                json!({ "call": index, "attempt": attempt, "ok": false, "error": "cancelled" }),
            );
            return report;
        }

        let stripped = strip_reasoning(&ctx.profile.reasoning, &outcome.content);
        let mut reasoning = outcome.reasoning.clone();
        if !stripped.reasoning.is_empty() {
            if !reasoning.is_empty() {
                reasoning.push_str("\n\n");
            }
            reasoning.push_str(&stripped.reasoning);
        }
        let parsed = parse_blocks(&stripped.content);
        let outcomes = ws.apply(&parsed.blocks, source, call.allowed.as_deref());
        // Every target must receive at least one successful block across attempts.
        let touched: Vec<String> = report
            .attempts
            .iter()
            .flat_map(|a| a.outcomes.iter())
            .chain(outcomes.iter())
            .filter(|o| o.ok)
            .map(|o| o.path.to_lowercase())
            .collect();
        let missing: Vec<String> = required.iter().filter(|p| !touched.contains(&p.to_lowercase())).cloned().collect();
        let truncated = outcome.finish_reason.as_deref() == Some("length");
        let mut issues = parsed.issues.clone();
        if truncated {
            issues.push(ParseIssue {
                line: 0,
                message: format!(
                    "Your reply was cut off at the {}-token output limit. Send fewer, smaller blocks.",
                    ctx.profile.max_output_tokens
                ),
            });
        }
        if parsed.blocks.is_empty() && issues.is_empty() {
            issues.push(ParseIssue {
                line: 0,
                message: "Your reply contained no SEARCH/REPLACE blocks.".into(),
            });
        }
        let ok = issues.is_empty() && outcomes.iter().all(|o| o.ok) && missing.is_empty();
        let att = AttemptReport {
            attempt,
            raw_output: outcome.content.clone(),
            reasoning,
            blocks: parsed.blocks.len(),
            outcomes: outcomes.clone(),
            parse_issues: issues.clone(),
            missing_targets: missing.clone(),
            ok,
            tok_per_sec: outcome.tok_per_sec,
            prompt_tokens: outcome.prompt_tokens,
            completion_tokens: outcome.completion_tokens,
            duration_ms: outcome.duration_ms,
            finish_reason: outcome.finish_reason.clone(),
            error: None,
        };
        ev(
            &ctx.sink,
            &req.run_id,
            "callEnd",
            json!({ "call": index, "attempt": attempt, "ok": ok, "blocks": parsed.blocks.len(), "failed": outcomes.iter().filter(|o| !o.ok).count(),
                    "issues": issues.len(), "missing": missing, "tokPerSec": outcome.tok_per_sec, "completionTokens": outcome.completion_tokens }),
        );
        report.attempts.push(att);
        if ok {
            report.ok = true;
            return report;
        }
        if attempt == 2 {
            return report;
        }
        // One automatic retry with the errors.
        let mut msg = retry_message(&outcomes, &parsed.blocks, &issues, ws, true);
        if !missing.is_empty() {
            msg = format!(
                "{}\n\nThe specification requires changes to these files, but you made no successful edit to them: {}.",
                msg,
                missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")
            );
        }
        // Keep the retry inside the budget: drop the full file contents if they don't fit.
        let used: u32 = messages.iter().map(|m| estimate_tokens(&m.content)).sum::<u32>() + estimate_tokens(&outcome.content);
        let limit = (ctx.profile.context_limit as f64 * (1.0 - budget::SAFETY_FRACTION)) as u32;
        if used + estimate_tokens(&msg) + ctx.profile.max_output_tokens + ctx.profile.reasoning.reasoning_reserve > limit {
            msg = retry_message(&outcomes, &parsed.blocks, &issues, ws, false);
        }
        ev(&ctx.sink, &req.run_id, "retry", json!({ "call": index, "message": msg }));
        messages.push(ChatMessage::assistant(stripped.content));
        messages.push(ChatMessage::user(msg));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_profiles;

    const SPEC: &str = "## Summary\nS\n## Files\n- `a.py` (modify) — x\n- `b.py` (create) — y\n## Changes\n### `a.py`\nchange a\n### `b.py`\ncreate b, see `util.py`\n## Edge cases\n- e\n## Acceptance criteria\n- ok\n";

    fn project() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.py"), "def a():\n    return 1\n").unwrap();
        std::fs::write(d.path().join("util.py"), "X = 1\n").unwrap();
        std::fs::write(d.path().join("notes.md"), "n\n").unwrap();
        d
    }

    #[test]
    fn prepare_collects_targets_context_and_mentions() {
        let d = project();
        let p = prepare(
            d.path(),
            SPEC,
            &["notes.md".into(), "../evil".into()],
            &builtin_profiles()[0],
            "sys",
        );
        let names: Vec<(&str, bool, bool)> = p.files.iter().map(|f| (f.path.as_str(), f.target, f.exists)).collect();
        assert_eq!(
            names,
            vec![
                ("a.py", true, true),
                ("b.py", true, false),
                ("notes.md", false, true),
                ("util.py", false, true)
            ]
        );
        assert!(p.warnings.iter().any(|w| w.contains("unsafe path")));
        assert_eq!(p.budget.recommended, SplitMode::Single);
    }

    #[test]
    fn user_message_marks_new_files_and_context() {
        let m = build_user_message(
            "SPEC",
            &[("a.py".into(), Some("x = 1\n".into())), ("b.py".into(), None)],
            &[("u.py".into(), "y".into())],
        );
        assert!(m.contains("## `a.py`\n```python\nx = 1\n```"));
        assert!(m.contains("New file"));
        assert!(m.contains("Read-only context"));
    }

    #[test]
    fn per_file_plan_restricts_allowed_paths() {
        let d = project();
        let p = prepare(d.path(), SPEC, &[], &builtin_profiles()[0], "sys");
        let calls = plan_calls(&p, SPEC, SplitMode::PerFile);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].allowed.as_deref(), Some(&["a.py".to_string()][..]));
        assert!(calls[1].spec_text.contains("## Changes for `b.py`"));
        assert!(calls[1].targets[0].1.is_none());
    }
}
