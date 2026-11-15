//! The self-reliant loop after the spec is approved:
//!
//! 1. sync the staging copy of the project;
//! 2. the coder implements the spec into it;
//! 3. the verification commands run there; a failure goes back to the coder with the condensed
//!    output (up to `max_fix_rounds` times);
//! 4. the planner reviews the change and the results; NEEDS_FIXES with a follow-up spec starts
//!    another pass (up to `follow_up_rounds`), which is verified and reviewed again;
//! 5. the result is the diff between the staging copy and the project, for the user to accept.
//!
//! The real project is only read. Model swaps (planner and coder on different profiles) happen
//! through `AppCore::ensure_active`.

use crate::app::AppCore;
use crate::budget::{SplitMode, estimate_tokens};
use crate::claude::{self, ClaudeCall, Review};
use crate::config::{PlannerBackend, Profile, Settings};
use crate::editformat::{ProposedFile, Workspace, fenced_lang};
use crate::events::{Sink, emit};
use crate::llm::{ChatMessage, LiveStats};
use crate::pipeline::{self, CallReport, EditCall, ImplementRequest, ImplementResult, RunContext, lang_for};
use crate::planner::{self, PlannerCall, PlannerEvent};
use crate::project::{ProjectSource, unified_diff};
use crate::spec::parse_spec;
use crate::staging::Staging;
use crate::templates::{self, Template};
use crate::verify::{self, CommandResult, VerifyCommand, VerifyKind};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepKind {
    Stage,
    Implement,
    Verify,
    Fix,
    Review,
    FollowUp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStep {
    pub kind: StepKind,
    pub label: String,
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub commands: Vec<CommandResult>,
    #[serde(default)]
    pub review: Option<Review>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowReport {
    pub staging_dir: String,
    pub planner: String,
    pub coder: String,
    pub commands: Vec<VerifyCommand>,
    /// "spec", "detected" or "off".
    pub commands_source: String,
    pub steps: Vec<WorkflowStep>,
    /// `None`: nothing was verified.
    pub verified: Option<bool>,
    /// The last review.
    pub review: Option<Review>,
    /// One line for the result header.
    pub outcome: String,
}

/// What a run will do after the coder's first pass, decided from the settings and the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub commands: Vec<VerifyCommand>,
    pub commands_source: String,
    pub review: bool,
}

impl Plan {
    /// A plain implementation run (no staging) when there is nothing to verify or review.
    pub fn is_plain(&self) -> bool {
        self.commands.is_empty() && !self.review
    }
}

pub fn plan(core: &AppCore, settings: &Settings, spec_text: &str, root: &Path) -> Plan {
    let (commands, commands_source) = if settings.verify_enabled {
        let (c, src) = verify::commands_for(&parse_spec(spec_text), &verify::detect(root));
        (c, src.to_string())
    } else {
        (vec![], "off".to_string())
    };
    let review = settings.loop_review
        && match settings.planner_backend {
            PlannerBackend::Local => core.profile(&settings.planner_profile).is_ok(),
            PlannerBackend::Claude => true,
        };
    Plan {
        commands,
        commands_source,
        review,
    }
}

pub fn planner_label(core: &AppCore, settings: &Settings) -> String {
    match settings.planner_backend {
        PlannerBackend::Local => core
            .profile(&settings.planner_profile)
            .map(|p| format!("{} (local)", p.name))
            .unwrap_or_else(|_| settings.planner_profile.clone()),
        PlannerBackend::Claude => format!(
            "Claude ({})",
            if settings.claude_backend == crate::config::ClaudeBackend::Cli {
                "CLI"
            } else {
                "API"
            }
        ),
    }
}

pub struct Input<'a> {
    pub run_id: &'a str,
    pub project_root: &'a Path,
    pub spec: &'a str,
    pub context_files: &'a [String],
    pub full_context: &'a [String],
    pub mode: Option<SplitMode>,
    pub coder: &'a Profile,
    pub cancel: &'a CancellationToken,
    pub prewarmed: Option<u64>,
}

/// Where planner output streams to: the expand step's `claude-event`s (with this job id), or a
/// run's `run-event`s (review inside the loop).
#[derive(Clone, Copy)]
pub enum EventTarget<'a> {
    Job(&'a str),
    Run(&'a str),
}

pub const REVIEW_SYSTEM: &str = "You are a meticulous senior engineer reviewing a change made by a smaller model. Think it through, check the code against every requirement, then answer in exactly the requested format.";

/// Batches planner output into events (one per ~100 ms).
struct Streamer<'a> {
    sink: &'a Sink,
    target: EventTarget<'a>,
    content: String,
    reasoning: String,
    last: Instant,
}

impl<'a> Streamer<'a> {
    fn new(sink: &'a Sink, target: EventTarget<'a>) -> Self {
        Self {
            sink,
            target,
            content: String::new(),
            reasoning: String::new(),
            last: Instant::now(),
        }
    }

    fn send(&self, kind: &str, text: &str, extra: serde_json::Value) {
        match self.target {
            EventTarget::Job(job) => emit(self.sink, "claude-event", &json!({ "jobId": job, "kind": kind, "text": text })),
            EventTarget::Run(run) => {
                let kind = match kind {
                    "delta" => "reviewDelta",
                    "thinking" => "reviewReasoning",
                    "tool" => "reviewTool",
                    "segment" => "reviewSegment",
                    "stats" => "reviewStats",
                    _ => "reviewStatus",
                };
                let mut v = json!({ "runId": run, "kind": kind, "text": text });
                if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
                    o.extend(e.clone());
                }
                emit(self.sink, "run-event", &v)
            }
        }
    }

    fn flush(&mut self) {
        if !self.reasoning.is_empty() {
            let t = std::mem::take(&mut self.reasoning);
            self.send("thinking", &t, json!({}));
        }
        if !self.content.is_empty() {
            let t = std::mem::take(&mut self.content);
            self.send("delta", &t, json!({}));
        }
        self.last = Instant::now();
    }

    fn on(&mut self, e: PlannerEvent<'_>) {
        match e {
            PlannerEvent::Content(t) => self.content.push_str(t),
            PlannerEvent::Reasoning(t) => self.reasoning.push_str(t),
            PlannerEvent::Tool(what) => {
                self.flush();
                self.send("tool", &what, json!({}));
                // Text before a tool call is exploration chatter, not the answer.
                self.send("segment", "", json!({}));
            }
            PlannerEvent::Status(s) => {
                self.flush();
                self.send("status", &s, json!({}));
            }
            PlannerEvent::Stats(LiveStats { tokens, tok_per_sec, .. }) => {
                self.send("stats", "", json!({ "tokens": tokens, "tokPerSec": tok_per_sec }));
            }
        }
        if self.last.elapsed().as_millis() >= 100 {
            self.flush();
        }
    }
}

/// The planner's answer to `prompt`, from the local planner or Claude per the settings. Local:
/// loads the planner profile if needed and lets it read `root` with tools (if enabled).
pub async fn planner_answer(
    core: &AppCore,
    settings: &Settings,
    system: Option<String>,
    prompt: String,
    root: &Path,
    target: EventTarget<'_>,
    cancel: &CancellationToken,
) -> Result<claude::ClaudeResult> {
    match settings.planner_backend {
        PlannerBackend::Claude => {
            let job = match target {
                EventTarget::Job(j) => j.to_string(),
                EventTarget::Run(r) => format!("{r}:review"),
            };
            let explore = settings.claude_cli_explore && settings.claude_backend == crate::config::ClaudeBackend::Cli;
            claude::run(
                settings,
                ClaudeCall {
                    job_id: &job,
                    system,
                    prompt,
                    cwd: Some(root.to_path_buf()),
                    allow_read_tools: explore,
                },
                &core.sink,
                cancel,
                &core.stream_http,
            )
            .await
        }
        PlannerBackend::Local => {
            let profile = core.profile(&settings.planner_profile)?;
            let mut streamer = Streamer::new(&core.sink, target);
            let st = core.server.status();
            if st.profile_id.as_deref() != Some(profile.id.as_str()) || st.state != crate::server::ServerState::Ready {
                streamer.on(PlannerEvent::Status(format!("Loading the planner model {}…", profile.name)));
            }
            let endpoint = tokio::select! {
                e = core.ensure_active(&profile.id) => e?,
                _ = cancel.cancelled() => return Ok(claude::ClaudeResult { backend: "local".into(), cancelled: true, ..Default::default() }),
            };
            streamer.on(PlannerEvent::Status(format!(
                "{} is thinking{}",
                profile.name,
                if settings.planner_tools {
                    " (read-only project tools on)"
                } else {
                    ""
                }
            )));
            let call = PlannerCall {
                profile: &profile,
                endpoint: &endpoint,
                system: system.unwrap_or_else(|| REVIEW_SYSTEM.to_string()),
                prompt,
                tools_root: settings.planner_tools.then_some(root),
                effort: &settings.planner_effort,
            };
            let out = planner::run(&core.stream_http, call, cancel, &mut |e| streamer.on(e)).await;
            streamer.flush();
            let out = match out {
                Ok(o) => o,
                Err(e) => {
                    streamer.send("error", &e.to_string(), json!({}));
                    return Err(e);
                }
            };
            streamer.send(if out.cancelled { "cancelled" } else { "done" }, "", json!({}));
            Ok(claude::ClaudeResult {
                text: out.text,
                backend: "local".into(),
                model: profile.name.clone(),
                duration_ms: out.duration_ms,
                cost_usd: None,
                stop_reason: out.finish_reason,
                cancelled: out.cancelled,
                cache_read_tokens: None,
                cache_write_tokens: None,
                tok_per_sec: out.tok_per_sec,
                tool_calls: out.tool_calls,
            })
        }
    }
}

/// Condensed output of a failed command for the coder, at most about `max_chars`.
pub fn failure_text(r: &CommandResult, cmd: Option<&VerifyCommand>, max_chars: usize) -> String {
    let mut s = format!("{}.", r.summary());
    if r.expect_missing
        && let Some(e) = cmd.and_then(|c| c.expect.as_deref())
    {
        s.push_str(&format!(" Its output must contain \"{e}\"."));
    }
    if r.timed_out && r.kind != Some(VerifyKind::Run) {
        s.push_str(" It did not finish in time; look for an endless loop or a wait for input.");
    }
    let out = verify::digest_output(&r.output, max_chars);
    if out.is_empty() {
        s.push_str("\n\n(The command printed nothing.)");
    } else {
        s.push_str(&format!("\n\nOutput (shortened):\n{}", fenced_lang(&out, "text")));
    }
    s
}

/// The review's view of the verification: one line per command, plus the failing output.
pub fn verification_summary(results: &[CommandResult], planned: &[VerifyCommand]) -> String {
    if planned.is_empty() {
        return "No verification commands (the spec says nothing can be checked automatically, or verification is off).".into();
    }
    let mut s = String::new();
    for r in results {
        s.push_str(&format!("- {}\n", r.summary()));
    }
    for c in planned.iter().skip(results.len()) {
        s.push_str(&format!(
            "- {} `{}` did not run (an earlier step failed)\n",
            c.kind.word(),
            c.command
        ));
    }
    if let Some(f) = results.iter().find(|r| !r.passed) {
        let out = verify::digest_output(&f.output, 3000);
        if !out.is_empty() {
            s.push_str(&format!(
                "\nOutput of the failing step (shortened):\n{}\n",
                fenced_lang(&out, "text")
            ));
        }
    }
    s
}

struct Loop<'a> {
    core: &'a AppCore,
    inp: &'a Input<'a>,
    settings: Settings,
    plan: Plan,
    staging: Staging,
    /// Content of each changed file when the run started (`None`: did not exist).
    originals: BTreeMap<String, Option<String>>,
    calls: Vec<CallReport>,
    report: WorkflowReport,
    last_results: Vec<CommandResult>,
    setup_done: bool,
}

impl<'a> Loop<'a> {
    fn phase(&self, step: StepKind, label: &str) {
        emit(
            &self.core.sink,
            "run-event",
            &json!({ "runId": self.inp.run_id, "kind": "phase", "step": step, "label": label }),
        );
    }

    fn step(&mut self, s: WorkflowStep) {
        emit(
            &self.core.sink,
            "run-event",
            &json!({ "runId": self.inp.run_id, "kind": "stepEnd", "step": s.kind, "label": s.label, "ok": s.ok, "detail": s.detail }),
        );
        self.report.steps.push(s);
    }

    fn cancelled(&self) -> bool {
        self.inp.cancel.is_cancelled()
    }

    /// Write proposed files into the staging copy, remembering where each started.
    fn write(&mut self, files: &[ProposedFile]) -> Result<()> {
        for f in files {
            let key = self
                .originals
                .keys()
                .find(|k| k.eq_ignore_ascii_case(&f.path))
                .cloned()
                .unwrap_or_else(|| f.path.clone());
            self.originals.entry(key).or_insert_with(|| f.original.clone());
            self.staging.write(&f.path, &f.proposed)?;
        }
        Ok(())
    }

    async fn coder_ctx(&self) -> Result<RunContext<'a>> {
        let endpoint = self.core.ensure_active(&self.inp.coder.id).await?;
        Ok(RunContext {
            sink: self.core.sink.clone(),
            http: self.core.stream_http.clone(),
            endpoint,
            profile: self.inp.coder,
            system_prompt: templates::load(Template::ImplementSystem),
            cancel: self.inp.cancel.clone(),
        })
    }

    /// The coder implements `spec` into the staging copy (first pass or a follow-up).
    async fn implement(&mut self, spec: &str, label: Option<String>, prewarmed: Option<u64>) -> Result<ImplementResult> {
        let ctx = self.coder_ctx().await?;
        let root = self.staging.root.to_string_lossy().into_owned();
        let prep = self
            .core
            .prepare(&root, spec, self.inp.context_files, self.inp.full_context, &self.inp.coder.id)
            .await?;
        let mode = if label.is_none() { self.inp.mode } else { None };
        if mode.unwrap_or(prep.budget.recommended) == SplitMode::PerFile {
            self.core
                .prime_shared_context(
                    self.inp.run_id,
                    self.inp.coder,
                    &ctx.endpoint,
                    &prep,
                    spec,
                    self.inp.cancel,
                    prewarmed,
                )
                .await;
        }
        let req = ImplementRequest {
            run_id: self.inp.run_id.to_string(),
            project_root: root,
            spec: spec.to_string(),
            context_files: self.inp.context_files.to_vec(),
            mode,
            context_options: self.core.context_options(self.inp.full_context),
        };
        let mut r = pipeline::implement_prepared(&ctx, &req, prep).await;
        for c in &mut r.calls {
            c.label = label.clone();
            c.index += self.calls.len();
        }
        self.calls.extend(r.calls.iter().cloned());
        self.write(&r.files)?;
        Ok(r)
    }

    async fn verify_and_fix(&mut self) -> Option<bool> {
        let max = self.settings.max_fix_rounds;
        let timeouts = {
            let (b, r) = (self.settings.build_timeout_secs, self.settings.run_timeout_secs);
            move |k: VerifyKind| Duration::from_secs(if k == VerifyKind::Run { r } else { b })
        };
        for round in 0..=max {
            let cmds: Vec<VerifyCommand> = self
                .plan
                .commands
                .iter()
                .filter(|c| !(self.setup_done && c.kind == VerifyKind::Setup))
                .cloned()
                .collect();
            let label = if round == 0 {
                "Verify".to_string()
            } else {
                format!("Verify after fix {round}")
            };
            self.phase(
                StepKind::Verify,
                &format!("{label}: running {} command(s) in the staging copy", cmds.len()),
            );
            let t = Instant::now();
            let results = verify::run_all(
                &cmds,
                &self.staging.root,
                &timeouts,
                self.inp.cancel,
                &self.core.sink,
                self.inp.run_id,
            )
            .await;
            let ok = results.len() == cmds.len() && results.iter().all(|r| r.passed);
            if results.iter().any(|r| r.kind == Some(VerifyKind::Setup) && r.passed) {
                self.setup_done = true;
            }
            let failed = results.iter().find(|r| !r.passed).cloned();
            self.step(WorkflowStep {
                kind: StepKind::Verify,
                label,
                ok,
                duration_ms: t.elapsed().as_millis() as u64,
                detail: match &failed {
                    Some(f) => f.summary(),
                    None if ok => format!("all {} passed", results.len()),
                    None => "cancelled".into(),
                },
                commands: results.clone(),
                review: None,
            });
            self.last_results = results;
            if ok {
                return Some(true);
            }
            if self.cancelled() {
                return None;
            }
            let Some(failed) = failed else { return Some(false) };
            if round == max || failed.kind == Some(VerifyKind::Setup) || failed.spawn_error.is_some() {
                return Some(false);
            }
            match self.fix(round + 1, &failed).await {
                Ok(true) => continue,
                Ok(false) => return Some(false),
                Err(e) => {
                    self.step(WorkflowStep {
                        kind: StepKind::Fix,
                        label: format!("Fix {}", round + 1),
                        ok: false,
                        duration_ms: 0,
                        detail: e.to_string(),
                        commands: vec![],
                        review: None,
                    });
                    return Some(false);
                }
            }
        }
        Some(false)
    }

    /// One fix round: the coder sees the failing output and the relevant files. True when it
    /// produced edits.
    async fn fix(&mut self, round: u32, failed: &CommandResult) -> Result<bool> {
        let label = format!("Fix {round}");
        self.phase(
            StepKind::Fix,
            &format!("{label}: sending the failure back to {}", self.inp.coder.name),
        );
        let t = Instant::now();
        let ctx = self.coder_ctx().await?;
        let coder = self.inp.coder;
        let system = ctx.system_prompt.clone();
        let cmd = self.plan.commands.iter().find(|c| c.command == failed.command);
        let failure = failure_text(failed, cmd, 12_000);
        let fixed = estimate_tokens(&system) + estimate_tokens(self.inp.spec) + estimate_tokens(&failure) + 1500;
        let budget = coder
            .context_limit
            .saturating_sub(coder.max_output_tokens + coder.reasoning.reasoning_reserve + fixed);
        // The files the output names first, then the rest of the change.
        let mut candidates = verify::paths_in_output(&failed.output, &self.staging.root);
        for p in self.originals.keys() {
            if !candidates.iter().any(|c| c.eq_ignore_ascii_case(p)) {
                candidates.push(p.clone());
            }
        }
        let mut files = String::new();
        let mut used = 0;
        let mut shown = Vec::new();
        for p in &candidates {
            let Ok(Some(text)) = self.staging.read(p) else { continue };
            let t = estimate_tokens(&text) + 20;
            if used + t > budget {
                continue;
            }
            used += t;
            files.push_str(&format!("\n## `{p}`\n{}\n", fenced_lang(&text, lang_for(p))));
            shown.push(p.clone());
        }
        if shown.is_empty() {
            self.step(WorkflowStep {
                kind: StepKind::Fix,
                label,
                ok: false,
                duration_ms: 0,
                detail: "The files involved do not fit in the coder's context.".into(),
                commands: vec![],
                review: None,
            });
            return Ok(false);
        }
        let user = templates::render(
            &templates::load(Template::Fix),
            &[("files", files.trim()), ("spec", self.inp.spec.trim()), ("failure", &failure)],
        );
        let call = EditCall {
            run_id: self.inp.run_id,
            index: self.calls.len(),
            target: None,
            label: Some(label.clone()),
            messages: vec![ChatMessage::system(system), ChatMessage::user(user)],
            required: vec![],
            allowed: None,
        };
        let source = ProjectSource {
            root: self.staging.root.clone(),
        };
        let mut ws = Workspace::new();
        let report = pipeline::run_edit_call(&ctx, &call, &source, &mut ws).await;
        let proposed = ws.proposed();
        self.write(&proposed)?;
        let ok = report.ok && !proposed.is_empty();
        let error = report.attempts.iter().rev().find_map(|a| a.error.clone());
        self.calls.push(report);
        self.step(WorkflowStep {
            kind: StepKind::Fix,
            label,
            ok,
            duration_ms: t.elapsed().as_millis() as u64,
            detail: if proposed.is_empty() {
                error.unwrap_or_else(|| "The coder produced no valid edits.".into())
            } else {
                format!(
                    "edited {} (saw {})",
                    proposed.iter().map(|f| format!("`{}`", f.path)).collect::<Vec<_>>().join(", "),
                    shown.join(", ")
                )
            },
            commands: vec![],
            review: None,
        });
        Ok(!proposed.is_empty())
    }

    /// The change so far: (unified diff, files after) against the project as it was.
    fn change(&self) -> (String, Vec<(String, String)>) {
        let mut diff = String::new();
        let mut after = Vec::new();
        for (p, orig) in &self.originals {
            let cur = self.staging.read(p).ok().flatten().unwrap_or_default();
            if orig.as_deref() == Some(cur.as_str()) {
                continue;
            }
            diff.push_str(&unified_diff(p, orig.as_deref(), &cur));
            diff.push('\n');
            after.push((p.clone(), cur));
        }
        if after.iter().map(|(_, c)| c.len()).sum::<usize>() > 60_000 {
            after.clear(); // the diff alone; the planner can read files with its tools
        }
        (diff, after)
    }

    async fn review(&mut self, n: u32) -> Option<Review> {
        let label = if n == 1 { "Review".to_string() } else { format!("Review {n}") };
        self.phase(
            StepKind::Review,
            &format!("{label}: {} checks the change", planner_label(self.core, &self.settings)),
        );
        let t = Instant::now();
        let (diff, after) = self.change();
        if diff.trim().is_empty() {
            return None;
        }
        let verification = verification_summary(&self.last_results, &self.plan.commands);
        let prompt = claude::build_review_prompt(self.inp.spec, &diff, &after, &verification);
        let res = planner_answer(
            self.core,
            &self.settings,
            None,
            prompt,
            &self.staging.root,
            EventTarget::Run(self.inp.run_id),
            self.inp.cancel,
        )
        .await;
        let (ok, detail, review) = match res {
            Ok(r) if r.cancelled => (false, "cancelled".to_string(), None),
            Ok(r) => {
                let rv = claude::parse_review(&r.text);
                let speed = r.tok_per_sec.map(|t| format!(" · {t:.0} tok/s")).unwrap_or_default();
                (
                    rv.passed,
                    format!("{}{speed}", if rv.verdict.is_empty() { "no verdict" } else { &rv.verdict }),
                    Some(rv),
                )
            }
            Err(e) => (false, format!("Review failed: {e}"), None),
        };
        self.step(WorkflowStep {
            kind: StepKind::Review,
            label,
            ok,
            duration_ms: t.elapsed().as_millis() as u64,
            detail,
            commands: vec![],
            review: review.clone(),
        });
        review
    }

    fn finish(mut self, mut first: ImplementResult, started: Instant) -> ImplementResult {
        let mut files = Vec::new();
        for (p, orig) in &self.originals {
            let Ok(Some(cur)) = self.staging.read(p) else { continue };
            if orig.as_deref() == Some(cur.as_str()) {
                continue;
            }
            files.push(ProposedFile {
                path: p.clone(),
                original: orig.clone(),
                proposed: cur,
            });
        }
        let speeds: Vec<f64> = self
            .calls
            .iter()
            .flat_map(|c| c.attempts.iter())
            .filter_map(|a| a.tok_per_sec)
            .collect();
        first.avg_tok_per_sec = (!speeds.is_empty()).then(|| speeds.iter().sum::<f64>() / speeds.len() as f64);
        for (i, c) in self.calls.iter_mut().enumerate() {
            c.index = i;
        }
        first.calls = std::mem::take(&mut self.calls);
        first.files = files;
        first.cancelled |= self.cancelled();
        if self.report.verified == Some(true) {
            first.validation_ok = true;
        }
        first.duration_ms = started.elapsed().as_millis() as u64;
        let r = &mut self.report;
        let reviewed = r
            .review
            .as_ref()
            .map(|rv| if rv.passed { "review PASS" } else { "review: needs fixes" });
        let verified = match r.verified {
            Some(true) => Some("verified"),
            Some(false) => Some("verification failed"),
            None if r.commands.is_empty() => None,
            None => Some("not verified"),
        };
        let fixes = r.steps.iter().filter(|s| s.kind == StepKind::Fix).count();
        let follow = r.steps.iter().filter(|s| s.kind == StepKind::FollowUp).count();
        let mut parts: Vec<String> = [verified, reviewed].into_iter().flatten().map(String::from).collect();
        if fixes > 0 {
            parts.push(format!("{fixes} fix round(s)"));
        }
        if follow > 0 {
            parts.push(format!("{follow} follow-up pass(es)"));
        }
        r.outcome = if first.cancelled {
            "cancelled".into()
        } else if first.files.is_empty() {
            "no changes".into()
        } else {
            parts.join(" · ")
        };
        first.workflow = Some(self.report);
        first
    }
}

/// Run the loop. Errors before the coder's first pass (staging, model load) are returned;
/// later failures end the loop and are reported in the steps.
pub async fn run(core: &AppCore, inp: Input<'_>, plan: Plan) -> Result<ImplementResult> {
    let started = Instant::now();
    let settings = core.settings();
    emit(
        &core.sink,
        "run-event",
        &json!({ "runId": inp.run_id, "kind": "phase", "step": StepKind::Stage, "label": "Syncing the staging copy of the project" }),
    );
    let staging = Staging::for_project(inp.project_root)?;
    let t = Instant::now();
    let st = {
        let s = staging.clone();
        tokio::task::spawn_blocking(move || s.sync())
            .await
            .map_err(|e| anyhow!("staging: {e}"))??
    };
    let mut lp = Loop {
        core,
        inp: &inp,
        report: WorkflowReport {
            staging_dir: staging.root.to_string_lossy().into_owned(),
            planner: planner_label(core, &settings),
            coder: inp.coder.name.clone(),
            commands: plan.commands.clone(),
            commands_source: plan.commands_source.clone(),
            ..Default::default()
        },
        settings,
        plan,
        staging,
        originals: BTreeMap::new(),
        calls: vec![],
        last_results: vec![],
        setup_done: false,
    };
    lp.step(WorkflowStep {
        kind: StepKind::Stage,
        label: "Staging copy".into(),
        ok: true,
        duration_ms: t.elapsed().as_millis() as u64,
        detail: format!("{} files, {} copied, {} removed", st.files, st.copied, st.deleted),
        commands: vec![],
        review: None,
    });

    lp.phase(StepKind::Implement, &format!("{} implements the spec", inp.coder.name));
    let t = Instant::now();
    let first = lp.implement(inp.spec, None, inp.prewarmed).await?;
    lp.step(WorkflowStep {
        kind: StepKind::Implement,
        label: "Implement".into(),
        ok: first.validation_ok,
        duration_ms: t.elapsed().as_millis() as u64,
        detail: format!(
            "{} file(s){}",
            first.files.len(),
            if first.validation_ok {
                ""
            } else {
                ", some edits failed validation"
            }
        ),
        commands: vec![],
        review: None,
    });
    if first.cancelled || first.files.is_empty() {
        return Ok(lp.finish(first, started));
    }

    let passes = lp.settings.follow_up_rounds;
    for pass in 0..=passes {
        if !lp.plan.commands.is_empty() {
            lp.report.verified = lp.verify_and_fix().await;
        }
        if lp.cancelled() || !lp.plan.review {
            break;
        }
        let Some(rv) = lp.review(pass + 1).await else { break };
        lp.report.review = Some(rv.clone());
        if rv.passed || pass == passes || lp.cancelled() {
            break;
        }
        let Some(follow) = rv.follow_up_spec.clone() else { break };
        let n = pass + 1;
        lp.phase(
            StepKind::FollowUp,
            &format!("Follow-up {n}: {} implements the review's fixes", lp.inp.coder.name),
        );
        let t = Instant::now();
        let r = match lp.implement(&follow, Some(format!("Follow-up {n}")), None).await {
            Ok(r) => r,
            Err(e) => {
                lp.step(WorkflowStep {
                    kind: StepKind::FollowUp,
                    label: format!("Follow-up {n}"),
                    ok: false,
                    duration_ms: t.elapsed().as_millis() as u64,
                    detail: e.to_string(),
                    commands: vec![],
                    review: None,
                });
                break;
            }
        };
        lp.step(WorkflowStep {
            kind: StepKind::FollowUp,
            label: format!("Follow-up {n}"),
            ok: r.validation_ok,
            duration_ms: t.elapsed().as_millis() as u64,
            detail: format!("{} file(s) changed", r.files.len()),
            commands: vec![],
            review: None,
        });
        if r.files.is_empty() || r.cancelled {
            break;
        }
    }
    Ok(lp.finish(first, started))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(kind: VerifyKind, cmd: &str, passed: bool, out: &str) -> CommandResult {
        CommandResult {
            kind: Some(kind),
            command: cmd.into(),
            exit_code: Some(if passed { 0 } else { 101 }),
            passed,
            duration_ms: 1200,
            output: out.into(),
            ..Default::default()
        }
    }

    #[test]
    fn failure_text_names_the_step_and_shows_the_errors() {
        let r = result(
            VerifyKind::Build,
            "cargo build",
            false,
            "   Compiling x\nerror[E0425]: cannot find value `y`\n --> src/main.rs:2:5\n",
        );
        let t = failure_text(&r, None, 4000);
        assert!(t.starts_with("build `cargo build` failed (exit code 101) in 1.2s."), "{t}");
        assert!(
            t.contains("```text\nerror[E0425]: cannot find value `y`") && !t.contains("Compiling"),
            "{t}"
        );
        let mut run = VerifyCommand::new(VerifyKind::Run, "app.exe");
        run.expect = Some("Ready".into());
        let mut r = result(VerifyKind::Run, "app.exe", false, "");
        r.expect_missing = true;
        let t = failure_text(&r, Some(&run), 4000);
        assert!(t.contains("must contain \"Ready\"") && t.contains("printed nothing"), "{t}");
    }

    #[test]
    fn verification_summary_lists_skipped_steps() {
        let planned = vec![
            VerifyCommand::new(VerifyKind::Build, "cargo build"),
            VerifyCommand::new(VerifyKind::Test, "cargo test"),
        ];
        let s = verification_summary(&[result(VerifyKind::Build, "cargo build", false, "error: boom")], &planned);
        assert!(
            s.contains("- build `cargo build` failed") && s.contains("- test `cargo test` did not run"),
            "{s}"
        );
        assert!(s.contains("error: boom"));
        assert!(verification_summary(&[], &[]).starts_with("No verification commands"));
    }
}
