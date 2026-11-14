//! GPU benchmark of profile variants for the reference card (uses the real llama-server and the
//! three real models — GPU!). For each variant it loads the model, measures VRAM, load time,
//! prompt-processing speed on a long prompt, generation speed and validity on the e2e retry spec
//! (twice), and prompt-cache reuse in a per-file run. The `default` variants (shipped profiles) also run the cache
//! control (per-file calls without priming), a spec-review prewarm, and a two-slot parallel test.
//!
//!   cargo run --release --example bench                # all variants
//!   cargo run --release --example bench -- qwen3.8     # variants whose name contains a filter
//!   cargo run --release --example bench -- --list
//!
//! Env: RELAY_LLAMA_SERVER, RELAY_MODELS_DIR (defaults: D:\LocalAI-models), RELAY_BENCH_DIR.
//! Refuses to start when more than 2.5 GB of VRAM is already in use (a game?) unless --force.

#[allow(dead_code, unused_imports)]
#[path = "e2e.rs"]
mod e2e;

use relay_lib::app::{AppCore, ImplementCommand, PrewarmCommand};
use relay_lib::budget::SplitMode;
use relay_lib::config::Profile;
use relay_lib::events::{NullSink, Sink};
use relay_lib::llm::{ChatMessage, Endpoint, build_body, stream_chat};
use relay_lib::pipeline::ImplementResult;
use relay_lib::server::{ServerState, gpu_memory};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

struct Variant {
    name: &'static str,
    base: &'static str,
    /// Replaces the built-in launch arguments.
    args: Option<&'static str>,
    /// Appended to the launch arguments.
    extra: &'static str,
    ctx: Option<u32>,
    temperature: Option<f32>,
    baseline: bool,
}


// Launch arguments the variants build on (the profile defaults before the 2026-11-14 tuning), so
// the comparisons do not shift when the built-in defaults change.
const Q38: &str = "-ngl all -fa on -ctk q8_0 -ctv q8_0";
const Q36: &str = "-ngl all --n-cpu-moe 18 -fa on --load-mode none";
const OSS: &str = "-ngl all -fa on";

fn variants() -> Vec<Variant> {
    let v = |name, base, args, extra| Variant {
        name,
        base,
        args,
        extra,
        ctx: None,
        temperature: None,
        baseline: false,
    };
    vec![
        // `default` = the shipped profile; these also run the cache control, prewarm and two-slot tests.
        Variant { baseline: true, ..v("qwen3.8 default", "qwen3.8-27b", None, "") },
        v("qwen3.8 plain", "qwen3.8-27b", Some(Q38), ""),
        v("qwen3.8 mtp", "qwen3.8-27b", Some(Q38), "--spec-type draft-mtp"),
        v("qwen3.8 ngram-mod", "qwen3.8-27b", Some(Q38), "--spec-type ngram-mod"),
        v("qwen3.8 ngram-simple", "qwen3.8-27b", Some(Q38), "--spec-type ngram-simple"),
        Variant { temperature: Some(0.3), ..v("qwen3.8 mtp temp0.3", "qwen3.8-27b", Some(Q38), "--spec-type draft-mtp") },
        // Longer drafts: n-max 5 gained nothing on first runs; 7 spilled out of VRAM (38 tok/s).
        v("qwen3.8 mtpn5", "qwen3.8-27b", Some(Q38), "--spec-type draft-mtp,ngram-mod --spec-draft-n-max 5"),
        Variant { baseline: true, ..v("qwen3.6 default", "qwen3.6-35b-a3b", None, "") },
        v("qwen3.6 plain", "qwen3.6-35b-a3b", Some(Q36), ""),
        v("qwen3.6 ub2048", "qwen3.6-35b-a3b", Some(Q36), "-ub 2048 -b 2048"),
        v("qwen3.6 ngram-mod", "qwen3.6-35b-a3b", Some(Q36), "--spec-type ngram-mod"),
        v("qwen3.6 q8kv ncmoe16", "qwen3.6-35b-a3b", Some("-ngl all --n-cpu-moe 16 -fa on --load-mode none -ctk q8_0 -ctv q8_0"), ""),
        v("qwen3.6 q8kv ncmoe14", "qwen3.6-35b-a3b", Some("-ngl all --n-cpu-moe 14 -fa on --load-mode none -ctk q8_0 -ctv q8_0"), ""),
        v("qwen3.6 combo ncmoe16", "qwen3.6-35b-a3b", Some("-ngl all --n-cpu-moe 16 -fa on --load-mode none -ctk q8_0 -ctv q8_0 -ub 2048 -b 2048"), ""),
        v("qwen3.6 combo ncmoe14", "qwen3.6-35b-a3b", Some("-ngl all --n-cpu-moe 14 -fa on --load-mode none -ctk q8_0 -ctv q8_0 -ub 2048 -b 2048"), ""),
        Variant { baseline: true, ..v("gpt-oss default", "gpt-oss-20b", None, "") },
        Variant { ctx: Some(65536), ..v("gpt-oss ctx64k", "gpt-oss-20b", Some(OSS), "") },
        v("gpt-oss ngram-mod", "gpt-oss-20b", Some(OSS), "--spec-type ngram-mod"),
    ]
}

const PERFILE_SPEC: &str = "## Summary\nAdd a module docstring to both handlers.\n## Files\n- `src/big_a.py` (modify) — docstring\n- `src/big_b.py` (modify) — docstring\n## Changes\n### `src/big_a.py`\n1. Add a one-line module docstring at the very top: `\"\"\"Handler A built on the response models in src/models.py.\"\"\"`.\n### `src/big_b.py`\n1. Add a one-line module docstring at the very top: `\"\"\"Handler B built on the response models in src/models.py.\"\"\"`.\n## Edge cases\n- Keep everything else unchanged.\n## Acceptance criteria\n- Both files start with the docstring; nothing else changes.\n";

#[derive(Serialize, Default, Clone)]
struct RunStat {
    job: String,
    valid: bool,
    attempts: usize,
    tok_s: Option<f64>,
    completion: u32,
    /// (prompt tokens, cached tokens) of each call's first attempt.
    calls: Vec<(u32, u32)>,
    prompt_tps: Option<f64>,
    draft: Option<(u32, u32)>,
    secs: f64,
    error: Option<String>,
}

#[derive(Serialize, Default)]
struct VariantReport {
    name: String,
    launch_args: String,
    ctx: u32,
    temperature: f32,
    loaded: bool,
    load_secs: Option<f64>,
    vram_mib: Option<u64>,
    pp_tps: Option<f64>,
    runs: Vec<RunStat>,
    acceptance_log: Option<String>,
    extra: Vec<String>,
    error: Option<String>,
}

fn stat(job: &str, r: &ImplementResult, secs: f64) -> RunStat {
    let attempts: Vec<_> = r.calls.iter().flat_map(|c| c.attempts.iter()).collect();
    let (dn, da) = attempts.iter().fold((0, 0), |(n, a), x| {
        (n + x.draft_tokens.unwrap_or(0), a + x.draft_accepted.unwrap_or(0))
    });
    RunStat {
        job: job.into(),
        valid: r.validation_ok,
        attempts: attempts.len(),
        tok_s: r.avg_tok_per_sec,
        completion: attempts.iter().map(|a| a.completion_tokens.unwrap_or(0)).sum(),
        calls: r
            .calls
            .iter()
            .filter_map(|c| c.attempts.first())
            .map(|a| (a.prompt_tokens.unwrap_or(0), a.cached_tokens.unwrap_or(0)))
            .collect(),
        prompt_tps: attempts.first().and_then(|a| a.prompt_per_sec),
        draft: (dn > 0).then_some((dn, da)),
        secs,
        error: r.error.clone(),
    }
}

fn fmt_opt(x: Option<f64>, digits: usize) -> String {
    x.map(|v| format!("{v:.digits$}")).unwrap_or_else(|| "–".into())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        for v in variants() {
            println!("{}", v.name);
        }
        return;
    }
    let filters: Vec<String> = args.iter().filter(|a| !a.starts_with("--")).cloned().collect();
    let force = args.iter().any(|a| a == "--force");
    let base = std::env::var("RELAY_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("relay-bench"));
    let app_dir = base.join("appdata");
    let _ = std::fs::remove_dir_all(&app_dir);
    std::fs::create_dir_all(&app_dir).unwrap();
    // SAFETY: set before the runtime starts any threads.
    unsafe { std::env::set_var("RELAY_APP_DIR", &app_dir) };
    if let Some((used, total)) = gpu_memory() {
        println!("VRAM in use before the benchmark: {used} / {total} MiB");
        if used > 2500 && !force {
            eprintln!("More than 2.5 GB of VRAM is already in use (a game or another model?). Close it or pass --force.");
            std::process::exit(2);
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let code = rt.block_on(run(&base, &filters));
    std::process::exit(code);
}

async fn run(base: &Path, filters: &[String]) -> i32 {
    let sink: Sink = Arc::new(NullSink);
    let core = AppCore::new(sink).expect("core");
    let mut settings = core.settings();
    settings.llama_server_path =
        std::env::var("RELAY_LLAMA_SERVER").unwrap_or_else(|_| r"D:\LocalAI-models\llama.cpp-b12360\bin\llama-server.exe".into());
    settings.models_dir = std::env::var("RELAY_MODELS_DIR").unwrap_or_else(|_| r"D:\LocalAI-models".into());
    core.set_settings(settings.clone()).unwrap();
    let project = base.join("project");
    let root = project.to_string_lossy().into_owned();
    let builtins = core.profiles();

    let mut reports = Vec::new();
    for v in variants() {
        if !filters.is_empty() && !filters.iter().any(|f| v.name.contains(f.as_str())) {
            continue;
        }
        println!("\n== {}", v.name);
        let mut p: Profile = builtins.iter().find(|p| p.id == v.base).unwrap().clone();
        p.id = format!("bench-{}", v.name.replace([' ', '+', '.'], "-"));
        p.builtin = false;
        p.name = v.name.into();
        p.launch_args = format!("{} {}", v.args.unwrap_or(&p.launch_args), v.extra).trim().to_string();
        if let Some(c) = v.ctx {
            p.context_limit = c;
        }
        if let Some(t) = v.temperature {
            p.sampling.temperature = t;
        }
        let mut list = core.profiles();
        list.retain(|x| x.id != p.id);
        list.push(p.clone());
        core.set_profiles(list).unwrap();
        let mut rep = VariantReport {
            name: v.name.into(),
            launch_args: p.launch_args.clone(),
            ctx: p.context_limit,
            temperature: p.sampling.temperature,
            ..Default::default()
        };

        let st = core.switch_profile(&p.id).await;
        let st = match st {
            Ok(st) if st.state == ServerState::Ready => st,
            Ok(st) => {
                rep.error = Some(format!("{} {}", st.message, st.hint.unwrap_or_default()));
                println!("   load failed: {:?}", rep.error);
                let tail: Vec<String> = core.server.log_lines().into_iter().rev().take(8).collect();
                rep.extra.extend(tail.into_iter().rev());
                reports.push(rep);
                continue;
            }
            Err(e) => {
                rep.error = Some(e.to_string());
                reports.push(rep);
                continue;
            }
        };
        rep.loaded = true;
        rep.load_secs = st.load_secs;
        tokio::time::sleep(Duration::from_millis(500)).await;
        rep.vram_mib = gpu_memory().map(|(u, _)| u);
        println!("   loaded in {:?}s, VRAM {:?} MiB", st.load_secs, rep.vram_mib);
        let ep = Endpoint {
            base_url: st.base_url.clone().unwrap_or_default(),
            model: String::new(),
            api_key: None,
        };

        // Prompt processing on a long prompt (~12k tokens), cache off.
        rep.pp_tps = prompt_speed(&ep, &p).await;
        println!("   prompt processing {} tok/s", fmt_opt(rep.pp_tps, 0));

        // Single call on the retry spec, twice (run 2 has the prompt cached).
        for i in 1..=2 {
            e2e::make_project(&project);
            let t = Instant::now();
            let job = core.implement(cmd(&root, e2e::RETRY_SPEC, &[], SplitMode::Single, &p.id, &format!("single{i}")));
            match tokio::time::timeout(Duration::from_secs(300), job).await {
                Ok(Ok(r)) => {
                    let s = stat(&format!("single run {i}"), &r.result, t.elapsed().as_secs_f64());
                    println!("   single run {i}: valid {} · {} tok/s · drafts {:?} · {:.1}s", s.valid, fmt_opt(s.tok_s, 1), s.draft, s.secs);
                    rep.runs.push(s);
                }
                Ok(Err(e)) => rep.runs.push(RunStat { job: format!("single run {i}"), error: Some(e.to_string()), ..Default::default() }),
                Err(_) => {
                    rep.runs.push(RunStat { job: format!("single run {i}"), error: Some("timeout (300 s): VRAM spill?".into()), ..Default::default() });
                    break;
                }
            }
        }
        if let Some(line) = core.server.log_lines().iter().rev().find(|l| l.contains("acceptance")) {
            rep.acceptance_log = Some(line.trim().to_string());
        }

        // Per-file run with a shared read-only context (cache checkpoint reuse).
        e2e::make_project(&project);
        write_big_files(&project);
        let ctx = vec!["src/models.py".to_string()];
        if v.baseline {
            // Control: the same per-file calls without priming.
            match control(&core, &root, &ctx, &p, &ep).await {
                Some((a, b)) => rep.extra.push(format!("control without priming: call 2 reused {} of {} prompt tokens (call 1: {} of {})", b.1, b.0, a.1, a.0)),
                None => rep.extra.push("control failed".into()),
            }
        }
        let t = Instant::now();
        let job = core.implement(cmd(&root, PERFILE_SPEC, &ctx, SplitMode::PerFile, &p.id, "perfile"));
        if let Ok(Ok(r)) = tokio::time::timeout(Duration::from_secs(300), job).await {
            let s = stat("per-file", &r.result, t.elapsed().as_secs_f64());
            println!("   per-file: valid {} · calls (prompt, cached) {:?}", s.valid, s.calls);
            rep.runs.push(s);
        } else {
            rep.runs.push(RunStat { job: "per-file".into(), error: Some("failed or timed out".into()), ..Default::default() });
        }

        if v.baseline {
            // Spec-review prewarm on the real server, then the run.
            e2e::make_project(&project);
            let mut s = core.settings();
            s.prewarm_prompt = true;
            core.set_settings(s).unwrap();
            let pw = core
                .prewarm(PrewarmCommand {
                    project_root: root.clone(),
                    spec: e2e::RETRY_SPEC.into(),
                    context_files: vec![],
                    full_context: vec![],
                    profile_id: p.id.clone(),
                    mode: Some(SplitMode::Single),
                })
                .await;
            let mut s = core.settings();
            s.prewarm_prompt = false;
            core.set_settings(s).unwrap();
            match pw {
                Ok(pw) => {
                    let primed = pw.outcomes.last().map(|o| o.tokens).unwrap_or(0);
                    let ms: u64 = pw.outcomes.iter().map(|o| o.ms).sum();
                    let t = Instant::now();
                    if let Ok(r) = core.implement(cmd(&root, e2e::RETRY_SPEC, &[], SplitMode::Single, &p.id, "prewarmed")).await {
                        let s = stat("single after prewarm", &r.result, t.elapsed().as_secs_f64());
                        rep.extra.push(format!(
                            "prewarm: {} ({primed} tokens in {ms} ms); the run then reused {:?} (prompt, cached)",
                            pw.status, s.calls
                        ));
                        rep.runs.push(s);
                    }
                }
                Err(e) => rep.extra.push(format!("prewarm error: {e}")),
            }
        }
        reports.push(rep);
    }
    let _ = core.stop_server().await;

    // Two slots: is decoding two per-file calls at once faster than one after the other?
    if filters.is_empty() || filters.iter().any(|f| "parallel".contains(f.as_str())) {
        for id in ["qwen3.8-27b", "gpt-oss-20b"] {
            let p = builtins.iter().find(|p| p.id == id).unwrap();
            match parallel_test(&settings, p, &project).await {
                Ok(line) => {
                    println!("\n== parallel {id}: {line}");
                    if let Some(r) = reports.iter_mut().find(|r| r.name.starts_with(&id[..7]) && r.name.ends_with("default")) {
                        r.extra.push(format!("two slots (-np 2): {line}"));
                    }
                }
                Err(e) => println!("\n== parallel {id}: failed: {e}"),
            }
        }
    }

    let md = markdown(&reports);
    std::fs::create_dir_all(base).unwrap();
    std::fs::write(base.join("report.md"), &md).unwrap();
    std::fs::write(base.join("report.json"), serde_json::to_string_pretty(&reports).unwrap()).unwrap();
    println!("\n{md}\nReport: {}", base.join("report.md").display());
    0
}

fn cmd(root: &str, spec: &str, ctx: &[String], mode: SplitMode, profile: &str, run: &str) -> ImplementCommand {
    ImplementCommand {
        job_id: "bench".into(),
        run_id: Some(format!("{run}-{}", uuid::Uuid::new_v4().simple())),
        project_root: root.into(),
        instruction: "bench".into(),
        spec: spec.into(),
        context_files: ctx.to_vec(),
        mode: Some(mode),
        profile_id: profile.into(),
        full_context: ctx.to_vec(),
    }
}

fn write_big_files(project: &Path) {
    let big = format!(
        "{}\n{}",
        e2e::CLIENT_PY,
        (0..60).map(|i| format!("# handler note {i}: keep this line\n")).collect::<String>()
    );
    std::fs::write(project.join("src/big_a.py"), &big).unwrap();
    std::fs::write(project.join("src/big_b.py"), &big).unwrap();
}

async fn prompt_speed(ep: &Endpoint, p: &Profile) -> Option<f64> {
    // ~12k tokens of source code.
    let text = e2e::CLIENT_PY.repeat(55);
    let msgs = vec![ChatMessage::user(format!("{text}\n\nReply with OK."))];
    let mut body = build_body(p, "", &msgs, 1);
    body["cache_prompt"] = serde_json::json!(false);
    let http = reqwest::Client::new();
    let o = stream_chat(&http, ep, &body, &CancellationToken::new(), &mut |_, _| {}).await.ok()?;
    o.prompt_per_sec
}

async fn control(core: &AppCore, root: &str, ctx: &[String], p: &Profile, ep: &Endpoint) -> Option<((u32, u32), (u32, u32))> {
    let prep = core.prepare(root, PERFILE_SPEC, ctx, ctx, &p.id).await.ok()?;
    let system = relay_lib::templates::load(relay_lib::templates::Template::ImplementSystem);
    let msgs = relay_lib::pipeline::call_messages(&prep, PERFILE_SPEC, SplitMode::PerFile, &system);
    let http = reqwest::Client::new();
    let mut out = Vec::new();
    for m in &msgs {
        let o = stream_chat(&http, ep, &build_body(p, "", m, 256), &CancellationToken::new(), &mut |_, _| {})
            .await
            .ok()?;
        out.push((o.prompt_tokens.unwrap_or(0), o.cached_tokens.unwrap_or(0)));
    }
    Some((*out.first()?, *out.get(1)?))
}

/// Start llama-server with two slots and time two requests one after the other, then together.
async fn parallel_test(settings: &relay_lib::config::Settings, p: &Profile, project: &Path) -> anyhow::Result<String> {
    let exe = settings.llama_server_path.clone();
    let gguf = p.resolved_gguf(Path::new(&settings.models_dir));
    let port = 39391u16;
    let mut args = relay_lib::server::build_args(p, &gguf, port);
    if let Some(i) = args.iter().position(|a| a == "-np") {
        args[i + 1] = "2".into();
    }
    // Keep the profile's context (a bigger KV cache would not fit next to Qwen3.8) and let both
    // slots share it; each test request is ~2k tokens.
    args.push("--kv-unified".into());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let _server = e2e::Helper::spawn(&exe, &argv);
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    let t = Instant::now();
    loop {
        if http.get(format!("{base}/health")).send().await.is_ok_and(|r| r.status().is_success()) {
            break;
        }
        if t.elapsed() > Duration::from_secs(240) {
            anyhow::bail!("server did not become ready (VRAM?)");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let vram = gpu_memory().map(|(u, _)| u);
    let ep = Endpoint {
        base_url: format!("{base}/v1"),
        model: String::new(),
        api_key: None,
    };
    let client = std::fs::read_to_string(project.join("src/http_client.py")).unwrap_or_default();
    let req = |n: usize| {
        let msgs = vec![ChatMessage::user(format!(
            "Rewrite this file with a docstring on every method (variant {n}). Output only the code.\n\n{client}"
        ))];
        let mut b = build_body(p, "", &msgs, 400);
        b["cache_prompt"] = serde_json::json!(false);
        b
    };
    let one = |n: usize| {
        let http = http.clone();
        let ep = ep.clone();
        let body = req(n);
        async move {
            stream_chat(&http, &ep, &body, &CancellationToken::new(), &mut |_, _| {})
                .await
                .map(|o| o.completion_tokens.unwrap_or(0))
                .unwrap_or(0)
        }
    };
    let t = Instant::now();
    let a = one(1).await + one(2).await;
    let seq = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let (b1, b2) = tokio::join!(one(3), one(4));
    let par = t.elapsed().as_secs_f64();
    Ok(format!(
        "sequential {a} tokens in {seq:.1}s ({:.0} tok/s) vs concurrent {} tokens in {par:.1}s ({:.0} tok/s); VRAM {vram:?} MiB",
        a as f64 / seq,
        b1 + b2,
        (b1 + b2) as f64 / par
    ))
}

fn markdown(reports: &[VariantReport]) -> String {
    let mut s = String::from(
        "| Variant | Launch args | Load | VRAM | Prompt tok/s | Gen tok/s (run 1 / 2) | Drafts accepted | Valid | Per-file call 2 cached |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for r in reports {
        if !r.loaded {
            s.push_str(&format!("| {} | `{}` | failed: {} | | | | | | |\n", r.name, r.launch_args, r.error.clone().unwrap_or_default()));
            continue;
        }
        let single: Vec<&RunStat> = r.runs.iter().filter(|x| x.job.starts_with("single run")).collect();
        let tps = single.iter().map(|x| fmt_opt(x.tok_s, 1)).collect::<Vec<_>>().join(" / ");
        let drafts = single
            .iter()
            .filter_map(|x| x.draft)
            .map(|(n, a)| format!("{:.0}%", 100.0 * a as f64 / n.max(1) as f64))
            .collect::<Vec<_>>()
            .join(" / ");
        let valid = r.runs.iter().filter(|x| x.valid).count();
        let pf = r
            .runs
            .iter()
            .find(|x| x.job == "per-file")
            .and_then(|x| x.calls.get(1))
            .map(|(p, c)| format!("{c} of {p}"))
            .unwrap_or_else(|| "–".into());
        s.push_str(&format!(
            "| {} | `{}`{} | {} s | {:.1} GB | {} | {} | {} | {}/{} | {} |\n",
            r.name,
            r.launch_args,
            if r.ctx != 32768 { format!(" ctx {}", r.ctx) } else { String::new() },
            fmt_opt(r.load_secs, 1),
            r.vram_mib.unwrap_or(0) as f64 / 1024.0,
            fmt_opt(r.pp_tps, 0),
            tps,
            if drafts.is_empty() { "–".into() } else { drafts },
            valid,
            r.runs.len(),
            pf
        ));
    }
    for r in reports {
        if !r.extra.is_empty() || r.acceptance_log.is_some() {
            s.push_str(&format!("\n**{}**\n", r.name));
            if let Some(a) = &r.acceptance_log {
                s.push_str(&format!("- server: `{a}`\n"));
            }
            for e in &r.extra {
                s.push_str(&format!("- {e}\n"));
            }
        }
    }
    s
}
