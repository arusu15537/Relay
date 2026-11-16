//! Application core shared by the Tauri commands and the headless e2e harness.

use crate::budget::{SplitMode, estimate_tokens};
use crate::claude::{self, ClaudeCall, ClaudeResult, ExpandInputs, Explore, Review};
use crate::config::{self, Profile, ProfileKind, Settings, builtin_profiles};
use crate::editformat::{Workspace, parse_blocks};
use crate::events::Sink;
use crate::history::{History, JobRecord};
use crate::llm::{ChatMessage, Endpoint, build_body, stream_chat};
use crate::pipeline::{self, ContextOptions, ImplementRequest, ImplementResult, Prepared, RunContext};
use crate::prewarm::{self, PrimeError, PrimeOutcome};
use crate::project::{self, WriteConflict};
use crate::reasoning::strip_reasoning;
use crate::server::{ServerManager, ServerState, ServerStatus};
use crate::spec::{ParsedSpec, parse_spec};
use crate::templates::{self, Template};
use crate::tokens::{TokenCounter, TokenizerKey};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct AppCore {
    pub sink: Sink,
    pub http: reqwest::Client,
    /// No overall timeout: local generations and Claude streams can take many minutes.
    pub stream_http: reqwest::Client,
    settings: RwLock<Settings>,
    profiles: RwLock<Vec<Profile>>,
    pub server: ServerManager,
    pub history: History,
    jobs: Mutex<HashMap<String, CancellationToken>>,
    run_lock: tokio::sync::Mutex<()>,
    pub startup_notes: Vec<String>,
    /// Exact token counts from the loaded model's tokenizer, cached per text.
    pub tokens: TokenCounter,
    /// Cancels a spec-review prewarm when a run starts.
    prewarm_cancel: Mutex<Option<CancellationToken>>,
    /// What the last prewarm left in the server's cache (cleared by runs and model switches).
    last_prewarm: Mutex<Option<u64>>,
    /// Endpoints that cannot prewarm (no /apply-template, /tokenize or /completion).
    prime_unsupported: Mutex<std::collections::HashSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrewarmCommand {
    pub project_root: String,
    pub spec: String,
    #[serde(default)]
    pub context_files: Vec<String>,
    #[serde(default)]
    pub full_context: Vec<String>,
    pub profile_id: String,
    #[serde(default)]
    pub mode: Option<SplitMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PrewarmResult {
    /// `primed`, `warm` (already primed), `skipped`, `unsupported`, `cancelled` or `error`.
    pub status: String,
    pub message: String,
    pub outcomes: Vec<PrimeOutcome>,
}

impl PrewarmResult {
    fn new(status: &str, message: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            message: message.into(),
            outcomes: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandRequest {
    pub job_id: String,
    pub instruction: String,
    pub project_root: String,
    #[serde(default)]
    pub context_files: Vec<String>,
    pub profile_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandResult {
    pub job_id: String,
    pub spec: String,
    pub parsed: ParsedSpec,
    pub claude: ClaudeResult,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplementCommand {
    pub job_id: String,
    #[serde(default)]
    pub run_id: Option<String>,
    pub project_root: String,
    #[serde(default)]
    pub instruction: String,
    pub spec: String,
    #[serde(default)]
    pub context_files: Vec<String>,
    #[serde(default)]
    pub mode: Option<SplitMode>,
    pub profile_id: String,
    /// Context files to send in full even when outlines are on (Settings → context outlines).
    #[serde(default)]
    pub full_context: Vec<String>,
    /// Started by autopilot (the spec was not reviewed): secure mode applies.
    #[serde(default)]
    pub autopilot: bool,
}

/// Whether autopilot may implement a spec without the review, and how it would run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutopilotCheck {
    pub secure: bool,
    /// The verification commands would run sandboxed.
    pub sandboxed: bool,
    pub commands: Vec<crate::verify::VerifyCommand>,
    /// Commands secure mode does not run unattended (empty: autopilot may go on).
    pub blocked: Vec<crate::sandbox::Blocked>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplementResponse {
    pub result: ImplementResult,
    pub fallback_suggested: bool,
    pub fallback_profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptOutcome {
    pub ok: bool,
    pub conflict: Option<WriteConflict>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileTest {
    pub ok: bool,
    pub message: String,
    pub output: String,
    pub reasoning_chars: usize,
    pub tok_per_sec: Option<f64>,
    pub load_secs: Option<f64>,
    pub format_ok: bool,
}

impl AppCore {
    pub fn new(sink: Sink) -> Result<Self> {
        let mut notes = Vec::new();
        if let Some(n) = crate::server::cleanup_orphans() {
            notes.push(n);
        }
        let settings = config::load_settings();
        let profiles = config::load_profiles();
        let history = History::open(&config::app_dir().join("history.db"))?;
        for t in Template::ALL {
            let _ = templates::load(t); // materialise editable copies
        }
        Ok(Self {
            server: ServerManager::new(sink.clone()),
            sink,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            stream_http: reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build()?,
            settings: RwLock::new(settings),
            profiles: RwLock::new(profiles),
            history,
            jobs: Mutex::new(HashMap::new()),
            run_lock: tokio::sync::Mutex::new(()),
            startup_notes: notes,
            tokens: TokenCounter::default(),
            prewarm_cancel: Mutex::new(None),
            last_prewarm: Mutex::new(None),
            prime_unsupported: Mutex::new(Default::default()),
        })
    }

    // ----- settings & profiles -----------------------------------------------------------------

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    pub fn set_settings(&self, s: Settings) -> Result<()> {
        config::save_settings(&s)?;
        *self.settings.write().unwrap() = s;
        Ok(())
    }

    fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut s = self.settings.write().unwrap();
        f(&mut s);
        let _ = config::save_settings(&s);
    }

    pub fn profiles(&self) -> Vec<Profile> {
        self.profiles.read().unwrap().clone()
    }

    pub fn profile(&self, id: &str) -> Result<Profile> {
        self.profiles
            .read()
            .unwrap()
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown profile `{id}`"))
    }

    pub fn set_profiles(&self, list: Vec<Profile>) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for p in &list {
            if p.id.trim().is_empty() {
                bail!("Every profile needs an id");
            }
            if !seen.insert(p.id.clone()) {
                bail!("Duplicate profile id `{}`", p.id);
            }
            if p.context_limit < 2048 {
                bail!("`{}`: context limit must be at least 2048", p.name);
            }
            if p.max_output_tokens >= p.context_limit {
                bail!("`{}`: max output tokens must be smaller than the context limit", p.name);
            }
            if p.kind == ProfileKind::External && !p.external.base_url.starts_with("http") {
                bail!("`{}`: external base URL must start with http:// or https://", p.name);
            }
        }
        // Built-ins can be edited but not deleted.
        let mut list = list;
        for b in builtin_profiles().into_iter().rev() {
            if !list.iter().any(|p| p.id == b.id) {
                list.insert(0, b);
            }
        }
        config::save_profiles(&list)?;
        *self.profiles.write().unwrap() = list;
        Ok(())
    }

    pub fn reset_profile(&self, id: &str) -> Result<Profile> {
        let def = builtin_profiles()
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| anyhow!("`{id}` is not a built-in profile"))?;
        let mut list = self.profiles();
        if let Some(p) = list.iter_mut().find(|p| p.id == id) {
            *p = def.clone();
        }
        self.set_profiles(list)?;
        Ok(def)
    }

    pub fn endpoint_key(profile_id: &str) -> String {
        format!("endpoint:{profile_id}")
    }

    // ----- model server -----------------------------------------------------------------------

    pub async fn switch_profile(&self, id: &str) -> Result<ServerStatus> {
        let profile = self.profile(id)?;
        self.update_settings(|s| s.active_profile = id.to_string());
        let key = if profile.kind == ProfileKind::External && profile.external.use_api_key {
            claude::get_secret(&Self::endpoint_key(id))
        } else {
            None
        };
        let settings = self.settings();
        let st = self.server.activate(&profile, &settings, key).await?;
        if st.state == ServerState::Ready
            && let Some(secs) = st.load_secs
        {
            self.update_settings(|s| {
                s.load_stats.insert(id.to_string(), secs);
            });
        }
        Ok(st)
    }

    pub async fn stop_server(&self) -> Result<()> {
        self.server.stop().await
    }

    /// Make sure `profile_id` is loaded/connected and return its endpoint.
    pub async fn ensure_active(&self, profile_id: &str) -> Result<Endpoint> {
        let profile = self.profile(profile_id)?;
        let st = self.server.status();
        let st = if st.state == ServerState::Ready && st.profile_id.as_deref() == Some(profile_id) {
            st
        } else {
            self.switch_profile(profile_id).await?
        };
        if st.state != ServerState::Ready {
            bail!(
                "{} could not be started: {}{}",
                profile.name,
                st.message,
                st.hint.map(|h| format!(" — {h}")).unwrap_or_default()
            );
        }
        Ok(Self::endpoint_of(&profile, &st))
    }

    fn endpoint_of(profile: &Profile, st: &ServerStatus) -> Endpoint {
        match profile.kind {
            ProfileKind::Managed => Endpoint {
                base_url: st.base_url.clone().unwrap_or_default(),
                model: String::new(),
                api_key: None,
            },
            ProfileKind::External => Endpoint {
                base_url: profile.external.base_url.trim_end_matches('/').to_string(),
                model: profile.external.model.clone(),
                api_key: if profile.external.use_api_key {
                    claude::get_secret(&Self::endpoint_key(&profile.id))
                } else {
                    None
                },
            },
        }
    }

    /// The endpoint of `profile` if it is loaded and ready (never loads anything).
    fn loaded_endpoint(&self, profile: &Profile) -> Option<(Endpoint, ServerStatus)> {
        let st = self.server.status();
        (st.state == ServerState::Ready && st.profile_id.as_deref() == Some(profile.id.as_str()))
            .then(|| (Self::endpoint_of(profile, &st), st))
    }

    fn prime_supported(&self, endpoint: &Endpoint) -> bool {
        !self
            .prime_unsupported
            .lock()
            .unwrap()
            .contains(&crate::tokens::root_url(&endpoint.base_url))
    }

    fn mark_prime_unsupported(&self, endpoint: &Endpoint) {
        self.prime_unsupported
            .lock()
            .unwrap()
            .insert(crate::tokens::root_url(&endpoint.base_url));
    }

    /// While the spec is reviewed: evaluate everything before the spec (and, for per-file runs,
    /// the shared context) into the loaded model's cache, so the run only processes the spec.
    /// Does nothing unless the setting is on and the profile is already loaded and idle.
    pub async fn prewarm(&self, cmd: PrewarmCommand) -> Result<PrewarmResult> {
        if !self.settings().prewarm_prompt {
            return Ok(PrewarmResult::new("skipped", "Prewarming is off"));
        }
        let profile = self.profile(&cmd.profile_id)?;
        let Some((endpoint, st)) = self.loaded_endpoint(&profile) else {
            return Ok(PrewarmResult::new("skipped", format!("{} is not loaded", profile.name)));
        };
        if !self.prime_supported(&endpoint) {
            return Ok(PrewarmResult::new("unsupported", "This server cannot prewarm prompts"));
        }
        let Ok(_guard) = self.run_lock.try_lock() else {
            return Ok(PrewarmResult::new("skipped", "A run is in progress"));
        };
        let prep = self
            .prepare(&cmd.project_root, &cmd.spec, &cmd.context_files, &cmd.full_context, &cmd.profile_id)
            .await?;
        let mode = cmd.mode.unwrap_or(prep.budget.recommended);
        if mode == SplitMode::Single && !prep.budget.single.fits {
            return Ok(PrewarmResult::new("skipped", "The job does not fit in one call"));
        }
        let system = templates::load(Template::ImplementSystem);
        let points = pipeline::prime_points(&prep, &cmd.spec, mode, &system, true);
        if points.is_empty() {
            return Ok(PrewarmResult::new("skipped", "Nothing to prewarm"));
        }
        let sig = prewarm_signature(&profile, &st, &points);
        if *self.last_prewarm.lock().unwrap() == Some(sig) {
            return Ok(PrewarmResult::new("warm", "Already in the model's cache"));
        }
        let cancel = CancellationToken::new();
        *self.prewarm_cancel.lock().unwrap() = Some(cancel.clone());
        let mut res = PrewarmResult::new("primed", "");
        for p in &points {
            match prewarm::prime(&self.http, &self.stream_http, &endpoint, &profile, p, &cancel).await {
                Ok(o) => res.outcomes.push(o),
                Err(PrimeError::Unsupported) => {
                    self.mark_prime_unsupported(&endpoint);
                    return Ok(PrewarmResult::new("unsupported", "This server cannot prewarm prompts"));
                }
                Err(PrimeError::Cancelled) => {
                    res.status = "cancelled".into();
                    res.message = "Cancelled (a run started)".into();
                    return Ok(res);
                }
                Err(PrimeError::Failed(m)) => {
                    res.status = "error".into();
                    res.message = m;
                    return Ok(res);
                }
            }
        }
        *self.last_prewarm.lock().unwrap() = Some(sig);
        let last = res.outcomes.last().cloned().unwrap_or_default();
        res.message = format!(
            "{} prompt tokens ready in {}'s cache ({} processed in {:.1}s)",
            last.tokens,
            profile.name,
            res.outcomes.iter().filter_map(|o| o.processed).sum::<u32>(),
            res.outcomes.iter().map(|o| o.ms).sum::<u64>() as f64 / 1000.0
        );
        Ok(res)
    }

    /// Per-file runs: prime the context the calls share, so each call after the first can go back
    /// to it instead of processing it again. Skipped when a spec-review prewarm already primed
    /// exactly this run (priming again would discard its deeper checkpoint).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prime_shared_context(
        &self,
        run_id: &str,
        profile: &Profile,
        endpoint: &Endpoint,
        prep: &Prepared,
        spec: &str,
        cancel: &CancellationToken,
        prewarmed: Option<u64>,
    ) {
        let system = templates::load(Template::ImplementSystem);
        if let (Some(sig), Some((_, st))) = (prewarmed, self.loaded_endpoint(profile))
            && prewarm_signature(profile, &st, &pipeline::prime_points(prep, spec, SplitMode::PerFile, &system, true)) == sig
        {
            return;
        }
        self.prime_points_for_run(run_id, profile, endpoint, prep, spec, cancel, &system)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn prime_points_for_run(
        &self,
        run_id: &str,
        profile: &Profile,
        endpoint: &Endpoint,
        prep: &Prepared,
        spec: &str,
        cancel: &CancellationToken,
        system: &str,
    ) {
        if !self.prime_supported(endpoint) {
            return;
        }
        for p in pipeline::prime_points(prep, spec, SplitMode::PerFile, system, false) {
            let ev = match prewarm::prime(&self.http, &self.stream_http, endpoint, profile, &p, cancel).await {
                Ok(o) => serde_json::json!({ "runId": run_id, "kind": "primed", "label": o.label, "tokens": o.tokens,
                                             "cached": o.cached, "processed": o.processed, "ms": o.ms }),
                Err(PrimeError::Unsupported) => {
                    self.mark_prime_unsupported(endpoint);
                    return;
                }
                Err(PrimeError::Cancelled) => return,
                Err(PrimeError::Failed(m)) => serde_json::json!({ "runId": run_id, "kind": "primed", "label": p.label, "error": m }),
            };
            crate::events::emit(&self.sink, "run-event", &ev);
        }
    }

    /// Load the profile and run a tiny edit task to check speed, reasoning handling and format.
    pub async fn test_profile(&self, profile_id: &str) -> Result<ProfileTest> {
        let profile = self.profile(profile_id)?;
        let endpoint = match self.ensure_active(profile_id).await {
            Ok(e) => e,
            Err(e) => {
                return Ok(ProfileTest {
                    ok: false,
                    message: e.to_string(),
                    output: String::new(),
                    reasoning_chars: 0,
                    tok_per_sec: None,
                    load_secs: None,
                    format_ok: false,
                });
            }
        };
        let load_secs = self.server.status().load_secs;
        let file = "def greet():\n    print('hi')\n";
        let msgs = vec![
            ChatMessage::system(templates::load(Template::ImplementSystem)),
            ChatMessage::user(pipeline::build_user_message(
                "## Summary\nChange the greeting printed by `greet()` from 'hi' to 'hello'.\n## Files\n- `hello.py` (modify)\n",
                &[("hello.py".into(), Some(file.into()))],
                &[],
            )),
        ];
        let body = build_body(&profile, &endpoint.model, &msgs, 1024);
        let cancel = CancellationToken::new();
        let out = stream_chat(&self.stream_http, &endpoint, &body, &cancel, &mut |_, _| {}).await?;
        let stripped = strip_reasoning(&profile.reasoning, &out.content);
        let parsed = parse_blocks(&stripped.content);
        let src: BTreeMap<String, String> = [("hello.py".to_string(), file.to_string())].into();
        let mut ws = Workspace::new();
        let outcomes = ws.apply(&parsed.blocks, &src, None);
        let format_ok =
            !parsed.blocks.is_empty() && outcomes.iter().all(|o| o.ok) && ws.current_text("hello.py").is_some_and(|t| t.contains("hello"));
        let reasoning_chars = out.reasoning.len() + stripped.reasoning.len();
        Ok(ProfileTest {
            ok: true,
            message: format!(
                "{}: {}{}. Edit-format check {}.",
                profile.name,
                out.tok_per_sec
                    .map(|t| format!("{t:.1} tok/s"))
                    .unwrap_or_else(|| "speed unknown".into()),
                if reasoning_chars > 0 {
                    format!(", {reasoning_chars} chars of reasoning stripped")
                } else {
                    String::new()
                },
                if format_ok { "passed" } else { "FAILED" }
            ),
            output: stripped.content,
            reasoning_chars,
            tok_per_sec: out.tok_per_sec,
            load_secs,
            format_ok,
        })
    }

    // ----- jobs -------------------------------------------------------------------------------

    pub fn register_job(&self, id: &str) -> CancellationToken {
        let t = CancellationToken::new();
        self.jobs.lock().unwrap().insert(id.to_string(), t.clone());
        t
    }

    pub fn finish_job(&self, id: &str) {
        self.jobs.lock().unwrap().remove(id);
    }

    pub fn cancel(&self, id: &str) -> bool {
        match self.jobs.lock().unwrap().get(id) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    // ----- expand -----------------------------------------------------------------------------

    pub async fn expand(&self, req: ExpandRequest) -> Result<ExpandResult> {
        let settings = self.settings();
        let profile = self.profile(&req.profile_id)?;
        let root = PathBuf::from(&req.project_root);
        if !root.is_dir() {
            bail!("Choose a project folder first");
        }
        if req.instruction.trim().is_empty() {
            bail!("Type an instruction first");
        }
        let local = settings.planner_backend == config::PlannerBackend::Local;
        let explore = if local {
            if settings.planner_tools {
                Explore::LocalTools
            } else {
                Explore::None
            }
        } else if settings.claude_cli_explore && settings.claude_backend == config::ClaudeBackend::Cli {
            Explore::ClaudeTools
        } else {
            Explore::None
        };
        // The local planner gets a source overview in its prompt (Claude reads files with tools).
        let outline_budget = if local {
            self.profile(&settings.planner_profile)
                .map(|p| (p.context_limit / 5).min(12_000))
                .unwrap_or(0)
        } else {
            0
        };
        let (prompt, warnings) = claude::build_expand_prompt(&ExpandInputs {
            instruction: &req.instruction,
            root: &root,
            context_files: &req.context_files,
            profile: &profile,
            explore,
            outline_budget,
        })?;
        let cancel = self.register_job(&req.job_id);
        let res = if local {
            let _guard = self.run_lock.lock().await;
            crate::workflow::planner_answer(
                self,
                &settings,
                prompt.system,
                prompt.prompt,
                &root,
                crate::workflow::EventTarget::Job(&req.job_id),
                &cancel,
            )
            .await
        } else {
            let call = ClaudeCall {
                job_id: &req.job_id,
                system: prompt.system,
                prompt: prompt.prompt,
                cwd: Some(root.clone()),
                allow_read_tools: explore == Explore::ClaudeTools,
            };
            claude::run(&settings, call, &self.sink, &cancel, &self.stream_http).await
        };
        self.finish_job(&req.job_id);
        let res = res?;
        let spec = claude::clean_spec(&res.text);
        let parsed = parse_spec(&spec);
        if !res.cancelled {
            self.history.upsert_job(&JobRecord {
                id: req.job_id.clone(),
                created_at: String::new(),
                project_root: req.project_root.clone(),
                instruction: req.instruction.clone(),
                spec: spec.clone(),
                spec_edited: false,
                claude_backend: res.backend.clone(),
                claude_model: if res.model.is_empty() {
                    settings.claude_model.clone()
                } else {
                    res.model.clone()
                },
                expand_ms: Some(res.duration_ms as i64),
                expand_cost: res.cost_usd,
                context_files: req.context_files.clone(),
            })?;
        }
        Ok(ExpandResult {
            job_id: req.job_id,
            spec,
            parsed,
            claude: res,
            warnings,
        })
    }

    // ----- implement --------------------------------------------------------------------------

    /// Parse the spec, read the files and plan the budget. Token counts are exact (from the
    /// model's tokenizer) when `profile_id` is loaded on a server that offers `/tokenize`, and
    /// estimated otherwise.
    pub async fn prepare(
        &self,
        root: &str,
        spec: &str,
        context_files: &[String],
        full_context: &[String],
        profile_id: &str,
    ) -> Result<Prepared> {
        let profile = self.profile(profile_id)?;
        let system = templates::load(Template::ImplementSystem);
        let opts = self.context_options(full_context);
        let run = |count: &dyn Fn(&str) -> u32, exact: bool| {
            pipeline::prepare_with(Path::new(root), spec, context_files, &opts, &profile, &system, count, exact)
        };
        let Some(key) = self.tokenizer_key(&profile) else {
            return Ok(run(&estimate_tokens, false));
        };
        // First pass with what is cached; tokenize the rest, then plan again with exact counts.
        let misses = std::cell::RefCell::new(Vec::new());
        let first = run(
            &|t: &str| {
                self.tokens.cached(&key, t).unwrap_or_else(|| {
                    misses.borrow_mut().push(t.to_string());
                    estimate_tokens(t)
                })
            },
            true,
        );
        let misses = misses.into_inner();
        if misses.is_empty() {
            return Ok(first);
        }
        drop(first);
        if !self.tokens.fill(&self.http, &key, misses).await {
            return Ok(run(&estimate_tokens, false));
        }
        Ok(run(
            &|t: &str| self.tokens.cached(&key, t).unwrap_or_else(|| estimate_tokens(t)),
            true,
        ))
    }

    /// The tokenizer for `profile` if it is the loaded profile and its server can tokenize.
    fn tokenizer_key(&self, profile: &Profile) -> Option<TokenizerKey> {
        let st = self.server.status();
        if st.state != ServerState::Ready || st.profile_id.as_deref() != Some(profile.id.as_str()) {
            return None;
        }
        let (model, api_key) = match profile.kind {
            ProfileKind::Managed => (
                profile
                    .resolved_gguf(Path::new(&self.settings().models_dir))
                    .to_string_lossy()
                    .into_owned(),
                None,
            ),
            ProfileKind::External => (
                profile.external.model.clone(),
                if profile.external.use_api_key {
                    claude::get_secret(&Self::endpoint_key(&profile.id))
                } else {
                    None
                },
            ),
        };
        let key = TokenizerKey::new(&profile.id, &model, st.base_url.as_deref()?, api_key);
        (!self.tokens.is_unsupported(&key)).then_some(key)
    }

    pub(crate) fn context_options(&self, full_context: &[String]) -> ContextOptions {
        ContextOptions {
            outlines: self.settings().outline_context,
            full: full_context.to_vec(),
        }
    }

    /// What autopilot would do with `spec`: secure mode's verdict on its verification commands.
    pub fn autopilot_check(&self, project_root: &str, spec: &str) -> AutopilotCheck {
        let settings = self.settings();
        let plan = crate::workflow::plan(self, &settings, spec, Path::new(project_root));
        let secure = settings.autopilot_secure;
        AutopilotCheck {
            secure,
            sandboxed: !plan.commands.is_empty() && (secure || settings.sandbox_all_runs),
            blocked: if secure { crate::sandbox::blocked(&plan.commands) } else { vec![] },
            commands: plan.commands,
        }
    }

    pub async fn implement(&self, cmd: ImplementCommand) -> Result<ImplementResponse> {
        if let Some(t) = self.prewarm_cancel.lock().unwrap().take() {
            t.cancel();
        }
        let _guard = self.run_lock.lock().await;
        // The run replaces what a prewarm left in the server's cache.
        let prewarmed = self.last_prewarm.lock().unwrap().take();
        let settings = self.settings();
        let profile = self.profile(&cmd.profile_id)?;
        if !Path::new(&cmd.project_root).is_dir() {
            bail!("Project folder does not exist: {}", cmd.project_root);
        }
        let plan = crate::workflow::plan(self, &settings, &cmd.spec, Path::new(&cmd.project_root));
        let secure = cmd.autopilot && settings.autopilot_secure;
        if secure {
            let blocked = crate::sandbox::blocked(&plan.commands);
            if !blocked.is_empty() {
                bail!("{}", crate::sandbox::blocked_message(&blocked));
            }
        }
        let sandbox = secure || settings.sandbox_all_runs;
        let run_id = cmd.run_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.history.ensure_job(&cmd.job_id, &cmd.project_root, &cmd.instruction)?;
        self.history.update_job_spec(&cmd.job_id, &cmd.spec, &cmd.context_files)?;
        let cancel = self.register_job(&run_id);
        let endpoint = tokio::select! {
            e = self.ensure_active(&cmd.profile_id) => e,
            _ = cancel.cancelled() => Err(anyhow!("cancelled")),
        };
        let endpoint = match endpoint {
            Ok(e) => e,
            Err(e) => {
                self.finish_job(&run_id);
                return Err(e);
            }
        };
        if !plan.is_plain() {
            let input = crate::workflow::Input {
                run_id: &run_id,
                project_root: Path::new(&cmd.project_root),
                spec: &cmd.spec,
                context_files: &cmd.context_files,
                full_context: &cmd.full_context,
                mode: cmd.mode,
                coder: &profile,
                cancel: &cancel,
                prewarmed,
                sandbox,
            };
            let result = crate::workflow::run(self, input, plan).await;
            self.finish_job(&run_id);
            let result = result?;
            return self.finish_implement(&cmd, &settings, &profile, result);
        }
        let ctx = RunContext {
            sink: self.sink.clone(),
            http: self.stream_http.clone(),
            endpoint,
            profile: &profile,
            system_prompt: templates::load(Template::ImplementSystem),
            cancel: cancel.clone(),
        };
        let req = ImplementRequest {
            run_id: run_id.clone(),
            project_root: cmd.project_root.clone(),
            spec: cmd.spec.clone(),
            context_files: cmd.context_files.clone(),
            mode: cmd.mode,
            context_options: self.context_options(&cmd.full_context),
        };
        let prep = match self
            .prepare(&cmd.project_root, &cmd.spec, &cmd.context_files, &cmd.full_context, &cmd.profile_id)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                self.finish_job(&run_id);
                return Err(e);
            }
        };
        if cmd.mode.unwrap_or(prep.budget.recommended) == SplitMode::PerFile {
            self.prime_shared_context(&run_id, &profile, &ctx.endpoint, &prep, &cmd.spec, &cancel, prewarmed)
                .await;
        }
        let result = pipeline::implement_prepared(&ctx, &req, prep).await;
        self.finish_job(&run_id);
        self.finish_implement(&cmd, &settings, &profile, result)
    }

    /// Record a run in the history and decide whether to suggest the fallback model.
    fn finish_implement(
        &self,
        cmd: &ImplementCommand,
        settings: &Settings,
        profile: &Profile,
        result: ImplementResult,
    ) -> Result<ImplementResponse> {
        let model_file = match profile.kind {
            ProfileKind::Managed => profile
                .resolved_gguf(Path::new(&settings.models_dir))
                .to_string_lossy()
                .into_owned(),
            ProfileKind::External => format!("{} ({})", profile.external.base_url, profile.external.model),
        };
        self.history.save_run(&cmd.job_id, &cmd.spec, &model_file, &result)?;
        if let Some(rv) = result.workflow.as_ref().and_then(|w| w.review.as_ref()) {
            self.history.save_review(&result.run_id, &serde_json::to_value(rv)?)?;
        }
        let fallback_suggested = !result.validation_ok
            && !result.cancelled
            && settings.auto_fallback
            && settings.fallback_profile != profile.id
            && self.profile(&settings.fallback_profile).is_ok();
        Ok(ImplementResponse {
            result,
            fallback_suggested,
            fallback_profile: fallback_suggested.then(|| settings.fallback_profile.clone()),
        })
    }

    // ----- apply ------------------------------------------------------------------------------

    pub fn accept_file(&self, run_id: &str, path: &str, content: Option<String>, force: bool) -> Result<AcceptOutcome> {
        let (_, root, _) = self.history.run_context(run_id)?.ok_or_else(|| anyhow!("Unknown run `{run_id}`"))?;
        let rec = self
            .history
            .get_run_file(run_id, path)?
            .ok_or_else(|| anyhow!("`{path}` is not part of this run"))?;
        let content = content.unwrap_or(rec.proposed.clone());
        match project::write_file(Path::new(&root), path, &content, rec.original.as_deref(), force)? {
            Ok(()) => {
                self.history.set_decision(run_id, path, "accepted", Some(&content))?;
                Ok(AcceptOutcome {
                    ok: true,
                    conflict: None,
                    message: format!("Wrote {path}"),
                })
            }
            Err(c) => Ok(AcceptOutcome {
                ok: false,
                message: match c {
                    WriteConflict::ChangedOnDisk => format!("`{path}` changed on disk since the run started. Overwrite anyway?"),
                    WriteConflict::NowExists => format!("`{path}` was created on disk since the run started. Overwrite it?"),
                },
                conflict: Some(c),
            }),
        }
    }

    pub fn reject_file(&self, run_id: &str, path: &str) -> Result<()> {
        self.history.set_decision(run_id, path, "rejected", None)
    }

    // ----- review -----------------------------------------------------------------------------

    pub async fn review(&self, run_id: &str, job_id_for_events: &str) -> Result<Review> {
        let settings = self.settings();
        let (_, root, spec) = self.history.run_context(run_id)?.ok_or_else(|| anyhow!("Unknown run `{run_id}`"))?;
        let files = self.history.run_files(run_id)?;
        let accepted: Vec<_> = files.iter().filter(|f| f.decision == "accepted").collect();
        if accepted.is_empty() {
            bail!("Accept at least one file before running the review");
        }
        let mut diff = String::new();
        let mut after = Vec::new();
        let mut after_chars = 0;
        for f in &accepted {
            let content = f.final_content.clone().unwrap_or_else(|| f.proposed.clone());
            diff.push_str(&project::unified_diff(&f.path, f.original.as_deref(), &content));
            diff.push('\n');
            after_chars += content.len();
            after.push((f.path.clone(), content));
        }
        if after_chars > 200_000 {
            after.clear(); // the diff alone; Claude CLI can still read files with tools
        }
        let prompt = claude::build_review_prompt(
            &spec,
            &diff,
            &after,
            "Not run for this review (the accepted files are reviewed in the project).",
        );
        let cancel = self.register_job(job_id_for_events);
        let res = {
            let _guard = self.run_lock.lock().await;
            crate::workflow::planner_answer(
                self,
                &settings,
                None,
                prompt,
                Path::new(&root),
                crate::workflow::EventTarget::Job(job_id_for_events),
                &cancel,
            )
            .await
        };
        self.finish_job(job_id_for_events);
        let res = res?;
        if res.cancelled {
            bail!("Review cancelled");
        }
        let review = claude::parse_review(&res.text);
        self.history.save_review(run_id, &serde_json::to_value(&review)?)?;
        Ok(review)
    }

    pub async fn shutdown(&self) {
        for t in self.jobs.lock().unwrap().values() {
            t.cancel();
        }
        let _ = self.server.stop().await;
    }
}

/// Identifies what a prewarm leaves in the server's cache: the loaded server and the prefixes.
fn prewarm_signature(profile: &Profile, st: &ServerStatus, points: &[pipeline::PrimePoint]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (&profile.id, st.pid, &st.base_url).hash(&mut h);
    for p in points {
        p.prefix_text().hash(&mut h);
    }
    h.finish()
}
