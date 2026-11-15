//! The implement step: spec → local model → validated SEARCH/REPLACE edits → proposed files.

use crate::budget::{self, BudgetInput, BudgetPlan, FileTokens, SplitMode, estimate_tokens};
use crate::config::Profile;
use crate::editformat::{BlockOutcome, ParseIssue, ProposedFile, Workspace, fenced_lang, parse_blocks, retry_message};
use crate::events::{Sink, emit};
use crate::llm::{ChatMessage, Delta, Endpoint, LiveStats, build_body, stream_chat};
use crate::outline;
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
    #[serde(default)]
    pub context_options: ContextOptions,
}

/// Read-only context files at least this large may be sent as an outline; smaller ones go in
/// full because an outline would save little.
pub const OUTLINE_MIN_TOKENS: u32 = 600;
/// An outline is only used if it is at most this fraction of the full file.
pub const OUTLINE_MAX_RATIO: f64 = 0.8;

/// How read-only context files are sent to the local model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContextOptions {
    /// Send large context files as outlines (signatures, declarations, docstrings; bodies omitted).
    pub outlines: bool,
    /// Context files to send in full even when outlines are on.
    pub full: Vec<String>,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            outlines: true,
            full: Vec::new(),
        }
    }
}

impl ContextOptions {
    fn wants_full(&self, path: &str) -> bool {
        self.full
            .iter()
            .any(|f| normalize_rel(f).is_ok_and(|n| n.eq_ignore_ascii_case(path)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedFile {
    pub path: String,
    pub exists: bool,
    pub target: bool,
    pub action: Option<FileAction>,
    /// Tokens sent to the model: the outline's size if `outline`, else the whole file's.
    pub tokens: u32,
    /// Size of the whole file.
    #[serde(default)]
    pub full_tokens: u32,
    /// Size of the outline, for context files where an outline is available and worthwhile.
    #[serde(default)]
    pub outline_tokens: Option<u32>,
    /// True if this context file is sent as an outline.
    #[serde(default)]
    pub outline: bool,
    pub error: Option<String>,
}

/// A read-only context file as sent to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextText {
    pub path: String,
    pub text: String,
    pub outline: bool,
}

/// Everything known before the local model runs: parsed spec, files, and the budget plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prepared {
    pub spec: ParsedSpec,
    pub files: Vec<PreparedFile>,
    pub budget: BudgetPlan,
    pub warnings: Vec<String>,
    /// Text sent to the model per file: targets in full, context files in full or as an outline.
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
    /// Prompt tokens reused from the server's prompt cache (not processed again).
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    /// Prompt processing speed (tokens/s) for the part not taken from the cache.
    #[serde(default)]
    pub prompt_per_sec: Option<f64>,
    /// Speculative decoding: tokens drafted and accepted (when the server reports them).
    #[serde(default)]
    pub draft_tokens: Option<u32>,
    #[serde(default)]
    pub draft_accepted: Option<u32>,
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
    /// What the call was for, when not the first implementation pass ("Fix round 1", "Follow-up").
    #[serde(default)]
    pub label: Option<String>,
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
    /// Staging, verification, fix rounds and review (absent for a plain implementation run).
    #[serde(default)]
    pub workflow: Option<crate::workflow::WorkflowReport>,
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

/// Parse the spec, read the target and context files (safely), outline large context files,
/// and compute the budget plan with estimated token counts.
pub fn prepare(
    root: &Path,
    spec_text: &str,
    context_files: &[String],
    opts: &ContextOptions,
    profile: &Profile,
    system_prompt: &str,
) -> Prepared {
    prepare_with(
        root,
        spec_text,
        context_files,
        opts,
        profile,
        system_prompt,
        &estimate_tokens,
        false,
    )
}

/// [`prepare`] with a token counter: `count` gives the size of each text sent to the model
/// (exact counts from the model's tokenizer when `exact`).
#[allow(clippy::too_many_arguments)]
pub fn prepare_with(
    root: &Path,
    spec_text: &str,
    context_files: &[String],
    opts: &ContextOptions,
    profile: &Profile,
    system_prompt: &str,
    count: &dyn Fn(&str) -> u32,
    exact: bool,
) -> Prepared {
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
                let full_tokens = content.as_deref().map(count).unwrap_or(0);
                let outlined = match content.as_deref() {
                    Some(text) if !target && full_tokens >= OUTLINE_MIN_TOKENS => outline::outline(&p, text)
                        .map(|o| (count(&o.text), o.text))
                        .filter(|(t, _)| f64::from(*t) <= f64::from(full_tokens) * OUTLINE_MAX_RATIO),
                    _ => None,
                };
                let outline_tokens = outlined.as_ref().map(|(t, _)| *t);
                let (sent, tokens, outline) = match outlined {
                    Some((t, text)) if opts.outlines && !opts.wants_full(&p) => (Some(text), t, true),
                    _ => (content, full_tokens, false),
                };
                files.push(PreparedFile {
                    path: p.clone(),
                    exists,
                    target,
                    action,
                    tokens,
                    full_tokens,
                    outline_tokens,
                    outline,
                    error: None,
                });
                contents.push((p, sent));
            }
            Err(e) => {
                warnings.push(format!("Cannot read `{p}`: {e}"));
                files.push(PreparedFile {
                    path: p,
                    exists: false,
                    target,
                    action,
                    tokens: 0,
                    full_tokens: 0,
                    outline_tokens: None,
                    outline: false,
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
        .map(|f| (f.path.clone(), count(&per_file_spec(&spec, &f.path))))
        .collect();
    let budget = budget::plan(&BudgetInput {
        context_limit: profile.context_limit,
        max_output: profile.max_output_tokens,
        reasoning_reserve: profile.reasoning.reasoning_reserve,
        system_tokens: count(system_prompt),
        spec_tokens: count(spec_text),
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
        exact,
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

pub const OUTLINE_NOTE: &str = "Files marked (outline) show only declarations, signatures, doc comments and docstrings. \
Each `… (N lines omitted)` line stands for code that exists in the real file but is not shown.";

/// The user message for one local-model call.
///
/// The parts that change least come first, because llama-server reuses the KV cache for the
/// longest prefix a prompt shares with the previous one and only processes the rest:
/// read-only context (shared by all calls of a per-file run), then the files to edit (the same
/// when only the spec is edited and the job re-run), then the spec, which changes most.
pub fn build_user_message(spec_text: &str, targets: &[(String, Option<String>)], context: &[ContextText]) -> String {
    user_message(spec_text, targets, context).text
}

/// A user message from [`build_user_message`] with the byte offsets where its sections start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessage {
    pub text: String,
    /// Start of `# Files to edit` (everything before it is the read-only context).
    pub files_at: usize,
    /// Start of `# Specification`.
    pub spec_at: usize,
}

pub fn user_message(spec_text: &str, targets: &[(String, Option<String>)], context: &[ContextText]) -> UserMessage {
    let mut m = String::new();
    if !context.is_empty() {
        m.push_str("# Read-only context (do NOT edit these files)\n");
        if context.iter().any(|c| c.outline) {
            m.push_str(OUTLINE_NOTE);
            m.push('\n');
        }
        for c in context {
            let label = if c.outline { " (outline)" } else { "" };
            m.push_str(&format!("\n## `{}`{label}\n{}\n", c.path, fenced_lang(&c.text, lang_for(&c.path))));
        }
        m.push('\n');
    }
    let files_at = m.len();
    m.push_str("# Files to edit\n");
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
    m.push('\n');
    let spec_at = m.len();
    m.push_str("# Specification\n\n");
    m.push_str(spec_text.trim());
    m.push_str("\n\nNow output the SEARCH/REPLACE blocks that implement the specification.");
    UserMessage {
        text: m,
        files_at,
        spec_at,
    }
}

/// A prompt prefix to evaluate into the server's cache before the run needs it: the messages of
/// a call, cut `cut` bytes into the last message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimePoint {
    pub messages: Vec<ChatMessage>,
    pub cut: usize,
    pub label: &'static str,
}

impl PrimePoint {
    /// The prefix text (system prompt and the start of the user message), for deduplication.
    pub fn prefix_text(&self) -> String {
        let mut s = String::new();
        if let Some((last, rest)) = self.messages.split_last() {
            for m in rest {
                s.push_str(&m.role);
                s.push('\n');
                s.push_str(&m.content);
                s.push('\n');
            }
            s.push_str(&last.role);
            s.push('\n');
            s.push_str(&last.content[..self.cut.min(last.content.len())]);
        }
        s
    }
}

/// The messages of each local-model call (first attempt) of a run in `mode`.
pub fn call_messages(prep: &Prepared, spec_text: &str, mode: SplitMode, system_prompt: &str) -> Vec<Vec<ChatMessage>> {
    plan_calls(prep, spec_text, mode)
        .iter()
        .map(|c| {
            vec![
                ChatMessage::system(system_prompt),
                ChatMessage::user(build_user_message(&c.spec_text, &c.targets, &c.context)),
            ]
        })
        .collect()
}

/// Shared prefixes shorter than this are not worth a separate request.
pub const MIN_PRIME_BYTES: usize = 2000;

/// Prefixes worth priming for a run in `mode`:
/// * per-file runs with several calls: the read-only context all calls share, so the server can
///   return to it after each call (hybrid and sliding-window models only reuse a prefix that ends
///   at a cache checkpoint, and a request leaves one at the end of its prompt);
/// * `through_files`: everything before the first call's spec, so only the spec is left to
///   process when the run starts.
pub fn prime_points(prep: &Prepared, spec_text: &str, mode: SplitMode, system_prompt: &str, through_files: bool) -> Vec<PrimePoint> {
    let calls = plan_calls(prep, spec_text, mode);
    let messages: Vec<UserMessage> = calls.iter().map(|c| user_message(&c.spec_text, &c.targets, &c.context)).collect();
    let Some(first) = messages.first() else {
        return vec![];
    };
    let with_system = |text: &str| vec![ChatMessage::system(system_prompt), ChatMessage::user(text)];
    let mut points = Vec::new();
    if mode == SplitMode::PerFile && messages.len() >= 2 {
        // The longest prefix every call shares, cut back to a line start.
        let shared = messages[1..]
            .iter()
            .map(|m| first.text.bytes().zip(m.text.bytes()).take_while(|(a, b)| a == b).count())
            .min()
            .unwrap_or(0)
            .min(first.files_at);
        let cut = first.text[..shared].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if cut >= MIN_PRIME_BYTES {
            points.push(PrimePoint {
                messages: with_system(&first.text),
                cut,
                label: "shared context",
            });
        }
    }
    if through_files {
        points.push(PrimePoint {
            messages: with_system(&first.text),
            cut: first.spec_at,
            label: "context and files",
        });
    }
    points
}

struct CallPlan {
    target: Option<String>,
    spec_text: String,
    targets: Vec<(String, Option<String>)>,
    context: Vec<ContextText>,
    allowed: Option<Vec<String>>,
}

fn plan_calls(prep: &Prepared, spec_text: &str, mode: SplitMode) -> Vec<CallPlan> {
    let target_files: Vec<&PreparedFile> = prep.files.iter().filter(|f| f.target && f.error.is_none()).collect();
    let ctx_of = |names: &[String]| -> Vec<ContextText> {
        names
            .iter()
            .filter_map(|n| {
                let text = content_of(prep, n).and_then(|c| c.clone())?;
                let outline = prep.files.iter().any(|f| f.outline && f.path.eq_ignore_ascii_case(n));
                Some(ContextText {
                    path: n.clone(),
                    text,
                    outline,
                })
            })
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

/// Run the implement step end to end (token counts estimated).
pub async fn implement(ctx: &RunContext<'_>, req: &ImplementRequest) -> ImplementResult {
    let prep = prepare(
        &PathBuf::from(&req.project_root),
        &req.spec,
        &req.context_files,
        &req.context_options,
        ctx.profile,
        &ctx.system_prompt,
    );
    implement_prepared(ctx, req, prep).await
}

/// Run the implement step for an already prepared request (e.g. with exact token counts).
pub async fn implement_prepared(ctx: &RunContext<'_>, req: &ImplementRequest, prep: Prepared) -> ImplementResult {
    let started = Instant::now();
    let root = PathBuf::from(&req.project_root);
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
        workflow: None,
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
                label: None,
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
    let messages = vec![
        ChatMessage::system(ctx.system_prompt.clone()),
        ChatMessage::user(build_user_message(&call.spec_text, &call.targets, &call.context)),
    ];
    let required: Vec<String> = call.targets.iter().map(|(p, _)| p.clone()).collect();
    run_edit_call(
        ctx,
        &EditCall {
            run_id: &req.run_id,
            index,
            target: call.target.clone(),
            label: None,
            messages,
            required,
            allowed: call.allowed.clone(),
        },
        source,
        ws,
    )
    .await
}

/// One local-model edit request: the messages to send, which files must receive an edit, which
/// may be edited (`None` = any file inside the source's root).
pub struct EditCall<'a> {
    pub run_id: &'a str,
    pub index: usize,
    pub target: Option<String>,
    pub label: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub required: Vec<String>,
    pub allowed: Option<Vec<String>>,
}

/// Send an edit request, apply the SEARCH/REPLACE blocks to `ws`, and retry once with the errors
/// if any block fails, a required file got no edit, or the reply was cut off.
pub async fn run_edit_call(
    ctx: &RunContext<'_>,
    call: &EditCall<'_>,
    source: &(dyn crate::editformat::FileSource + Sync),
    ws: &mut Workspace,
) -> CallReport {
    let index = call.index;
    let run_id = call.run_id;
    let mut report = CallReport {
        index,
        target: call.target.clone(),
        attempts: vec![],
        ok: false,
        label: call.label.clone(),
    };
    let mut messages = call.messages.clone();
    let required = &call.required;

    for attempt in 1..=2u32 {
        ev(
            &ctx.sink,
            run_id,
            "callStart",
            json!({ "call": index, "target": call.target, "attempt": attempt, "label": call.label }),
        );
        let body = build_body(ctx.profile, &ctx.endpoint.model, &messages, ctx.profile.max_output_tokens);
        let sink = ctx.sink.clone();
        let rid = run_id.to_string();
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
                        &rid,
                        "delta",
                        json!({ "call": index, "attempt": attempt, "text": std::mem::take(&mut buf) }),
                    );
                }
                if !rbuf.is_empty() {
                    ev(
                        &sink,
                        &rid,
                        "reasoningDelta",
                        json!({ "call": index, "attempt": attempt, "text": std::mem::take(&mut rbuf) }),
                    );
                }
                last_flush = Instant::now();
            }
            if last_stats.elapsed().as_millis() >= 300 {
                ev(
                    &sink,
                    &rid,
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
                run_id,
                "delta",
                json!({ "call": index, "attempt": attempt, "text": buf }),
            );
        }
        if !rbuf.is_empty() {
            ev(
                &ctx.sink,
                run_id,
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
                    run_id,
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
                run_id,
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
            cached_tokens: outcome.cached_tokens,
            completion_tokens: outcome.completion_tokens,
            prompt_per_sec: outcome.prompt_per_sec,
            draft_tokens: outcome.draft_tokens,
            draft_accepted: outcome.draft_accepted,
            duration_ms: outcome.duration_ms,
            finish_reason: outcome.finish_reason.clone(),
            error: None,
        };
        ev(
            &ctx.sink,
            run_id,
            "callEnd",
            json!({ "call": index, "attempt": attempt, "ok": ok, "blocks": parsed.blocks.len(), "failed": outcomes.iter().filter(|o| !o.ok).count(),
                    "issues": issues.len(), "missing": missing, "tokPerSec": outcome.tok_per_sec, "completionTokens": outcome.completion_tokens,
                    "promptTokens": outcome.prompt_tokens, "cachedTokens": outcome.cached_tokens }),
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
        // Keep the retry inside the budget: drop the full file contents if they don't fit. The
        // conversation so far is the server's own count when it reports usage (an overestimate
        // by the reasoning tokens, which are not sent back).
        let used: u32 = match (outcome.prompt_tokens, outcome.completion_tokens) {
            (Some(p), Some(c)) => p + c,
            _ => messages.iter().map(|m| estimate_tokens(&m.content)).sum::<u32>() + estimate_tokens(&outcome.content),
        };
        let limit = (ctx.profile.context_limit as f64 * (1.0 - budget::SAFETY_FRACTION)) as u32;
        if used + estimate_tokens(&msg) + ctx.profile.max_output_tokens + ctx.profile.reasoning.reasoning_reserve > limit {
            msg = retry_message(&outcomes, &parsed.blocks, &issues, ws, false);
        }
        ev(&ctx.sink, run_id, "retry", json!({ "call": index, "message": msg }));
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

    fn prep(d: &tempfile::TempDir, context: &[String], opts: &ContextOptions) -> Prepared {
        prepare(d.path(), SPEC, context, opts, &builtin_profiles()[0], "sys")
    }

    /// A Python module of `n` helpers, each with a 4-line body after its docstring.
    fn big_py(n: usize) -> String {
        let mut s = String::from("import os\n\n");
        for i in 0..n {
            s.push_str(&format!(
                "def helper_{i}(value: int) -> int:\n    \"\"\"Helper {i}.\"\"\"\n    total = value + {i}\n    total *= 2\n    total -= 1\n    return total\n\n\n"
            ));
        }
        s
    }

    #[test]
    fn prepare_collects_targets_context_and_mentions() {
        let d = project();
        let p = prep(&d, &["notes.md".into(), "../evil".into()], &ContextOptions::default());
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
            &[ContextText {
                path: "u.py".into(),
                text: "y".into(),
                outline: false,
            }],
        );
        assert!(m.contains("## `a.py`\n```python\nx = 1\n```"));
        assert!(m.contains("New file"));
        assert!(m.contains("Read-only context"));
        assert!(m.contains("## `u.py`\n```python\ny\n```"));
        assert!(!m.contains(OUTLINE_NOTE), "no outline note without outlines");
    }

    fn common_prefix(a: &str, b: &str) -> usize {
        a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
    }

    #[test]
    fn message_puts_context_then_files_then_spec() {
        let m = build_user_message(
            "SPEC TEXT",
            &[("a.py".into(), Some("x = 1\n".into()))],
            &[ContextText {
                path: "u.py".into(),
                text: "y".into(),
                outline: false,
            }],
        );
        let ctx = m.find("# Read-only context").unwrap();
        let files = m.find("# Files to edit").unwrap();
        let spec = m.find("# Specification\n\nSPEC TEXT").unwrap();
        assert!(ctx == 0 && ctx < files && files < spec, "{m}");
        assert!(m.ends_with("Now output the SEARCH/REPLACE blocks that implement the specification."));
        // Without context the message starts with the files.
        let m = build_user_message("S", &[("a.py".into(), None)], &[]);
        assert!(m.starts_with("# Files to edit\n"), "{m}");
    }

    #[test]
    fn per_file_calls_share_the_context_as_a_cacheable_prefix() {
        let d = project();
        std::fs::write(d.path().join("util.py"), big_py(40)).unwrap();
        std::fs::write(d.path().join("notes.md"), "notes\n".repeat(50)).unwrap();
        let p = prep(&d, &["notes.md".into()], &ContextOptions::default());
        let calls = plan_calls(&p, SPEC, SplitMode::PerFile);
        assert_eq!(calls.len(), 2);
        let msgs: Vec<String> = calls
            .iter()
            .map(|c| build_user_message(&c.spec_text, &c.targets, &c.context))
            .collect();
        let files_at = msgs[0].find("# Files to edit").unwrap();
        let shared = &msgs[0][..files_at];
        assert!(
            shared.contains("## `util.py` (outline)") && shared.contains("## `notes.md`"),
            "{shared}"
        );
        assert!(
            common_prefix(&msgs[0], &msgs[1]) > files_at,
            "the second call must reuse the whole context section"
        );
    }

    #[test]
    fn editing_the_spec_keeps_the_files_cacheable() {
        let d = project();
        let p = prep(&d, &[], &ContextOptions::default());
        let calls = plan_calls(&p, SPEC, SplitMode::Single);
        let before = build_user_message(SPEC, &calls[0].targets, &calls[0].context);
        let edited = SPEC.replace("change a", "change a differently");
        let after = build_user_message(&edited, &calls[0].targets, &calls[0].context);
        assert!(common_prefix(&before, &after) > before.find("# Specification").unwrap());
    }

    #[test]
    fn large_context_files_are_outlined_unless_sent_in_full() {
        let d = project();
        // util.py is mentioned by SPEC, so it is auto-included as read-only context.
        std::fs::write(d.path().join("util.py"), big_py(40)).unwrap();
        let p = prep(&d, &[], &ContextOptions::default());
        let u = p.files.iter().find(|f| f.path == "util.py").unwrap();
        assert!(u.outline && !u.target);
        assert_eq!(u.outline_tokens, Some(u.tokens));
        assert!(u.tokens * 4 < u.full_tokens * 3, "outline {} vs full {}", u.tokens, u.full_tokens);

        let calls = plan_calls(&p, SPEC, SplitMode::Single);
        let c = calls[0].context.iter().find(|c| c.path == "util.py").unwrap();
        assert!(c.outline);
        assert!(
            c.text
                .contains("def helper_3(value: int) -> int:\n    \"\"\"Helper 3.\"\"\"\n    # … (4 lines omitted)\n"),
            "{}",
            c.text
        );
        let msg = build_user_message(SPEC, &calls[0].targets, &calls[0].context);
        assert!(msg.contains("## `util.py` (outline)\n```python\nimport os"));
        assert!(msg.contains(OUTLINE_NOTE));

        // Asking for the whole file (any spelling of the path) or switching outlines off sends it in
        // full; the budget then counts the whole file.
        for opts in [
            ContextOptions {
                outlines: true,
                full: vec!["./UTIL.py".into()],
            },
            ContextOptions {
                outlines: false,
                full: vec![],
            },
        ] {
            let pf = prep(&d, &[], &opts);
            let uf = pf.files.iter().find(|f| f.path == "util.py").unwrap();
            assert!(!uf.outline && uf.tokens == uf.full_tokens, "{opts:?}");
            assert_eq!(uf.outline_tokens, u.outline_tokens, "the outline size is still reported");
            assert_eq!(
                pf.budget.single.input_tokens - p.budget.single.input_tokens,
                u.full_tokens - u.tokens
            );
            let calls = plan_calls(&pf, SPEC, SplitMode::Single);
            let c = calls[0].context.iter().find(|c| c.path == "util.py").unwrap();
            assert!(!c.outline && c.text.contains("    total -= 1\n"));
        }
    }

    #[test]
    fn targets_and_small_or_unsupported_context_files_are_sent_in_full() {
        let d = project();
        std::fs::write(d.path().join("a.py"), big_py(40)).unwrap();
        std::fs::write(d.path().join("notes.md"), "line of notes\n".repeat(400)).unwrap();
        let p = prep(&d, &["notes.md".into()], &ContextOptions::default());
        for f in &p.files {
            assert!(!f.outline && f.tokens == f.full_tokens, "{f:?}");
        }
        let a = p.files.iter().find(|f| f.path == "a.py").unwrap();
        assert!(a.target && a.outline_tokens.is_none() && a.tokens > OUTLINE_MIN_TOKENS);
        let u = p.files.iter().find(|f| f.path == "util.py").unwrap();
        assert!(u.tokens < OUTLINE_MIN_TOKENS && u.outline_tokens.is_none());
        let n = p.files.iter().find(|f| f.path == "notes.md").unwrap();
        assert!(
            n.tokens > OUTLINE_MIN_TOKENS && n.outline_tokens.is_none(),
            "markdown has no outline"
        );
    }

    #[test]
    fn prepare_with_uses_the_given_counter() {
        let d = project();
        let words = |t: &str| t.split_whitespace().count() as u32;
        let p = prepare_with(
            d.path(),
            SPEC,
            &[],
            &ContextOptions::default(),
            &builtin_profiles()[0],
            "one two three",
            &words,
            true,
        );
        let a = p.files.iter().find(|f| f.path == "a.py").unwrap();
        assert_eq!(
            a.tokens,
            words(
                "def a():
    return 1
"
            )
        );
        assert!(p.budget.exact);
        let est = prep(&d, &[], &ContextOptions::default());
        assert!(!est.budget.exact);
        assert_eq!(
            est.files.iter().find(|f| f.path == "a.py").unwrap().tokens,
            estimate_tokens(
                "def a():
    return 1
"
            )
        );
    }

    #[test]
    fn user_message_offsets_mark_the_sections() {
        let ctx = [ContextText {
            path: "u.py".into(),
            text: "y".into(),
            outline: false,
        }];
        let m = user_message("SPEC", &[("a.py".into(), Some("x = 1\n".into()))], &ctx);
        assert!(m.text[m.files_at..].starts_with("# Files to edit\n"));
        assert!(m.text[m.spec_at..].starts_with("# Specification\n\nSPEC"));
        assert!(m.text[..m.files_at].starts_with("# Read-only context"));
        let m = user_message("S", &[("a.py".into(), None)], &[]);
        assert_eq!(m.files_at, 0);
    }

    #[test]
    fn prime_points_cover_the_shared_context_and_the_first_call() {
        let d = project();
        std::fs::write(d.path().join("util.py"), big_py(60)).unwrap();
        let opts = ContextOptions {
            outlines: true,
            full: vec!["util.py".into()],
        };
        let p = prepare(d.path(), SPEC, &[], &opts, &builtin_profiles()[0], "sys");
        let calls = plan_calls(&p, SPEC, SplitMode::PerFile);
        let second = build_user_message(&calls[1].spec_text, &calls[1].targets, &calls[1].context);

        let pts = prime_points(&p, SPEC, SplitMode::PerFile, "sys", true);
        assert_eq!(
            pts.iter().map(|x| x.label).collect::<Vec<_>>(),
            vec!["shared context", "context and files"]
        );
        let shared = &pts[0];
        let text = &shared.messages[1].content;
        assert_eq!(shared.messages[0].content, "sys");
        assert!(text[..shared.cut].contains("## `util.py`") && !text[..shared.cut].contains("# Files to edit"));
        assert!(text[..shared.cut].ends_with('\n'));
        assert!(
            second.starts_with(&text[..shared.cut]),
            "the second call starts with the primed prefix"
        );
        let files = &pts[1];
        assert!(files.messages[1].content[files.cut..].starts_with("# Specification"));

        // Single mode: only the prefix before the spec, and only when asked.
        let single = prime_points(&p, SPEC, SplitMode::Single, "sys", true);
        assert_eq!(single.len(), 1);
        assert!(prime_points(&p, SPEC, SplitMode::Single, "sys", false).is_empty());
        // Editing the spec leaves the prefixes unchanged.
        let edited = SPEC.replace("change a", "change a differently");
        let again = prime_points(&p, &edited, SplitMode::PerFile, "sys", true);
        assert_eq!(again[0].prefix_text(), pts[0].prefix_text());
        assert_eq!(again[1].prefix_text(), pts[1].prefix_text());
        // Without a large shared context there is nothing worth priming separately.
        std::fs::write(d.path().join("util.py"), "X = 1\n").unwrap();
        let small = prepare(d.path(), SPEC, &[], &ContextOptions::default(), &builtin_profiles()[0], "sys");
        assert!(prime_points(&small, SPEC, SplitMode::PerFile, "sys", false).is_empty());
    }

    #[test]
    fn per_file_plan_restricts_allowed_paths() {
        let d = project();
        let p = prep(&d, &[], &ContextOptions::default());
        let calls = plan_calls(&p, SPEC, SplitMode::PerFile);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].allowed.as_deref(), Some(&["a.py".to_string()][..]));
        assert!(calls[1].spec_text.contains("## Changes for `b.py`"));
        assert!(calls[1].targets[0].1.is_none());
    }
}
