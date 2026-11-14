//! Headless end-to-end checks for Relay's backend (the same `AppCore` the UI uses).
//!
//!   cargo run --example e2e -- fake     # managed fake llama-server, no GPU
//!   cargo run --example e2e -- claude   # + real Claude CLI expansion/review (no GPU)
//!   cargo run --example e2e -- real     # real llama-server + the three real models (GPU!)
//!
//! Env: RELAY_FAKE_SERVER (path to fake-llama-server.exe), RELAY_LLAMA_SERVER, RELAY_MODELS_DIR.

use relay_lib::app::{AppCore, ExpandRequest, ImplementCommand};
use relay_lib::budget::SplitMode;
use relay_lib::config::{Profile, ProfileKind};
use relay_lib::events::{PrintSink, Sink};
use relay_lib::server::ServerState;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Checks {
    passed: Vec<String>,
    failed: Vec<String>,
}

impl Checks {
    fn check(&mut self, ok: bool, what: impl Into<String>) {
        let w = what.into();
        if ok {
            println!("  PASS  {w}");
            self.passed.push(w);
        } else {
            println!("  FAIL  {w}");
            self.failed.push(w);
        }
    }
}

const CLIENT_PY: &str = r#"import json
import urllib.parse
import urllib.request


class HttpClient:
    """Tiny JSON-over-HTTP client."""

    def __init__(self, base_url, timeout=10):
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout

    def get(self, path, params=None):
        url = self.base_url + path
        if params:
            url += "?" + urllib.parse.urlencode(params)
        return self._request("GET", url)

    def post(self, path, payload):
        return self._request("POST", self.base_url + path, json.dumps(payload).encode())

    def _request(self, method, url, data=None):
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(req, timeout=self.timeout) as resp:
            return json.loads(resp.read().decode())
"#;

pub const RETRY_SPEC: &str = r#"## Summary
Add automatic retries with exponential backoff to `HttpClient._request` for transient failures (connection errors and HTTP 5xx). The backoff logic lives in a new helper module.

## Files
- `src/http_client.py` (modify) — call the retry helper from `_request`
- `src/retry.py` (create) — `retry_with_backoff` helper

## Changes

### `src/http_client.py`
1. Add `import urllib.error` after `import urllib.parse`.
2. Add `from .retry import retry_with_backoff` after the existing imports (blank line before the class stays).
3. Change `__init__(self, base_url, timeout=10)` to `__init__(self, base_url, timeout=10, max_retries=3, backoff_base=0.5)` and store both new arguments as `self.max_retries` and `self.backoff_base`.
4. Change `_request` so that it still builds the `urllib.request.Request` and adds the `Content-Type` header exactly as now, but moves the existing `with urllib.request.urlopen(...)` block (open, read, decode, `json.loads`) into an inner function `send()` that takes no arguments and returns the parsed JSON. `_request` then returns `retry_with_backoff(send, retries=self.max_retries, base_delay=self.backoff_base, should_retry=_is_transient)`.
5. Add a module-level function `_is_transient(exc) -> bool` after the class. It returns True for an `urllib.error.HTTPError` whose `code` is between 500 and 599 inclusive, False for any other `HTTPError`, True for `urllib.error.URLError`, `TimeoutError` and `ConnectionError`, and False for everything else. Check `HTTPError` first, because it is a subclass of `URLError`.

### `src/retry.py`
Create the file. It imports `time` and defines one function:
`retry_with_backoff(fn, retries=3, base_delay=0.5, should_retry=lambda exc: True, sleep=time.sleep)`.
1. Give it a docstring saying it calls `fn()` and retries retryable exceptions with exponential backoff, making at most `retries + 1` calls.
2. Keep an attempt counter starting at 0. In a loop, call `fn()` and return its result if it succeeds.
3. If `fn()` raises an `Exception`: re-raise that same exception unchanged when the counter is already equal to or greater than `retries`, or when `should_retry(exc)` is False. Otherwise call `sleep(base_delay * 2 ** counter)`, increase the counter by 1, and loop again.
4. Exceptions that are not subclasses of `Exception` (e.g. `KeyboardInterrupt`) are not caught.

## Edge cases
- 4xx responses (`HTTPError` with code < 500) are not retried.
- `max_retries=0` means exactly one attempt.

## Acceptance criteria
- `HttpClient("http://x").max_retries == 3` and `.backoff_base == 0.5`.
- A request that fails twice with HTTP 503 and then succeeds returns the parsed JSON after two sleeps (0.5s, 1.0s).
- A 404 raises immediately without sleeping.
"#;

fn make_project(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/http_client.py"), CLIENT_PY).unwrap();
    std::fs::write(dir.join("src/__init__.py"), "").unwrap();
    // A read-only context file large enough to be sent as an outline.
    let mut models = String::from("\"\"\"Response models.\"\"\"\nimport dataclasses\n\n");
    for i in 0..30 {
        models.push_str(&format!(
            "\n@dataclasses.dataclass\nclass Model{i}:\n    \"\"\"Model {i}.\"\"\"\n\n    id: int\n    name: str\n\n    def describe(self, verbose=False):\n        text = f\"{{self.id}}: {{self.name}}\"\n        if verbose:\n            text += \" (model {i})\"\n        return text\n\n"
        ));
    }
    std::fs::write(dir.join("src/models.py"), models).unwrap();
    std::fs::write(dir.join("README.md"), "# demo\n\nA tiny HTTP client used to test Relay.\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "__pycache__/\n").unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    };
    git(&["init", "-q"]);
    git(&["-c", "user.email=relay@example.invalid", "-c", "user.name=Relay Test", "add", "-A"]);
    git(&[
        "-c",
        "user.email=relay@example.invalid",
        "-c",
        "user.name=Relay Test",
        "commit",
        "-q",
        "-m",
        "init",
    ]);
}

fn scratch() -> PathBuf {
    std::env::var("RELAY_E2E_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("relay-e2e"))
}

fn pid_alive(pid: u32) -> bool {
    relay_lib::winproc::image_path(pid).is_some()
}

/// A helper process started by the test itself (external server, fake orphans).
///
/// It must never outlive the test: a survivor keeps the test's inherited stdout pipe open, so
/// `cargo run --example e2e | grep …` would never see EOF and appear to hang. So: no stdio, a
/// kill-on-close job (dies with this process even if the test is interrupted), and kill on drop.
struct Helper {
    child: std::process::Child,
}

fn helper_job() -> Option<&'static relay_lib::winproc::TreeJob> {
    static JOB: std::sync::OnceLock<Option<relay_lib::winproc::TreeJob>> = std::sync::OnceLock::new();
    JOB.get_or_init(relay_lib::winproc::TreeJob::new).as_ref()
}

impl Helper {
    fn spawn(exe: &str, args: &[&str]) -> Helper {
        let child = std::process::Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn helper");
        if let Some(job) = helper_job() {
            job.assign_pid(child.id());
        }
        Helper { child }
    }
    fn id(&self) -> u32 {
        self.child.id()
    }
    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "fake".into());
    let base = scratch();
    let app_dir = base.join("appdata");
    let _ = std::fs::remove_dir_all(&app_dir);
    std::fs::create_dir_all(&app_dir).unwrap();
    let script = base.join("fake-script.json");
    std::fs::write(&script, "[]").unwrap();
    // SAFETY: set before the runtime starts any threads.
    unsafe {
        std::env::set_var("RELAY_APP_DIR", &app_dir);
        std::env::set_var("FAKE_LLM_SCRIPT", &script);
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let code = rt.block_on(run(&mode, &base, &script));
    std::process::exit(code);
}

fn set_script(path: &Path, rules: serde_json::Value) {
    std::fs::write(path, rules.to_string()).unwrap();
}

async fn run(mode: &str, base: &Path, script: &Path) -> i32 {
    let sink: Sink = Arc::new(PrintSink { verbose_deltas: false });
    let core = AppCore::new(sink).expect("core");
    let mut c = Checks {
        passed: vec![],
        failed: vec![],
    };
    let project = base.join("project");
    make_project(&project);
    let root = project.to_string_lossy().into_owned();

    // ---- configure ----------------------------------------------------------------------------
    let mut settings = core.settings();
    let mut profiles = core.profiles();
    if mode == "real" {
        settings.llama_server_path =
            std::env::var("RELAY_LLAMA_SERVER").unwrap_or_else(|_| r"D:\LocalAI-models\llama.cpp-b12360\bin\llama-server.exe".into());
        settings.models_dir = std::env::var("RELAY_MODELS_DIR").unwrap_or_else(|_| r"D:\LocalAI-models".into());
    } else {
        let fake = std::env::var("RELAY_FAKE_SERVER").unwrap_or_else(|_| {
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                r"\..\tools\fake-llama-server\target\release\fake-llama-server.exe"
            )
            .into()
        });
        settings.llama_server_path = fake;
        settings.models_dir = base.join("models").to_string_lossy().into_owned();
        std::fs::create_dir_all(&settings.models_dir).unwrap();
        for p in profiles.iter_mut() {
            std::fs::write(Path::new(&settings.models_dir).join(&p.hf_file), b"GGUF fake").unwrap();
            p.launch_args = format!("{} --fake-load-ms 700", p.launch_args);
            if p.id == "gpt-oss-20b" {
                p.launch_args.push_str(" --fake-reasoning");
            }
        }
    }
    settings.auto_fallback = true;
    core.set_settings(settings.clone()).unwrap();
    core.set_profiles(profiles.clone()).unwrap();

    // ---- slice a: managed server + hard-coded spec + apply --------------------------------------
    println!("\n== Slice a: managed llama-server for Qwen3.8-27B, hard-coded spec, apply");
    let t = Instant::now();
    let st = core.switch_profile("qwen3.8-27b").await.unwrap();
    c.check(
        st.state == ServerState::Ready,
        format!("Qwen3.8-27B ready ({:?}, {})", st.state, st.message),
    );
    let first_pid = st.pid;
    println!("     load took {:.1}s", t.elapsed().as_secs_f64());
    c.check(
        core.server.log_lines().iter().any(|l| l.contains("listening on")),
        "server log captured",
    );

    let prep = core.prepare(&root, RETRY_SPEC, &[], &[], "qwen3.8-27b").await.unwrap();
    c.check(prep.files.iter().filter(|f| f.target).count() == 2, "spec parsed: 2 target files");
    // The fake server's /tokenize counts bytes / 4 (the estimate uses bytes / 3.3).
    let client = prep.files.iter().find(|f| f.path == "src/http_client.py");
    let client_bytes = std::fs::read_to_string(project.join("src/http_client.py")).unwrap().len();
    c.check(
        prep.budget.exact && client.is_some_and(|f| f.tokens as usize == client_bytes.div_ceil(4)),
        format!(
            "exact token counts from the server's /tokenize ({:?} tokens for {client_bytes} bytes, exact: {})",
            client.map(|f| f.tokens),
            prep.budget.exact
        ),
    );
    c.check(
        prep.spec.warnings.is_empty(),
        format!("hand-written spec has no warnings (no code): {:?}", prep.spec.warnings),
    );
    let ctx = ["src/models.py".to_string()];
    let outlined = core.prepare(&root, RETRY_SPEC, &ctx, &[], "qwen3.8-27b").await.unwrap();
    let m = outlined.files.iter().find(|f| f.path == "src/models.py");
    c.check(
        m.is_some_and(|f| f.outline && !f.target && f.tokens * 4 < f.full_tokens * 3),
        format!(
            "large context file sent as an outline (≈{:?} of ≈{:?} tokens)",
            m.map(|f| f.tokens),
            m.map(|f| f.full_tokens)
        ),
    );
    let full = core.prepare(&root, RETRY_SPEC, &ctx, &ctx, "qwen3.8-27b").await.unwrap();
    let m = full.files.iter().find(|f| f.path == "src/models.py");
    c.check(
        m.is_some_and(|f| !f.outline && f.tokens == f.full_tokens),
        "the same file in full when asked",
    );
    c.check(
        prep.budget.recommended == SplitMode::Single,
        format!(
            "budget: single call fits ({} of {} tokens)",
            prep.budget.single.total_tokens, prep.budget.usable
        ),
    );

    let resp = core
        .implement(ImplementCommand {
            job_id: "job-a".into(),
            run_id: Some("run-a".into()),
            project_root: root.clone(),
            instruction: "add retry with backoff to the HTTP client".into(),
            spec: RETRY_SPEC.into(),
            context_files: vec!["src/models.py".into()],
            full_context: vec![],
            mode: None,
            profile_id: "qwen3.8-27b".into(),
        })
        .await
        .unwrap();
    let r = &resp.result;
    c.check(r.validation_ok, format!("validation ok (error: {:?})", r.error));
    c.check(
        r.files.len() == 2,
        format!("2 proposed files: {:?}", r.files.iter().map(|f| &f.path).collect::<Vec<_>>()),
    );
    c.check(
        r.avg_tok_per_sec.is_some(),
        format!("tok/s reported: {:?}", r.avg_tok_per_sec.map(|t| (t * 10.0).round() / 10.0)),
    );
    // The same job again on the same model: the server should take the whole prompt from its cache.
    let again = core
        .implement(ImplementCommand {
            job_id: "job-a-cache".into(),
            run_id: Some("run-a-cache".into()),
            project_root: root.clone(),
            instruction: "same job again (prompt cache)".into(),
            spec: RETRY_SPEC.into(),
            context_files: vec!["src/models.py".into()],
            full_context: vec![],
            mode: None,
            profile_id: "qwen3.8-27b".into(),
        })
        .await
        .unwrap();
    let a2 = again.result.calls.first().and_then(|c| c.attempts.first());
    let (p2, c2) = (a2.and_then(|a| a.prompt_tokens), a2.and_then(|a| a.cached_tokens));
    c.check(
        matches!((p2, c2), (Some(p), Some(cached)) if f64::from(cached) >= 0.9 * f64::from(p)),
        format!("same job again: prompt served from the cache ({c2:?} of {p2:?} tokens)"),
    );
    if mode == "real" {
        for call in &r.calls {
            for a in &call.attempts {
                println!("---- attempt {} raw output ----\n{}\n----", a.attempt, a.raw_output);
            }
        }
    }
    let git = relay_lib::project::git_status(&project);
    c.check(git.is_repo && !git.dirty, "git: clean repo before apply");
    for f in &r.files {
        let out = core.accept_file("run-a", &f.path, None, false).unwrap();
        c.check(out.ok, format!("accepted {} ({})", f.path, out.message));
    }
    let disk = std::fs::read_to_string(project.join("src/http_client.py")).unwrap();
    c.check(disk != CLIENT_PY, "src/http_client.py changed on disk");
    c.check(project.join("src/retry.py").exists(), "src/retry.py created");
    let git = relay_lib::project::git_status(&project);
    c.check(git.dirty, format!("git now dirty ({} changes)", git.changes.len()));
    if mode == "real" {
        let py = std::process::Command::new("python")
            .arg("-c")
            .arg("import sys; sys.path.insert(0, '.'); from src.http_client import HttpClient; c = HttpClient('http://x'); assert c.max_retries == 3 and c.backoff_base == 0.5; from src.retry import retry_with_backoff; n=[0]\ndef f():\n    n[0]+=1\n    if n[0]<3: raise ConnectionError()\n    return 'ok'\nsl=[]\nassert retry_with_backoff(f, sleep=sl.append)=='ok' and sl==[0.5,1.0]; print('PY OK')")
            .current_dir(&project)
            .output();
        let ok = py
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("PY OK"))
            .unwrap_or(false);
        c.check(
            ok,
            format!(
                "generated code passes acceptance checks in Python {}",
                py.map(|o| String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("").to_string())
                    .unwrap_or_default()
            ),
        );
    }
    // Conflict detection: the file changed on disk after the run read it.
    make_project(&project);
    std::fs::write(project.join("src/http_client.py"), "changed by user\n").unwrap();
    let conflict = core.accept_file("run-a", "src/http_client.py", None, false).unwrap();
    c.check(
        !conflict.ok && conflict.conflict.is_some(),
        "accept refuses to overwrite a file changed on disk",
    );
    make_project(&project);

    if mode != "real" {
        // ---- retry path ---------------------------------------------------------------------
        println!("\n== Edit-format validation: one automatic retry");
        let bad = "src/http_client.py\n<<<<<<< SEARCH\n  def get(self, path):\n=======\n    def get(self, path, params=None, headers=None):\n>>>>>>> REPLACE\n";
        let good = "src/http_client.py\n<<<<<<< SEARCH\n    def get(self, path, params=None):\n=======\n    def get(self, path, params=None, headers=None):\n>>>>>>> REPLACE\n";
        set_script(
            script,
            serde_json::json!([
                {"contains": "could not be applied", "response": good},
                {"contains": "RETRY_TEST", "response": bad}
            ]),
        );
        let spec = "## Summary\nRETRY_TEST add headers param\n## Files\n- `src/http_client.py` (modify)\n## Changes\n### `src/http_client.py`\nAdd a `headers=None` parameter to `get`.\n";
        let resp = core
            .implement(ImplementCommand {
                job_id: "job-retry".into(),
                run_id: Some("run-retry".into()),
                project_root: root.clone(),
                instruction: "retry test".into(),
                spec: spec.into(),
                context_files: vec![],
                full_context: vec![],
                mode: None,
                profile_id: "qwen3.8-27b".into(),
            })
            .await
            .unwrap();
        let attempts = resp.result.calls.first().map(|c| c.attempts.len()).unwrap_or(0);
        c.check(
            resp.result.validation_ok && attempts == 2,
            format!("bad SEARCH → retry → valid (attempts={attempts})"),
        );
        let first = &resp.result.calls[0].attempts[0];
        c.check(
            first.outcomes.iter().any(|o| o.closest.is_some()),
            "first attempt reported the closest matching lines",
        );
        if let Some(second) = resp.result.calls[0].attempts.get(1) {
            c.check(
                second.cached_tokens.unwrap_or(0) >= first.prompt_tokens.unwrap_or(u32::MAX),
                format!(
                    "retry reuses the first attempt's prompt from the cache ({:?} cached ≥ {:?})",
                    second.cached_tokens, first.prompt_tokens
                ),
            );
        }

        println!("\n== Validation fails twice → fallback offered");
        set_script(script, serde_json::json!([{"contains": "", "response": bad}]));
        let resp = core
            .implement(ImplementCommand {
                job_id: "job-fail".into(),
                run_id: Some("run-fail".into()),
                project_root: root.clone(),
                instruction: "fail test".into(),
                spec: spec.into(),
                context_files: vec![],
                full_context: vec![],
                mode: None,
                profile_id: "qwen3.8-27b".into(),
            })
            .await
            .unwrap();
        c.check(!resp.result.validation_ok, "validation failed after 2 attempts");
        c.check(
            resp.fallback_suggested && resp.fallback_profile.as_deref() == Some("gpt-oss-20b"),
            "auto-fallback suggests gpt-oss-20b",
        );

        println!("\n== Path safety: model tries to write outside the project");
        set_script(
            script,
            serde_json::json!([{"contains": "", "response": "../evil.txt\n<<<<<<< SEARCH\n=======\npwned\n>>>>>>> REPLACE\n"}]),
        );
        let resp = core
            .implement(ImplementCommand {
                job_id: "job-evil".into(),
                run_id: Some("run-evil".into()),
                project_root: root.clone(),
                instruction: "evil".into(),
                spec: "## Summary\nx\n".into(),
                context_files: vec![],
                full_context: vec![],
                mode: None,
                profile_id: "qwen3.8-27b".into(),
            })
            .await
            .unwrap();
        c.check(
            resp.result.files.is_empty() && !base.join("evil.txt").exists(),
            "../evil.txt rejected, nothing written",
        );
        set_script(script, serde_json::json!([]));
    }

    // ---- slice b: switching all profiles --------------------------------------------------------
    println!("\n== Slice b: switch between all three models");
    if mode != "real" {
        // The fake can't really rename things; script the answer to the profile test task.
        set_script(
            script,
            serde_json::json!([{"contains": "Change the greeting", "response": "hello.py\n<<<<<<< SEARCH\n    print('hi')\n=======\n    print('hello')\n>>>>>>> REPLACE\n"}]),
        );
    }
    for id in ["qwen3.6-35b-a3b", "gpt-oss-20b", "qwen3.8-27b"] {
        let prev = core.server.status().pid;
        let t = Instant::now();
        let test = core.test_profile(id).await.unwrap();
        c.check(
            test.ok && core.server.status().state == ServerState::Ready,
            format!("{id}: {}", test.message),
        );
        c.check(test.format_ok, format!("{id}: edit-format test"));
        if let Some(p) = prev {
            c.check(
                !pid_alive(p) || Some(p) == core.server.status().pid,
                format!("{id}: previous server (pid {p}) stopped before load"),
            );
        }
        if id == "gpt-oss-20b" {
            c.check(
                test.reasoning_chars > 0 && !test.output.contains("<|channel|>"),
                "gpt-oss reasoning was returned separately and stripped",
            );
        }
        println!("     switch+test took {:.1}s", t.elapsed().as_secs_f64());
    }
    let _ = first_pid;

    if mode != "real" {
        println!("\n== Load failure is diagnosed");
        let mut ps = core.profiles();
        let mut broken = ps[0].clone();
        broken.id = "broken".into();
        broken.builtin = false;
        broken.name = "Broken".into();
        broken.launch_args.push_str(" --fake-fail");
        ps.push(broken);
        core.set_profiles(ps).unwrap();
        let st = core.switch_profile("broken").await.unwrap();
        c.check(
            st.state == ServerState::Error && st.hint.as_deref().unwrap_or("").contains("VRAM"),
            format!("error state with VRAM hint: {} / {:?}", st.message, st.hint),
        );

        println!("\n== External mode (endpoint not managed by Relay)");
        let fake = core.settings().llama_server_path;
        let mut ext_proc = Helper::spawn(&fake, &["--port", "39391", "-m", "external-model", "--fake-load-ms", "100"]);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let mut ps = core.profiles();
        ps.push(Profile {
            id: "lmstudio".into(),
            name: "LM Studio (external)".into(),
            kind: ProfileKind::External,
            context_limit: 32768,
            external: relay_lib::config::ExternalEndpoint {
                base_url: "http://127.0.0.1:39391/v1".into(),
                model: "external-model".into(),
                use_api_key: false,
            },
            ..Default::default()
        });
        core.set_profiles(ps).unwrap();
        let st = core.switch_profile("lmstudio").await.unwrap();
        c.check(
            st.state == ServerState::Ready && st.external,
            format!("external endpoint connected: {}", st.message),
        );
        c.check(core.server.status().pid.is_none(), "no managed process while in external mode");
        let resp = core
            .implement(ImplementCommand {
                job_id: "job-ext".into(),
                run_id: Some("run-ext".into()),
                project_root: root.clone(),
                instruction: "ext".into(),
                spec: RETRY_SPEC.into(),
                context_files: vec![],
                full_context: vec![],
                mode: None,
                profile_id: "lmstudio".into(),
            })
            .await
            .unwrap();
        c.check(resp.result.validation_ok, "implement through the external endpoint");
        core.switch_profile("gpt-oss-20b").await.unwrap();
        c.check(ext_proc.running(), "switching away did not kill the external server");
        drop(ext_proc);

        println!("\n== Per-file split when the job doesn't fit");
        let mut ps = core.profiles();
        let mut small = ps.iter().find(|p| p.id == "qwen3.8-27b").unwrap().clone();
        small.id = "tiny-ctx".into();
        small.builtin = false;
        small.name = "Tiny context".into();
        small.context_limit = 4096;
        small.max_output_tokens = 1024;
        ps.push(small);
        core.set_profiles(ps).unwrap();
        let big: String = format!(
            "{CLIENT_PY}\n{}",
            (0..110)
                .map(|i| format!("# padding line {i} to make the file large enough\n"))
                .collect::<String>()
        );
        std::fs::write(project.join("src/big_a.py"), &big).unwrap();
        std::fs::write(project.join("src/big_b.py"), &big).unwrap();
        let spec = "## Summary\nTouch both.\n## Files\n- `src/big_a.py` (modify)\n- `src/big_b.py` (modify)\n## Changes\n### `src/big_a.py`\nAppend a comment.\n### `src/big_b.py`\nAppend a comment.\n";
        let prep = core.prepare(&root, spec, &[], &[], "tiny-ctx").await.unwrap();
        c.check(
            prep.budget.recommended == SplitMode::PerFile,
            format!(
                "budget recommends per-file ({} > {})",
                prep.budget.single.total_tokens, prep.budget.usable
            ),
        );
        let resp = core
            .implement(ImplementCommand {
                job_id: "job-split".into(),
                run_id: Some("run-split".into()),
                project_root: root.clone(),
                instruction: "split".into(),
                spec: spec.into(),
                context_files: vec![],
                full_context: vec![],
                mode: None,
                profile_id: "tiny-ctx".into(),
            })
            .await
            .unwrap();
        c.check(
            resp.result.mode == SplitMode::PerFile && resp.result.calls.len() == 2 && resp.result.validation_ok,
            format!("2 per-file calls, valid: {}", resp.result.validation_ok),
        );
        let cached = |i: usize| {
            resp.result
                .calls
                .get(i)
                .and_then(|c| c.attempts.first())
                .and_then(|a| a.cached_tokens)
        };
        let system = relay_lib::templates::load(relay_lib::templates::Template::ImplementSystem).len() as u32 / 4;
        c.check(
            cached(1).is_some_and(|t| t >= system),
            format!(
                "second per-file call reuses the shared prefix ({:?} cached, system prompt ≈{system})",
                cached(1)
            ),
        );

        println!("\n== Cancel a running generation");
        let mut ps = core.profiles();
        if let Some(p) = ps.iter_mut().find(|p| p.id == "gpt-oss-20b") {
            p.launch_args.push_str(" --fake-tps 15");
        }
        core.set_profiles(ps).unwrap();
        core.switch_profile("gpt-oss-20b").await.unwrap();
        let core = Arc::new(core);
        let c2 = core.clone();
        let root2 = root.clone();
        let h = tokio::spawn(async move {
            c2.implement(ImplementCommand {
                job_id: "job-cancel".into(),
                run_id: Some("run-cancel".into()),
                project_root: root2,
                instruction: "cancel".into(),
                spec: RETRY_SPEC.into(),
                context_files: vec![],
                full_context: vec![],
                mode: None,
                profile_id: "gpt-oss-20b".into(),
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let cancelled = core.cancel("run-cancel");
        let t = Instant::now();
        let resp = h.await.unwrap().unwrap();
        c.check(
            cancelled && resp.result.cancelled && t.elapsed() < Duration::from_secs(3),
            format!("cancelled within {:.1}s", t.elapsed().as_secs_f64()),
        );

        println!("\n== Orphan cleanup");
        // `fake` deliberately contains `src-tauri\..\tools`: the PID file path and the process's
        // reported image path differ textually but name the same file.
        let mut orphan = Helper::spawn(&fake, &["--port", "39392", "--fake-load-ms", "100"]);
        let pidfile = relay_lib::config::app_dir().join("run").join("llama-server.pid");
        std::fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
        std::fs::write(&pidfile, serde_json::json!({"pid": orphan.id(), "exe": fake}).to_string()).unwrap();
        let note = relay_lib::server::cleanup_orphans();
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.check(note.is_some() && !orphan.running(), format!("orphan killed: {note:?}"));
        drop(orphan);
        let mut innocent = Helper::spawn(&fake, &["--port", "39393", "--fake-load-ms", "100"]);
        std::fs::write(
            &pidfile,
            serde_json::json!({"pid": innocent.id(), "exe": "C:\\other\\llama-server.exe"}).to_string(),
        )
        .unwrap();
        relay_lib::server::cleanup_orphans();
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.check(innocent.running(), "a process whose image doesn't match the PID file is left alone");
        drop(innocent);

        println!("\n== History");
        let jobs = core.history.list_jobs(50, "").unwrap();
        c.check(
            jobs.iter().any(|j| j.id == "job-a" && !j.runs.is_empty()),
            format!("{} jobs recorded", jobs.len()),
        );
        let (_, runs) = core.history.get_job("job-a").unwrap().unwrap();
        c.check(runs[0].files.iter().all(|f| f.decision == "accepted"), "accept decisions recorded");

        if mode == "claude" {
            println!("\n== Slice c: Claude CLI expansion (+ review)");
            make_project(&project);
            let t = Instant::now();
            let ex = core
                .expand(ExpandRequest {
                    job_id: "job-claude".into(),
                    instruction: "add a retry with backoff to the HTTP client".into(),
                    project_root: root.clone(),
                    context_files: vec!["src/http_client.py".into()],
                    profile_id: "qwen3.8-27b".into(),
                })
                .await;
            match ex {
                Ok(ex) => {
                    println!("{}", ex.spec);
                    c.check(
                        !ex.parsed.files.is_empty(),
                        format!(
                            "Claude spec lists files: {:?} ({:.0}s, ${:?})",
                            ex.parsed.files.iter().map(|f| &f.path).collect::<Vec<_>>(),
                            t.elapsed().as_secs_f64(),
                            ex.claude.cost_usd
                        ),
                    );
                    c.check(
                        ex.parsed.warnings.is_empty(),
                        format!("spec structure ok: {:?}", ex.parsed.warnings),
                    );
                    let resp = core
                        .implement(ImplementCommand {
                            job_id: "job-claude".into(),
                            run_id: Some("run-claude".into()),
                            project_root: root.clone(),
                            instruction: String::new(),
                            spec: ex.spec.clone(),
                            context_files: vec![],
                            full_context: vec![],
                            mode: None,
                            profile_id: "qwen3.8-27b".into(),
                        })
                        .await
                        .unwrap();
                    c.check(resp.result.validation_ok, "fake model implemented the Claude spec");
                    for f in &resp.result.files {
                        core.accept_file("run-claude", &f.path, None, false).unwrap();
                    }
                    match core.review("run-claude", "review-1").await {
                        Ok(rv) => c.check(
                            !rv.verdict.is_empty(),
                            format!("review verdict: {} (follow-up spec: {})", rv.verdict, rv.follow_up_spec.is_some()),
                        ),
                        Err(e) => c.check(false, format!("review failed: {e}")),
                    }
                }
                Err(e) => c.check(false, format!("expansion failed: {e}")),
            }
        }
        core.shutdown().await;
    } else {
        core.shutdown().await;
    }

    println!("\n{} passed, {} failed", c.passed.len(), c.failed.len());
    for f in &c.failed {
        println!("  FAILED: {f}");
    }
    if c.failed.is_empty() { 0 } else { 1 }
}
