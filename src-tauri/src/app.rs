//! Application core shared by the Tauri commands and the headless e2e harness.

use crate::budget::SplitMode;
use crate::claude::{self, ClaudeCall, ClaudeResult, ExpandInputs, Review};
use crate::config::{self, Profile, ProfileKind, Settings, builtin_profiles};
use crate::editformat::{Workspace, parse_blocks};
use crate::events::Sink;
use crate::history::{History, JobRecord};
use crate::llm::{ChatMessage, Endpoint, build_body, stream_chat};
use crate::pipeline::{self, ContextOptions, ImplementRequest, ImplementResult, Prepared, RunContext};
use crate::project::{self, WriteConflict};
use crate::reasoning::strip_reasoning;
use crate::server::{ServerManager, ServerState, ServerStatus};
use crate::spec::{ParsedSpec, parse_spec};
use crate::templates::{self, Template};
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
        Ok(match profile.kind {
            ProfileKind::Managed => Endpoint {
                base_url: st.base_url.unwrap_or_default(),
                model: String::new(),
                api_key: None,
            },
            ProfileKind::External => Endpoint {
                base_url: profile.external.base_url.trim_end_matches('/').to_string(),
                model: profile.external.model.clone(),
                api_key: if profile.external.use_api_key {
                    claude::get_secret(&Self::endpoint_key(profile_id))
                } else {
                    None
                },
            },
        })
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
        let explore = settings.claude_cli_explore && settings.claude_backend == config::ClaudeBackend::Cli;
        let (prompt, warnings) = claude::build_expand_prompt(&ExpandInputs {
            instruction: &req.instruction,
            root: &root,
            context_files: &req.context_files,
            profile: &profile,
            explore,
        })?;
        let cancel = self.register_job(&req.job_id);
        let call = ClaudeCall {
            job_id: &req.job_id,
            prompt,
            cwd: Some(root.clone()),
            allow_read_tools: explore,
        };
        let res = claude::run(&settings, call, &self.sink, &cancel, &self.stream_http).await;
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

    pub fn prepare(
        &self,
        root: &str,
        spec: &str,
        context_files: &[String],
        full_context: &[String],
        profile_id: &str,
    ) -> Result<Prepared> {
        let profile = self.profile(profile_id)?;
        Ok(pipeline::prepare(
            Path::new(root),
            spec,
            context_files,
            &self.context_options(full_context),
            &profile,
            &templates::load(Template::ImplementSystem),
        ))
    }

    fn context_options(&self, full_context: &[String]) -> ContextOptions {
        ContextOptions {
            outlines: self.settings().outline_context,
            full: full_context.to_vec(),
        }
    }

    pub async fn implement(&self, cmd: ImplementCommand) -> Result<ImplementResponse> {
        let _guard = self.run_lock.lock().await;
        let settings = self.settings();
        let profile = self.profile(&cmd.profile_id)?;
        if !Path::new(&cmd.project_root).is_dir() {
            bail!("Project folder does not exist: {}", cmd.project_root);
        }
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
        let result = pipeline::implement(&ctx, &req).await;
        self.finish_job(&run_id);
        let model_file = match profile.kind {
            ProfileKind::Managed => profile
                .resolved_gguf(Path::new(&settings.models_dir))
                .to_string_lossy()
                .into_owned(),
            ProfileKind::External => format!("{} ({})", profile.external.base_url, profile.external.model),
        };
        self.history.save_run(&cmd.job_id, &cmd.spec, &model_file, &result)?;
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
        let prompt = claude::build_review_prompt(&spec, &diff, &after);
        let explore = settings.claude_cli_explore && settings.claude_backend == config::ClaudeBackend::Cli;
        let cancel = self.register_job(job_id_for_events);
        let call = ClaudeCall {
            job_id: job_id_for_events,
            prompt,
            cwd: Some(PathBuf::from(&root)),
            allow_read_tools: explore,
        };
        let res = claude::run(&settings, call, &self.sink, &cancel, &self.stream_http).await;
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
