//! Managed llama-server lifecycle (one model in VRAM at a time) and external-endpoint probing.

use crate::config::{Profile, ProfileKind, Settings, app_dir, split_args};
use crate::events::{Sink, emit};
use crate::winproc;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const LOG_CAP: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServerState {
    Unloaded,
    Loading,
    Ready,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub state: ServerState,
    pub profile_id: Option<String>,
    pub profile_name: Option<String>,
    pub external: bool,
    pub message: String,
    /// 0..1 while loading (estimated from load phases and previous load times).
    pub progress: Option<f32>,
    pub base_url: Option<String>,
    pub hint: Option<String>,
    pub load_secs: Option<f64>,
    pub pid: Option<u32>,
}

impl ServerStatus {
    fn unloaded() -> Self {
        Self {
            state: ServerState::Unloaded,
            profile_id: None,
            profile_name: None,
            external: false,
            message: "No model loaded".into(),
            progress: None,
            base_url: None,
            hint: None,
            load_secs: None,
            pid: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PidFile {
    pid: u32,
    exe: String,
}

fn pid_file() -> PathBuf {
    app_dir().join("run").join("llama-server.pid")
}

/// Kill a llama-server left behind by a previous Relay session (crash, forced exit). Only the
/// process recorded in our PID file is touched, and only if its image path still matches, so a
/// llama-server you started yourself (external mode) is never killed.
pub fn cleanup_orphans() -> Option<String> {
    let path = pid_file();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let pf: PidFile = serde_json::from_str(&text).ok()?;
    let image = winproc::image_path(pf.pid)?;
    if same_path(&image, &pf.exe) && winproc::kill_pid(pf.pid) {
        return Some(format!("Killed orphaned llama-server (pid {}) from a previous session", pf.pid));
    }
    None
}

fn norm(p: &str) -> String {
    p.trim_start_matches(r"\\?\").replace('/', "\\").to_lowercase()
}

/// True if both paths name the same file. Resolves `..`, case, slashes, 8.3 short names and links
/// via the filesystem; falls back to a normalised string comparison if a path can't be resolved.
pub fn same_path(a: &str, b: &str) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => norm(&x.to_string_lossy()) == norm(&y.to_string_lossy()),
        _ => norm(a) == norm(b),
    }
}

/// Find llama-server.exe: explicit setting → bundled next to Relay → %LOCALAPPDATA%\Relay\llama.cpp → PATH.
pub fn locate_llama_server(settings: &Settings) -> Option<PathBuf> {
    let explicit = settings.llama_server_path.trim();
    if !explicit.is_empty() {
        let p = PathBuf::from(explicit);
        return p.is_file().then_some(p);
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("llama.cpp").join("llama-server.exe"));
        candidates.push(dir.join("resources").join("llama.cpp").join("llama-server.exe"));
    }
    candidates.push(app_dir().join("llama.cpp").join("llama-server.exe"));
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
    }
    which::which("llama-server").ok()
}

/// Arguments for llama-server. Profile launch args come first; Relay's required arguments come
/// last so the profile's context limit (used for budgeting) is always what the server uses.
pub fn build_args(profile: &Profile, gguf: &Path, port: u16) -> Vec<String> {
    let mut args = split_args(&profile.launch_args);
    args.extend([
        "-m".to_string(),
        gguf.to_string_lossy().into_owned(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "-c".into(),
        profile.context_limit.to_string(),
        "-np".into(),
        "1".into(),
        "--log-colors".into(),
        "off".into(),
    ]);
    args
}

/// Map llama-server output / exit codes to an actionable hint.
pub fn diagnose(log_tail: &[String], exit_code: Option<i32>) -> Option<String> {
    let text = log_tail.join("\n").to_lowercase();
    if exit_code == Some(-1073741515) || text.contains("cudart64") && text.contains("not found") {
        return Some("llama-server.exe could not start because a DLL is missing. Extract the matching cudart zip (CUDA runtime DLLs) into the same folder as llama-server.exe.".into());
    }
    if text.contains("out of memory")
        || text.contains("cudamalloc failed")
        || text.contains("failed to allocate")
        || text.contains("shared object initialization failed")
        || text.contains("cuda error")
    {
        return Some("Not enough free VRAM. Close other GPU-heavy apps (games, hardware-accelerated browsers), lower the context limit, use a smaller quant, or for the MoE profile raise --n-cpu-moe.".into());
    }
    if text.contains("error loading model")
        || text.contains("failed to load model")
        || text.contains("unknown model architecture")
        || text.contains("invalid magic")
    {
        return Some(
            "The GGUF could not be loaded: the file may be incomplete/corrupt, or this llama.cpp build is too old for the model.".into(),
        );
    }
    if text.contains("couldn't bind") || text.contains("address already in use") || text.contains("failed to bind") {
        return Some("The port is already in use. Change the managed-server port in Settings.".into());
    }
    if text.contains("unknown argument") || text.contains("invalid argument") || text.contains("error: invalid") {
        return Some(
            "llama-server rejected a launch argument. Check the profile's launch arguments against `llama-server --help` for your build."
                .into(),
        );
    }
    None
}

/// Load phase from a log line → progress floor (0..1) and label.
fn phase_of(line: &str) -> Option<(f32, &'static str)> {
    if line.contains("loading model") {
        Some((0.03, "Reading model file"))
    } else if line.contains("llama threadpool init") || line.contains("load_tensors: offloaded") {
        Some((0.85, "Weights loaded"))
    } else if line.contains("initializing, n_slots") || line.contains("llama_context") {
        Some((0.90, "Allocating context"))
    } else if line.contains("model loaded") {
        Some((0.97, "Warming up"))
    } else if line.contains("listening on") {
        Some((0.99, "Starting HTTP server"))
    } else {
        None
    }
}

fn is_error_line(line: &str) -> bool {
    let l = line.to_lowercase();
    line.contains(" E ") || l.contains("error") || l.contains("failed to") || l.contains("out of memory") || l.contains("exception")
}

struct Inner {
    child: Option<Child>,
}

pub struct ServerManager {
    sink: Sink,
    inner: Mutex<Inner>,
    status: StdMutex<ServerStatus>,
    log: Arc<StdMutex<VecDeque<String>>>,
    phase: Arc<StdMutex<(f32, String)>>,
    errors: Arc<StdMutex<Vec<String>>>,
    generation: AtomicU64,
    http: reqwest::Client,
}

impl ServerManager {
    pub fn new(sink: Sink) -> Self {
        Self {
            sink,
            inner: Mutex::new(Inner { child: None }),
            status: StdMutex::new(ServerStatus::unloaded()),
            log: Arc::new(StdMutex::new(VecDeque::new())),
            phase: Arc::new(StdMutex::new((0.0, String::new()))),
            errors: Arc::new(StdMutex::new(Vec::new())),
            generation: AtomicU64::new(0),
            http: reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
        }
    }

    pub fn status(&self) -> ServerStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn log_lines(&self) -> Vec<String> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    fn set_status(&self, st: ServerStatus) {
        *self.status.lock().unwrap() = st.clone();
        emit(&self.sink, "server-status", &st);
    }

    fn push_log(log: &Arc<StdMutex<VecDeque<String>>>, sink: &Sink, line: String) {
        {
            let mut l = log.lock().unwrap();
            if l.len() >= LOG_CAP {
                l.pop_front();
            }
            l.push_back(line.clone());
        }
        emit(sink, "server-log", &line);
    }

    fn note(&self, line: String) {
        Self::push_log(&self.log, &self.sink, format!("[relay] {line}"));
    }

    /// Stop the managed server (if any) and wait until the process has exited (VRAM released).
    pub async fn stop(&self) -> Result<()> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut inner = self.inner.lock().await;
        if let Some(mut child) = inner.child.take() {
            let pid = child.id();
            let mut exited = false;
            if let Some(pid) = pid
                && winproc::send_ctrl_c(pid)
            {
                exited = tokio::time::timeout(Duration::from_secs(4), child.wait()).await.is_ok();
            }
            if !exited {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
            }
            self.note(format!("llama-server stopped (pid {})", pid.unwrap_or(0)));
        }
        let _ = std::fs::remove_file(pid_file());
        self.set_status(ServerStatus::unloaded());
        Ok(())
    }

    /// Switch to `profile`: unloads the current model, then loads the new one (managed) or
    /// connects to the endpoint (external). Returns when ready, failed, or superseded.
    pub async fn activate(&self, profile: &Profile, settings: &Settings, api_key: Option<String>) -> Result<ServerStatus> {
        self.stop().await?;
        let generation_id = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let base = ServerStatus {
            state: ServerState::Loading,
            profile_id: Some(profile.id.clone()),
            profile_name: Some(profile.name.clone()),
            external: profile.kind == ProfileKind::External,
            message: String::new(),
            progress: Some(0.0),
            base_url: None,
            hint: None,
            load_secs: None,
            pid: None,
        };
        match profile.kind {
            ProfileKind::External => self.activate_external(profile, base, api_key).await,
            ProfileKind::Managed => self.activate_managed(profile, settings, base, generation_id).await,
        }
    }

    async fn activate_external(&self, profile: &Profile, mut st: ServerStatus, api_key: Option<String>) -> Result<ServerStatus> {
        let url = profile.external.base_url.trim().trim_end_matches('/').to_string();
        st.message = format!("Connecting to {url}");
        self.set_status(st.clone());
        let r = probe_endpoint(&self.http, &url, api_key.as_deref()).await;
        match r {
            Ok(models) => {
                st.state = ServerState::Ready;
                st.progress = None;
                st.base_url = Some(url.clone());
                st.message = if models.is_empty() {
                    format!("Connected to {url}")
                } else {
                    format!("Connected to {url} ({})", models.join(", "))
                };
                self.note(st.message.clone());
            }
            Err(e) => {
                st.state = ServerState::Error;
                st.progress = None;
                st.message = format!("Endpoint not reachable: {e}");
                st.hint = Some("Start LM Studio / Ollama / your llama-server, and check the base URL (it should end in /v1).".into());
            }
        }
        self.set_status(st.clone());
        Ok(st)
    }

    async fn activate_managed(
        &self,
        profile: &Profile,
        settings: &Settings,
        mut st: ServerStatus,
        generation_id: u64,
    ) -> Result<ServerStatus> {
        let fail = |mut st: ServerStatus, msg: String, hint: Option<String>| {
            st.state = ServerState::Error;
            st.progress = None;
            st.message = msg;
            st.hint = hint;
            st
        };
        let Some(exe) = locate_llama_server(settings) else {
            let st = fail(
                st,
                "llama-server.exe not found".into(),
                Some("Set the llama-server path in Settings (a CUDA build of llama.cpp), or put it on PATH.".into()),
            );
            self.set_status(st.clone());
            return Ok(st);
        };
        let models_dir = PathBuf::from(&settings.models_dir);
        let gguf = profile.resolved_gguf(&models_dir);
        if !gguf.is_file() {
            let st = fail(
                st,
                format!("Model file not found: {}", gguf.display()),
                Some("Point the profile at a GGUF on disk, or download it from the profile settings.".into()),
            );
            self.set_status(st.clone());
            return Ok(st);
        }
        let port = pick_port(settings.server_port);
        let args = build_args(profile, &gguf, port);
        self.log.lock().unwrap().clear();
        self.errors.lock().unwrap().clear();
        *self.phase.lock().unwrap() = (0.0, "Starting llama-server".into());
        self.note(format!("{} {}", exe.display(), args.join(" ")));

        let mut cmd = Command::new(&exe);
        cmd.args(&args)
            .current_dir(exe.parent().unwrap_or(Path::new(".")))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        winproc::hide_window(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let st = fail(st, format!("Failed to start llama-server: {e}"), None);
                self.set_status(st.clone());
                return Ok(st);
            }
        };
        let pid = child.id();
        if let Some(pid) = pid {
            if !winproc::assign_to_global_job(pid) {
                self.note("warning: could not assign llama-server to the job object; it may survive a Relay crash".into());
            }
            let resolved = std::fs::canonicalize(&exe).unwrap_or_else(|_| exe.clone());
            let pf = PidFile {
                pid,
                exe: resolved.to_string_lossy().trim_start_matches(r"\\?\").to_string(),
            };
            if let Some(dir) = pid_file().parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(pid_file(), serde_json::to_string(&pf).unwrap_or_default());
        }
        if let Some(out) = child.stdout.take() {
            self.spawn_reader(out);
        }
        if let Some(err) = child.stderr.take() {
            self.spawn_reader(err);
        }
        {
            let mut inner = self.inner.lock().await;
            if self.generation.load(Ordering::SeqCst) != generation_id {
                // A newer switch/stop happened while we were spawning; don't leak this process.
                let _ = child.start_kill();
                return Ok(self.status());
            }
            inner.child = Some(child);
        }

        st.pid = pid;
        let base_url = format!("http://127.0.0.1:{port}/v1");
        let health = format!("http://127.0.0.1:{port}/health");
        let expected = settings
            .load_stats
            .get(&profile.id)
            .copied()
            .unwrap_or_else(|| 3.0 + std::fs::metadata(&gguf).map(|m| m.len() as f64 / 1.2e9).unwrap_or(15.0));
        let started = Instant::now();
        let timeout = Duration::from_secs(settings.load_timeout_secs.max(30));
        let mut last_emit = Instant::now() - Duration::from_secs(1);

        loop {
            if self.generation.load(Ordering::SeqCst) != generation_id {
                // Superseded by another switch or a stop.
                return Ok(self.status());
            }
            // Process exited?
            let exit = {
                let mut inner = self.inner.lock().await;
                match inner.child.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(status))) => Some(status.code()),
                    Some(Err(_)) | None => Some(None),
                    Some(Ok(None)) => None,
                }
            };
            if let Some(code) = exit {
                tokio::time::sleep(Duration::from_millis(300)).await; // let readers drain
                self.inner.lock().await.child = None;
                let _ = std::fs::remove_file(pid_file());
                let errors = self.errors.lock().unwrap().clone();
                let tail: Vec<String> = {
                    let l = self.log.lock().unwrap();
                    l.iter().rev().take(40).rev().cloned().collect()
                };
                let last_err = errors.last().cloned().unwrap_or_else(|| tail.last().cloned().unwrap_or_default());
                let st = fail(
                    st.clone(),
                    format!(
                        "llama-server exited{} — {}",
                        code.map(|c| format!(" (code {c})")).unwrap_or_default(),
                        last_err.trim()
                    ),
                    diagnose(&[errors, tail].concat(), code),
                );
                self.set_status(st.clone());
                return Ok(st);
            }
            if started.elapsed() > timeout {
                let st = fail(
                    st.clone(),
                    format!("Timed out after {}s waiting for llama-server to become healthy", timeout.as_secs()),
                    None,
                );
                self.set_status(st.clone());
                let _ = self.stop_keep_status().await;
                return Ok(st);
            }
            if let Ok(resp) = self.http.get(&health).timeout(Duration::from_secs(2)).send().await
                && resp.status().is_success()
            {
                let secs = started.elapsed().as_secs_f64();
                st.state = ServerState::Ready;
                st.progress = None;
                st.base_url = Some(base_url.clone());
                st.load_secs = Some(secs);
                st.message = format!("Ready on port {port} (loaded in {secs:.1}s)");
                self.note(st.message.clone());
                self.set_status(st.clone());
                return Ok(st);
            }
            if last_emit.elapsed() >= Duration::from_millis(400) {
                let (floor, label) = self.phase.lock().unwrap().clone();
                let elapsed = started.elapsed().as_secs_f64();
                let est = (0.03 + 0.82 * (elapsed / expected).min(0.97)) as f32;
                st.progress = Some(floor.max(est).min(0.99));
                st.message = format!(
                    "{} — {:.0}s (est. {:.0}s)",
                    if label.is_empty() { "Starting" } else { &label },
                    elapsed,
                    expected
                );
                self.set_status(st.clone());
                last_emit = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn stop_keep_status(&self) -> Result<()> {
        let st = self.status();
        self.stop().await?;
        self.set_status(st);
        Ok(())
    }

    fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(&self, stream: R) {
        let log = self.log.clone();
        let sink = self.sink.clone();
        let phase = self.phase.clone();
        let errors = self.errors.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                        if line.is_empty() {
                            continue;
                        }
                        if let Some((p, label)) = phase_of(&line) {
                            let mut ph = phase.lock().unwrap();
                            if p > ph.0 {
                                *ph = (p, label.to_string());
                            }
                        }
                        if is_error_line(&line) && !line.contains("unused tensor") {
                            let mut e = errors.lock().unwrap();
                            if e.len() > 50 {
                                e.remove(0);
                            }
                            e.push(line.clone());
                        }
                        Self::push_log(&log, &sink, line);
                    }
                }
            }
        });
    }
}

fn pick_port(preferred: u16) -> u16 {
    for p in preferred..preferred.saturating_add(20) {
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
    preferred
}

/// GET {base}/models on an OpenAI-compatible endpoint; returns model ids.
pub async fn probe_endpoint(http: &reqwest::Client, base_url: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    if base_url.is_empty() {
        return Err(anyhow!("no base URL configured"));
    }
    let mut req = http
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .timeout(Duration::from_secs(5));
    if let Some(k) = api_key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.unwrap_or_default();
    Ok(v.get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                .take(5)
                .collect()
        })
        .unwrap_or_default())
}

/// VRAM usage via nvidia-smi: (used MiB, total MiB).
pub fn gpu_memory() -> Option<(u64, u64)> {
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=memory.used,memory.total", "--format=csv,noheader,nounits"]);
    winproc::hide_window_std(&mut cmd);
    let out = cmd.output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let first = s.lines().next()?;
    let mut parts = first.split(',').map(|p| p.trim().parse::<u64>().ok());
    Some((parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_profiles;

    #[test]
    fn args_put_relay_flags_last() {
        let p = &builtin_profiles()[1];
        let a = build_args(p, Path::new("D:/m/q.gguf"), 4000);
        assert_eq!(&a[..4], &["-ngl", "all", "--n-cpu-moe", "18"]);
        let c = a.iter().position(|x| x == "-c").unwrap();
        assert_eq!(a[c + 1], "65536");
        assert!(a.windows(2).any(|w| w == ["-np", "1"]));
        assert!(a.windows(2).any(|w| w == ["--port", "4000"]));
    }

    #[test]
    fn diagnoses_common_failures() {
        let vram = diagnose(&["0.20 E CUDA error: shared object initialization failed".into()], Some(1)).unwrap();
        assert!(vram.contains("VRAM"));
        let dll = diagnose(&[], Some(-1073741515)).unwrap();
        assert!(dll.contains("DLL"));
        assert!(
            diagnose(&["llama_model_load: error loading model: invalid magic".into()], Some(1))
                .unwrap()
                .contains("GGUF")
        );
        assert!(diagnose(&["all good".into()], Some(0)).is_none());
    }

    #[test]
    fn same_path_resolves_dotdot_case_and_slashes() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("bin")).unwrap();
        std::fs::create_dir_all(d.path().join("other")).unwrap();
        std::fs::write(d.path().join("bin").join("llama-server.exe"), b"x").unwrap();
        std::fs::write(d.path().join("other").join("llama-server.exe"), b"x").unwrap();
        let direct = d.path().join("bin").join("llama-server.exe").to_string_lossy().into_owned();
        let dotted = d
            .path()
            .join("other")
            .join("..")
            .join("bin")
            .join("llama-server.exe")
            .to_string_lossy()
            .into_owned();
        assert!(same_path(&direct, &dotted), "`..` must resolve to the same file");
        assert!(same_path(&direct, &direct.to_uppercase().replace('\\', "/")));
        let other = d.path().join("other").join("llama-server.exe").to_string_lossy().into_owned();
        assert!(!same_path(&direct, &other));
        // Missing files fall back to a normalised string compare.
        assert!(same_path(r"C:\Missing\X.exe", "c:/missing/x.exe"));
        assert!(!same_path(r"C:\Missing\X.exe", r"C:\Missing\Y.exe"));
    }

    #[test]
    fn phases_progress_monotonic_labels() {
        assert_eq!(phase_of("I srv    load_model: loading model 'x'").unwrap().0, 0.03);
        assert_eq!(phase_of("I srv  server_run_i: model loaded").unwrap().0, 0.97);
        assert!(phase_of("random").is_none());
    }
}
