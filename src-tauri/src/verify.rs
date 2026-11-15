//! Verification: the build / test / run commands for a change, detected from the project or
//! taken from the spec's `## Verification` section, run in the staging copy with a timeout and
//! a hidden console. Their output is condensed for the coder's fix round.

use crate::events::{Sink, emit};
use crate::winproc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "camelCase")]
pub enum VerifyKind {
    /// Prepare the staging copy (e.g. install dependencies). Runs once per run.
    Setup,
    Build,
    Test,
    /// Start the program: passes if it exits with 0, or is still running when the timeout ends
    /// (a game or server that started fine), and its output contains `expect` if given.
    Run,
}

impl VerifyKind {
    pub fn word(self) -> &'static str {
        match self {
            VerifyKind::Setup => "setup",
            VerifyKind::Build => "build",
            VerifyKind::Test => "test",
            VerifyKind::Run => "run",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "setup" | "install" | "prepare" => Some(VerifyKind::Setup),
            "build" | "compile" | "check" | "lint" => Some(VerifyKind::Build),
            "test" | "tests" | "unit tests" => Some(VerifyKind::Test),
            "run" | "start" | "smoke" | "smoke test" => Some(VerifyKind::Run),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "camelCase")]
pub struct VerifyCommand {
    pub kind: VerifyKind,
    /// A command line for `cmd /C`, run in the project root.
    pub command: String,
    /// Seconds; `None` → the default for the kind (Settings).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// `run` only: text the output must contain.
    #[serde(default)]
    pub expect: Option<String>,
}

impl VerifyCommand {
    pub fn new(kind: VerifyKind, command: &str) -> Self {
        Self {
            kind,
            command: command.to_string(),
            timeout_secs: None,
            expect: None,
        }
    }

    /// The spec line for this command, as the planner is asked to write it.
    pub fn spec_line(&self) -> String {
        let mut extra = Vec::new();
        if let Some(t) = self.timeout_secs {
            extra.push(format!("timeout {t}s"));
        }
        if let Some(e) = &self.expect {
            extra.push(format!("expect \"{e}\""));
        }
        let extra = if extra.is_empty() {
            String::new()
        } else {
            format!(" ({})", extra.join(", "))
        };
        format!("- {}: `{}`{extra}", self.kind.word(), self.command)
    }
}

static ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:[-*]|\d+\.)\s*\**([a-z ]+?)\**\s*[:—–-]\s*`([^`]+)`(.*)$").unwrap());
static TIMEOUT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)timeout\s*(?:of\s*)?(\d+)\s*(?:s\b|sec|second)").unwrap());
static EXPECT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)expect\w*\s*["“`']([^"”`']+)["”`']"#).unwrap());

/// Parse one bullet of a `## Verification` section: `- build: `cargo build``, optionally followed
/// by `(timeout 30s)` and, for run, `expect "text"`. `Err` carries a message for unreadable items;
/// `Ok(None)` is a bullet that says there is nothing to run.
pub fn parse_verification_item(line: &str) -> Result<Option<VerifyCommand>, String> {
    let t = line.trim();
    let bare = t
        .trim_start_matches(['-', '*', ' '])
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if bare.is_empty()
        || bare == "none"
        || bare.starts_with("none ")
        || bare.starts_with("none,")
        || bare.starts_with("no command")
        || bare.starts_with("nothing to")
    {
        return Ok(None);
    }
    let Some(cap) = ITEM.captures(t) else {
        return Err(format!("Could not read a command in Verification entry: {t}"));
    };
    let Some(kind) = VerifyKind::parse(&cap[1]) else {
        return Err(format!(
            "Unknown verification step `{}` (use setup, build, test or run): {t}",
            cap[1].trim()
        ));
    };
    let command = cap[2].trim().to_string();
    if command.is_empty() {
        return Err(format!("Empty command in Verification entry: {t}"));
    }
    let rest = &cap[3];
    Ok(Some(VerifyCommand {
        kind,
        command,
        timeout_secs: TIMEOUT.captures(rest).and_then(|c| c[1].parse().ok()),
        expect: EXPECT.captures(rest).map(|c| c[1].to_string()).filter(|_| kind == VerifyKind::Run),
    }))
}

// ---------------------------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Detected {
    pub commands: Vec<VerifyCommand>,
    /// What the commands were derived from (e.g. "Cargo.toml").
    pub sources: Vec<String>,
    pub warnings: Vec<String>,
}

/// Directory names never searched while detecting (build output, dependencies, VCS).
const DETECT_SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    "bin",
    "obj",
];

fn any_file(root: &Path, max_depth: usize, pred: &dyn Fn(&str, &Path) -> bool) -> bool {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .max_depth(Some(max_depth))
        .filter_entry(|e| !DETECT_SKIP.iter().any(|s| e.file_name().eq_ignore_ascii_case(s)))
        .build();
    walker
        .flatten()
        .take(5000)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .any(|e| pred(&e.file_name().to_string_lossy().to_ascii_lowercase(), e.path()))
}

fn file_contains(path: &Path, needle: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|t| t.contains(needle))
}

fn need(tool: &str, d: &mut Detected) {
    if which::which(tool).is_err() {
        d.warnings
            .push(format!("`{tool}` is not on PATH, so the detected {tool} commands will fail."));
    }
}

/// The build / test commands this project most likely uses, from its manifest files. Tools that
/// are not on PATH are reported in `warnings` (their commands are still listed: the user may fix
/// PATH, or edit them in the spec).
pub fn detect(root: &Path) -> Detected {
    let mut d = Detected::default();
    let has = |name: &str| root.join(name).is_file();

    if has("Cargo.toml") {
        d.sources.push("Cargo.toml".into());
        need("cargo", &mut d);
        d.commands.push(VerifyCommand::new(VerifyKind::Build, "cargo build"));
        let has_tests = root.join("tests").is_dir() || any_file(root, 6, &|n, p| n.ends_with(".rs") && file_contains(p, "#[test]"));
        if has_tests {
            d.commands.push(VerifyCommand::new(VerifyKind::Test, "cargo test"));
        }
    }
    if has("package.json") {
        d.sources.push("package.json".into());
        need("npm", &mut d);
        let pkg: serde_json::Value = std::fs::read_to_string(root.join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let deps = ["dependencies", "devDependencies"]
            .iter()
            .any(|k| pkg[*k].as_object().is_some_and(|o| !o.is_empty()));
        if deps && !root.join("node_modules").is_dir() {
            let cmd = if has("package-lock.json") { "npm ci" } else { "npm install" };
            d.commands.push(VerifyCommand::new(VerifyKind::Setup, cmd));
        }
        let script = |name: &str| pkg["scripts"][name].as_str().map(str::to_string);
        if script("build").is_some() {
            d.commands.push(VerifyCommand::new(VerifyKind::Build, "npm run build"));
        } else if has("tsconfig.json") && pkg["devDependencies"]["typescript"].is_string() {
            d.commands.push(VerifyCommand::new(VerifyKind::Build, "npx tsc --noEmit"));
        }
        if script("test").is_some_and(|t| !t.contains("no test specified")) {
            d.commands.push(VerifyCommand::new(VerifyKind::Test, "npm test"));
        }
    }
    let py_manifest = ["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"]
        .iter()
        .find(|f| has(f));
    let py_files = py_manifest.is_some() || any_file(root, 3, &|n, _| n.ends_with(".py"));
    if py_files && !has("Cargo.toml") && !has("package.json") {
        d.sources.push(py_manifest.map(|s| s.to_string()).unwrap_or_else(|| "*.py".into()));
        need("python", &mut d);
        d.commands.push(VerifyCommand::new(VerifyKind::Build, "python -m compileall -q ."));
        let has_tests = any_file(root, 6, &|n, _| {
            n.ends_with(".py") && (n.starts_with("test_") || n.ends_with("_test.py"))
        });
        if has_tests {
            let mentions_pytest = [
                "pyproject.toml",
                "requirements.txt",
                "requirements-dev.txt",
                "setup.cfg",
                "tox.ini",
                "pytest.ini",
            ]
            .iter()
            .any(|f| file_contains(&root.join(f), "pytest"));
            let cmd = if mentions_pytest {
                "python -m pytest -q"
            } else {
                "python -m unittest discover -q"
            };
            d.commands.push(VerifyCommand::new(VerifyKind::Test, cmd));
        }
    }
    if has("go.mod") {
        d.sources.push("go.mod".into());
        need("go", &mut d);
        d.commands.push(VerifyCommand::new(VerifyKind::Build, "go build ./..."));
        if any_file(root, 8, &|n, _| n.ends_with("_test.go")) {
            d.commands.push(VerifyCommand::new(VerifyKind::Test, "go test ./..."));
        }
    }
    let dotnet = any_file(root, 2, &|n, _| {
        n.ends_with(".sln") || n.ends_with(".csproj") || n.ends_with(".fsproj")
    });
    if dotnet && d.commands.is_empty() {
        d.sources.push(".NET project".into());
        need("dotnet", &mut d);
        d.commands.push(VerifyCommand::new(VerifyKind::Build, "dotnet build"));
        if any_file(root, 3, &|n, p| {
            n.ends_with(".csproj") && file_contains(p, "Microsoft.NET.Test.Sdk")
        }) {
            d.commands.push(VerifyCommand::new(VerifyKind::Test, "dotnet test"));
        }
    }
    if has("CMakeLists.txt") && d.commands.is_empty() {
        d.sources.push("CMakeLists.txt".into());
        need("cmake", &mut d);
        d.commands.push(VerifyCommand::new(VerifyKind::Build, "cmake -S . -B build"));
        d.commands.push(VerifyCommand::new(VerifyKind::Build, "cmake --build build"));
    }
    if has("pom.xml") && d.commands.is_empty() {
        d.sources.push("pom.xml".into());
        need("mvn", &mut d);
        d.commands.push(VerifyCommand::new(VerifyKind::Build, "mvn -q compile"));
        d.commands.push(VerifyCommand::new(VerifyKind::Test, "mvn -q test"));
    }
    if (has("gradlew.bat") || has("gradlew")) && d.commands.is_empty() {
        d.sources.push("Gradle wrapper".into());
        d.commands.push(VerifyCommand::new(VerifyKind::Build, "gradlew.bat build -x test"));
        d.commands.push(VerifyCommand::new(VerifyKind::Test, "gradlew.bat test"));
    }
    d.commands.sort_by_key(|c| c.kind); // stable: setup, build, test, run
    d
}

/// What a run will verify, for the Spec pane.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyPreview {
    pub enabled: bool,
    pub commands: Vec<VerifyCommand>,
    /// "spec", "detected" or "off".
    pub source: String,
    pub detected_from: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn preview(enabled: bool, spec: &crate::spec::ParsedSpec, root: &Path) -> VerifyPreview {
    if !enabled {
        return VerifyPreview {
            source: "off".into(),
            ..Default::default()
        };
    }
    let detected = detect(root);
    let (commands, source) = commands_for(spec, &detected);
    VerifyPreview {
        enabled,
        commands,
        source: source.into(),
        detected_from: detected.sources,
        warnings: if source == "detected" { detected.warnings } else { vec![] },
    }
}

/// The commands a run will use: the spec's when it has a Verification section, else detected.
pub fn commands_for(spec: &crate::spec::ParsedSpec, detected: &Detected) -> (Vec<VerifyCommand>, &'static str) {
    if spec.verification_declared {
        let mut c = spec.verification.clone();
        c.sort_by_key(|c| c.kind);
        (c, "spec")
    } else {
        (detected.commands.clone(), "detected")
    }
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandResult {
    pub kind: Option<VerifyKind>,
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub passed: bool,
    pub duration_ms: u64,
    /// Combined stdout + stderr, ANSI codes removed; the middle is dropped beyond `MAX_KEEP`.
    pub output: String,
    /// `run` with `expect`: the text was not in the output.
    #[serde(default)]
    pub expect_missing: bool,
    /// Could not start the process at all.
    #[serde(default)]
    pub spawn_error: Option<String>,
}

impl CommandResult {
    /// One line for logs and prompts: "build `cargo build` failed (exit code 101) in 4.2s".
    pub fn summary(&self) -> String {
        let what = format!("{} `{}`", self.kind.map(|k| k.word()).unwrap_or("command"), self.command);
        let how = if let Some(e) = &self.spawn_error {
            format!("could not start: {e}")
        } else if self.cancelled {
            "was cancelled".into()
        } else if self.passed && self.timed_out {
            "started and was still running at the timeout (ok)".into()
        } else if self.passed {
            "passed".into()
        } else if self.expect_missing {
            "ran, but its output did not contain the expected text".into()
        } else if self.timed_out {
            "timed out".into()
        } else {
            format!(
                "failed (exit code {})",
                self.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
            )
        };
        format!("{what} {how} in {:.1}s", self.duration_ms as f64 / 1000.0)
    }
}

/// Output kept per command: the first and the last half of this many bytes.
pub const MAX_KEEP: usize = 256 * 1024;

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07").unwrap());

pub fn strip_ansi(s: &str) -> String {
    ANSI.replace_all(s, "").to_string()
}

/// Bounded output buffer: keeps the head and the tail.
#[derive(Default)]
struct OutBuf {
    head: String,
    tail: String,
    dropped: usize,
}

impl OutBuf {
    fn push(&mut self, s: &str) {
        let half = MAX_KEEP / 2;
        if self.head.len() < half {
            let cut = floor_char(s, half - self.head.len());
            self.head.push_str(&s[..cut]);
            self.push_tail(&s[cut..], half);
        } else {
            self.push_tail(s, half);
        }
    }
    fn push_tail(&mut self, s: &str, half: usize) {
        self.tail.push_str(s);
        if self.tail.len() > half * 2 {
            let cut = floor_char(&self.tail, self.tail.len() - half);
            self.dropped += cut;
            self.tail.drain(..cut);
        }
    }
    fn text(self) -> String {
        if self.dropped > 0 {
            format!("{}\n… ({} bytes of output omitted) …\n{}", self.head, self.dropped, self.tail)
        } else {
            self.head + &self.tail
        }
    }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

type Pipe = Box<dyn tokio::io::AsyncRead + Unpin + Send>;

fn read_pipe(pipe: Pipe, tx: tokio::sync::mpsc::UnboundedSender<String>) {
    tokio::spawn(async move {
        let mut pipe = pipe;
        let mut buf = vec![0u8; 8192];
        let mut pending = Vec::new();
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    let valid = match std::str::from_utf8(&pending) {
                        Ok(_) => pending.len(),
                        // An incomplete sequence at the end: wait for the rest.
                        Err(e) if e.error_len().is_none() => e.valid_up_to(),
                        // Invalid bytes (OEM code page output): decode lossily.
                        Err(_) => pending.len(),
                    };
                    let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                    pending.drain(..valid);
                    if tx.send(text).is_err() {
                        break;
                    }
                }
            }
        }
        if !pending.is_empty() {
            let _ = tx.send(String::from_utf8_lossy(&pending).into_owned());
        }
    });
}

/// Run one command with `cmd /C` in `cwd`: hidden console, no stdin, its whole process tree in a
/// job that is terminated on timeout or cancel. `on_output` receives the output as it arrives.
pub async fn run_command(
    cmd: &VerifyCommand,
    cwd: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
    on_output: &mut (dyn FnMut(&str) + Send),
) -> CommandResult {
    let started = Instant::now();
    let mut res = CommandResult {
        kind: Some(cmd.kind),
        command: cmd.command.clone(),
        ..Default::default()
    };
    let mut c = shell_command(&cmd.command);
    c.current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("NO_COLOR", "1")
        .env("CARGO_TERM_COLOR", "never")
        .env("FORCE_COLOR", "0")
        .env("CI", "1")
        .env("PYTHONUNBUFFERED", "1")
        .env("RUST_BACKTRACE", "1")
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .kill_on_drop(true);
    winproc::hide_window(&mut c);
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            res.spawn_error = Some(e.to_string());
            res.duration_ms = started.elapsed().as_millis() as u64;
            return res;
        }
    };
    let job = winproc::TreeJob::new();
    if let (Some(j), Some(pid)) = (&job, child.id()) {
        j.assign_pid(pid);
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(p) = child.stdout.take() {
        read_pipe(Box::new(p), tx.clone());
    }
    if let Some(p) = child.stderr.take() {
        read_pipe(Box::new(p), tx.clone());
    }
    drop(tx);
    let mut out = OutBuf::default();
    let take = |t: String, out: &mut OutBuf, on_output: &mut (dyn FnMut(&str) + Send)| {
        let t = strip_ansi(&t.replace("\r\n", "\n"));
        on_output(&t);
        out.push(&t);
    };
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut status = None;
    let mut pipes_open = true;
    while status.is_none() || pipes_open {
        tokio::select! {
            chunk = rx.recv(), if pipes_open => match chunk {
                Some(t) => take(t, &mut out, on_output),
                None => pipes_open = false,
            },
            s = child.wait(), if status.is_none() => status = Some(s),
            _ = &mut deadline => {
                // A child that exited but left a grandchild holding the pipes is done.
                res.timed_out = status.is_none();
                break;
            }
            _ = cancel.cancelled() => {
                res.cancelled = true;
                break;
            }
        }
    }
    if let Some(j) = &job {
        j.terminate();
    }
    let _ = child.start_kill();
    if status.is_none() {
        status = Some(child.wait().await);
    }
    while let Ok(t) = rx.try_recv() {
        take(t, &mut out, on_output);
    }
    res.duration_ms = started.elapsed().as_millis() as u64;
    res.output = out.text();
    if !res.timed_out && !res.cancelled {
        res.exit_code = status.and_then(|s| s.ok()).and_then(|s| s.code());
    }
    let exit_ok = res.exit_code == Some(0);
    res.passed = !res.cancelled
        && match cmd.kind {
            VerifyKind::Run => {
                let expect_ok = cmd.expect.as_deref().is_none_or(|e| res.output.contains(e));
                res.expect_missing = !expect_ok;
                (exit_ok || res.timed_out) && expect_ok
            }
            _ => exit_ok && !res.timed_out,
        };
    res
}

/// `cmd /D /S /C "<command>"`: with `/S` cmd.exe strips exactly the outer quotes, so quotes
/// inside the command survive (the line is passed raw, not re-quoted by Rust).
#[cfg(windows)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("cmd");
    c.arg("/D").arg("/S").arg("/C").raw_arg(format!("\"{command}\""));
    c
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(command);
    c
}

/// Run the commands in order until one fails (setup → build → test → run). `default_timeout`
/// gives the timeout per kind when a command has none. Emits `cmdStart` / `cmdOutput` /
/// `cmdEnd` run events.
pub async fn run_all(
    cmds: &[VerifyCommand],
    cwd: &Path,
    default_timeout: &(dyn Fn(VerifyKind) -> Duration + Sync),
    cancel: &CancellationToken,
    sink: &Sink,
    run_id: &str,
) -> Vec<CommandResult> {
    let mut results = Vec::new();
    for cmd in cmds {
        if cancel.is_cancelled() {
            break;
        }
        let timeout = cmd
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or_else(|| default_timeout(cmd.kind));
        emit(
            sink,
            "run-event",
            &json!({ "runId": run_id, "kind": "cmdStart", "step": cmd.kind, "command": cmd.command, "timeoutSecs": timeout.as_secs() }),
        );
        let mut buf = String::new();
        let mut last = Instant::now();
        let flush_sink = sink.clone();
        let rid = run_id.to_string();
        let mut on = move |t: &str| {
            buf.push_str(t);
            if !buf.is_empty() && (t.is_empty() || last.elapsed().as_millis() >= 100 || buf.len() > 2000) {
                emit(
                    &flush_sink,
                    "run-event",
                    &json!({ "runId": rid, "kind": "cmdOutput", "text": std::mem::take(&mut buf) }),
                );
                last = Instant::now();
            }
        };
        let r = run_command(cmd, cwd, timeout, cancel, &mut on).await;
        on("");
        emit(
            sink,
            "run-event",
            &json!({ "runId": run_id, "kind": "cmdEnd", "step": cmd.kind, "command": cmd.command, "passed": r.passed,
                     "exitCode": r.exit_code, "timedOut": r.timed_out, "durationMs": r.duration_ms, "summary": r.summary() }),
        );
        let stop = !r.passed;
        results.push(r);
        if stop {
            break;
        }
    }
    results
}

// ---------------------------------------------------------------------------------------------
// Condensing output for the coder
// ---------------------------------------------------------------------------------------------

static NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(Compiling|Downloaded|Downloading|Checking|Fresh|Updating|Locking|Adding|Blocking|Building|Installing|Fetching|Resolving|Packaging)\b|^\s*(added|removed|changed|audited) \d+ packages?|^npm (notice|WARN deprecated)",
    )
    .unwrap()
});
static FIRST_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(error|errors|failed|failure|panicked|exception|traceback|cannot find|not found|undefined|unresolved|mismatched)\b")
        .unwrap()
});

/// Shorten a command's output to about `max_chars` for the coder: progress noise removed, then
/// the region from the first error line on (60%) plus the end of the output (40%).
pub fn digest_output(output: &str, max_chars: usize) -> String {
    let lines: Vec<&str> = output.lines().filter(|l| !NOISE.is_match(l)).collect();
    let text = lines.join("\n");
    if text.len() <= max_chars {
        return text.trim().to_string();
    }
    let first = lines.iter().position(|l| FIRST_ERROR.is_match(l)).unwrap_or(0).saturating_sub(2);
    let head_budget = max_chars * 6 / 10;
    let tail_budget = max_chars - head_budget;
    let mut head = String::new();
    let mut last_head = first;
    for (i, l) in lines.iter().enumerate().skip(first) {
        if head.len() + l.len() + 1 > head_budget {
            break;
        }
        head.push_str(l);
        head.push('\n');
        last_head = i;
    }
    let mut tail: Vec<&str> = Vec::new();
    let mut size = 0;
    for (i, l) in lines.iter().enumerate().rev() {
        if i <= last_head || size + l.len() + 1 > tail_budget {
            break;
        }
        size += l.len() + 1;
        tail.push(l);
    }
    tail.reverse();
    let skipped_mid = lines.len().saturating_sub(last_head + 1 + tail.len());
    let mut out = String::new();
    if first > 0 {
        out.push_str(&format!("… ({first} earlier lines omitted)\n"));
    }
    out.push_str(&head);
    if skipped_mid > 0 && !tail.is_empty() {
        out.push_str(&format!("… ({skipped_mid} lines omitted)\n"));
    }
    out.push_str(&tail.join("\n"));
    out.trim().to_string()
}

static RUST_LOC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-->\s+((?:[A-Za-z]:)?[^\s:]+):\d+:\d+").unwrap());
static PY_LOC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"File "([^"]+)", line \d+"#).unwrap());
static GENERIC_LOC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"((?:[A-Za-z]:)?[A-Za-z0-9_.\\/\-]*[A-Za-z0-9_\-]\.[A-Za-z][A-Za-z0-9]{0,5})(?::|\()\d+").unwrap());

/// Project files named in compiler / test output (`--> src/main.rs:3:5`, `File "app.py", line 2`,
/// `src/a.ts(3,5)`, `main.go:12:3`), relative to `root`, existing, in order of appearance.
pub fn paths_in_output(output: &str, root: &Path) -> Vec<String> {
    let root_s = root.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    let mut found: Vec<(usize, String)> = Vec::new();
    for re in [&*RUST_LOC, &*PY_LOC, &*GENERIC_LOC] {
        for c in re.captures_iter(output) {
            let m = c.get(1).unwrap();
            found.push((m.start(), m.as_str().to_string()));
        }
    }
    found.sort_by_key(|(pos, _)| *pos);
    let mut out: Vec<String> = Vec::new();
    for (_, raw) in found {
        let mut p = raw.replace('\\', "/");
        if p.to_ascii_lowercase().starts_with(&format!("{root_s}/")) {
            p = p[root_s.len() + 1..].to_string();
        }
        let p = p.trim_start_matches("./").to_string();
        let Ok(rel) = crate::pathsafe::normalize_rel(&p) else { continue };
        if !root.join(&rel).is_file() || out.iter().any(|o| o.eq_ignore_ascii_case(&rel)) {
            continue;
        }
        out.push(rel);
        if out.len() >= 8 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_items_with_options() {
        let c = parse_verification_item("- run: `python app.py --demo` (timeout 5s, expect \"Ready\")")
            .unwrap()
            .unwrap();
        assert_eq!(
            (c.kind, c.command.as_str(), c.timeout_secs, c.expect.as_deref()),
            (VerifyKind::Run, "python app.py --demo", Some(5), Some("Ready"))
        );
        assert_eq!(
            parse_verification_item(&c.spec_line()).unwrap().unwrap(),
            c,
            "spec_line round-trips"
        );
        let b = parse_verification_item("* **Build** — `cargo build --release`").unwrap().unwrap();
        assert_eq!((b.kind, b.command.as_str()), (VerifyKind::Build, "cargo build --release"));
        let t = parse_verification_item("1. tests: `npm test` (timeout 120 seconds, expect \"x\")")
            .unwrap()
            .unwrap();
        assert_eq!(
            (t.kind, t.timeout_secs, t.expect),
            (VerifyKind::Test, Some(120), None),
            "expect only applies to run"
        );
        assert_eq!(parse_verification_item("- None."), Ok(None));
        assert!(
            parse_verification_item("- build: cargo build").is_err(),
            "commands must be in backticks"
        );
    }

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let path = d.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, c).unwrap();
        }
        d
    }

    fn cmds(d: &Detected) -> Vec<String> {
        d.commands.iter().map(|c| format!("{}: {}", c.kind.word(), c.command)).collect()
    }

    #[test]
    fn detects_rust_with_and_without_tests() {
        let d = project(&[("Cargo.toml", "[package]\nname='x'\n"), ("src/main.rs", "fn main() {}\n")]);
        assert_eq!(cmds(&detect(d.path())), vec!["build: cargo build"]);
        std::fs::write(d.path().join("src/lib.rs"), "#[cfg(test)]\nmod t {\n#[test]\nfn a() {}\n}\n").unwrap();
        assert_eq!(cmds(&detect(d.path())), vec!["build: cargo build", "test: cargo test"]);
        // Build output is not searched.
        let d = project(&[("Cargo.toml", ""), ("target/debug/build/x/out/gen.rs", "#[test] fn a() {}")]);
        assert_eq!(cmds(&detect(d.path())), vec!["build: cargo build"]);
    }

    #[test]
    fn detects_node_scripts_and_setup() {
        let d = project(&[(
            "package.json",
            r#"{"scripts": {"build": "vite build", "test": "echo \"Error: no test specified\" && exit 1"}, "devDependencies": {"vite": "1"}}"#,
        )]);
        assert_eq!(cmds(&detect(d.path())), vec!["setup: npm install", "build: npm run build"]);
        std::fs::create_dir(d.path().join("node_modules")).unwrap();
        std::fs::write(
            d.path().join("package.json"),
            r#"{"scripts": {"test": "vitest run"}, "devDependencies": {"typescript": "5"}}"#,
        )
        .unwrap();
        std::fs::write(d.path().join("tsconfig.json"), "{}").unwrap();
        assert_eq!(cmds(&detect(d.path())), vec!["build: npx tsc --noEmit", "test: npm test"]);
    }

    #[test]
    fn detects_python_go_and_nothing() {
        let d = project(&[("app.py", "print(1)\n"), ("tests/test_app.py", "def test_a(): pass\n")]);
        assert_eq!(
            cmds(&detect(d.path())),
            vec!["build: python -m compileall -q .", "test: python -m unittest discover -q"]
        );
        std::fs::write(d.path().join("requirements.txt"), "pytest\n").unwrap();
        assert_eq!(cmds(&detect(d.path()))[1], "test: python -m pytest -q");
        let g = project(&[("go.mod", "module x\n"), ("x_test.go", "package x\n")]);
        assert_eq!(cmds(&detect(g.path())), vec!["build: go build ./...", "test: go test ./..."]);
        let web = project(&[("index.html", "<p>hi</p>"), ("game.js", "let a = 1;")]);
        assert!(detect(web.path()).commands.is_empty());
    }

    #[test]
    fn spec_commands_win_over_detected_ones() {
        let detected = Detected {
            commands: vec![VerifyCommand::new(VerifyKind::Build, "cargo build")],
            ..Default::default()
        };
        let spec = crate::spec::parse_spec("## Files\n- `a.rs` (modify)\n## Verification\n- test: `cargo test`\n- build: `cargo check`\n");
        let (c, src) = commands_for(&spec, &detected);
        assert_eq!(
            (c.iter().map(|c| c.command.as_str()).collect::<Vec<_>>(), src),
            (vec!["cargo check", "cargo test"], "spec")
        );
        let none = crate::spec::parse_spec("## Files\n- `a.rs` (modify)\n## Verification\n- None\n");
        assert!(commands_for(&none, &detected).0.is_empty());
        let absent = crate::spec::parse_spec("## Files\n- `a.rs` (modify)\n");
        assert_eq!(commands_for(&absent, &detected), (detected.commands.clone(), "detected"));
    }

    fn run(cmd: VerifyCommand, secs: u64) -> CommandResult {
        let d = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(run_command(
            &cmd,
            d.path(),
            Duration::from_secs(secs),
            &CancellationToken::new(),
            &mut |_| {},
        ))
    }

    #[cfg(windows)]
    #[test]
    fn runs_commands_and_reports_exit_codes() {
        let ok = run(
            VerifyCommand::new(VerifyKind::Build, "echo built \"with quotes\" && echo warn 1>&2"),
            20,
        );
        assert!(ok.passed && ok.exit_code == Some(0), "{ok:?}");
        assert!(
            ok.output.contains("built \"with quotes\"") && ok.output.contains("warn"),
            "{:?}",
            ok.output
        );
        let bad = run(VerifyCommand::new(VerifyKind::Test, "echo boom && exit /b 3"), 20);
        assert!(!bad.passed && bad.exit_code == Some(3), "{bad:?}");
        assert!(bad.summary().contains("failed (exit code 3)"), "{}", bad.summary());
        let missing = run(VerifyCommand::new(VerifyKind::Build, "relay-no-such-tool-xyz --version"), 20);
        assert!(!missing.passed, "an unknown program fails: {missing:?}");
    }

    #[cfg(windows)]
    #[test]
    fn timeouts_fail_builds_but_pass_runs_that_keep_running() {
        let slow = "ping -n 30 127.0.0.1 >nul";
        let b = run(VerifyCommand::new(VerifyKind::Build, slow), 1);
        assert!(b.timed_out && !b.passed && b.duration_ms < 10_000, "{b:?}");
        let r = run(VerifyCommand::new(VerifyKind::Run, slow), 1);
        assert!(
            r.timed_out && r.passed,
            "a program still running at the timeout started fine: {r:?}"
        );
        let mut expect = VerifyCommand::new(VerifyKind::Run, "echo Usage: demo");
        expect.expect = Some("Usage".into());
        assert!(run(expect.clone(), 20).passed);
        expect.expect = Some("Missing".into());
        let m = run(expect, 20);
        assert!(!m.passed && m.expect_missing, "{m:?}");
        let crash = run(VerifyCommand::new(VerifyKind::Run, "exit /b 1"), 20);
        assert!(!crash.passed);
    }

    #[cfg(windows)]
    #[test]
    fn cancel_kills_the_process_tree() {
        let d = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let r = rt.block_on(async move {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                c2.cancel();
            });
            run_command(
                &VerifyCommand::new(VerifyKind::Build, "ping -n 30 127.0.0.1"),
                d.path(),
                Duration::from_secs(60),
                &cancel,
                &mut |_| {},
            )
            .await
        });
        assert!(r.cancelled && !r.passed && r.duration_ms < 10_000, "{r:?}");
    }

    #[test]
    fn digest_keeps_the_first_error_and_the_end() {
        let mut out = String::new();
        for i in 0..200 {
            out.push_str(&format!("   Compiling dep{i} v1.0.0\n"));
        }
        out.push_str("warning: unused variable\n");
        out.push_str("error[E0425]: cannot find value `x` in this scope\n --> src/main.rs:3:5\n");
        for i in 0..400 {
            out.push_str(&format!("note: filler line {i}\n"));
        }
        out.push_str("error: could not compile `demo` due to 1 previous error\n");
        let d = digest_output(&out, 2000);
        assert!(d.len() <= 2200, "{}", d.len());
        assert!(!d.contains("Compiling dep"), "progress noise is removed");
        assert!(d.contains("cannot find value `x`") && d.contains("could not compile `demo`"), "{d}");
        assert!(d.contains("lines omitted"));
        assert_eq!(digest_output("short\n", 100), "short");
    }

    #[test]
    fn finds_project_paths_in_compiler_output() {
        let d = project(&[("src/main.rs", ""), ("app.py", ""), ("web/a.ts", ""), ("main.go", "")]);
        let root = d.path();
        let abs = format!("{}\\app.py", root.display());
        let out = format!(
            "error[E0308]: mismatched types\n --> src/main.rs:3:5\nTraceback:\n  File \"{abs}\", line 2, in <module>\n  File \"C:\\Python310\\lib\\x.py\", line 9\nweb/a.ts(3,5): error TS2304\n./main.go:12:3: undefined: y\nsrc/main.rs:9:1 again\nsrc/missing.rs:1:1\n"
        );
        assert_eq!(paths_in_output(&out, root), vec!["src/main.rs", "app.py", "web/a.ts", "main.go"]);
    }

    #[test]
    fn ansi_codes_are_removed() {
        assert_eq!(strip_ansi("\x1b[31merror\x1b[0m: x"), "error: x");
    }
}
