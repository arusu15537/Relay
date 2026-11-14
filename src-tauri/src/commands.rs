//! Tauri command wrappers around `AppCore`.

use crate::app::{AcceptOutcome, AppCore, ExpandRequest, ExpandResult, ImplementCommand, ImplementResponse, ProfileTest};
use crate::claude::{self, Review};
use crate::config::{self, Profile, Settings};
use crate::download::{self, DownloadOutcome, HfFile, LlamaRelease};
use crate::history::{JobRecord, JobSummary, RunRecord};
use crate::pipeline::Prepared;
use crate::project::{self, FileList, GitStatus};
use crate::server::{self, ServerStatus};
use crate::spec::{ParsedSpec, parse_spec};
use crate::templates::{self, Template, TemplateInfo};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::State;

type Core<'a> = State<'a, Arc<AppCore>>;
type R<T> = Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStateDto {
    settings: Settings,
    profiles: Vec<Profile>,
    server: ServerStatus,
    cli_path: Option<String>,
    llama_server: Option<String>,
    app_dir: String,
    templates_dir: String,
    startup_notes: Vec<String>,
    has_api_key: bool,
    version: String,
}

#[tauri::command]
pub fn get_app_state(core: Core<'_>) -> AppStateDto {
    let settings = core.settings();
    AppStateDto {
        llama_server: server::locate_llama_server(&settings).map(|p| p.to_string_lossy().into_owned()),
        settings,
        profiles: core.profiles(),
        server: core.server.status(),
        cli_path: claude::locate_cli().map(|p| p.to_string_lossy().into_owned()),
        app_dir: config::app_dir().to_string_lossy().into_owned(),
        templates_dir: templates::templates_dir().to_string_lossy().into_owned(),
        startup_notes: core.startup_notes.clone(),
        has_api_key: claude::get_secret(claude::KEYRING_ANTHROPIC).is_some(),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

#[tauri::command]
pub fn save_settings(core: Core<'_>, settings: Settings) -> R<()> {
    core.set_settings(settings).map_err(e)
}

#[tauri::command]
pub fn save_profiles(core: Core<'_>, profiles: Vec<Profile>) -> R<Vec<Profile>> {
    core.set_profiles(profiles).map_err(e)?;
    Ok(core.profiles())
}

#[tauri::command]
pub fn reset_profile(core: Core<'_>, id: String) -> R<Profile> {
    core.reset_profile(&id).map_err(e)
}

#[tauri::command]
pub async fn switch_profile(core: Core<'_>, id: String) -> R<ServerStatus> {
    core.switch_profile(&id).await.map_err(e)
}

#[tauri::command]
pub async fn stop_server(core: Core<'_>) -> R<()> {
    core.stop_server().await.map_err(e)
}

#[tauri::command]
pub fn server_status(core: Core<'_>) -> ServerStatus {
    core.server.status()
}

#[tauri::command]
pub fn server_log(core: Core<'_>) -> Vec<String> {
    core.server.log_lines()
}

#[tauri::command]
pub async fn gpu_memory() -> Option<(u64, u64)> {
    tokio::task::spawn_blocking(server::gpu_memory).await.ok().flatten()
}

#[tauri::command]
pub async fn test_profile(core: Core<'_>, id: String) -> R<ProfileTest> {
    core.test_profile(&id).await.map_err(e)
}

#[tauri::command]
pub async fn test_claude(core: Core<'_>) -> R<String> {
    claude::test_backend(&core.settings(), &core.sink, &core.stream_http)
        .await
        .map_err(e)
}

#[tauri::command]
pub async fn list_files(root: String) -> R<FileList> {
    tokio::task::spawn_blocking(move || project::list_files(Path::new(&root), 20000))
        .await
        .map_err(e)?
        .map_err(e)
}

#[tauri::command]
pub fn read_project_file(root: String, path: String) -> R<Option<String>> {
    project::read_text(Path::new(&root), &path).map_err(e)
}

#[tauri::command]
pub async fn git_status(root: String) -> GitStatus {
    tokio::task::spawn_blocking(move || project::git_status(Path::new(&root)))
        .await
        .unwrap_or_default()
}

#[tauri::command]
pub async fn git_checkpoint(root: String, message: String) -> R<String> {
    tokio::task::spawn_blocking(move || project::git_checkpoint(Path::new(&root), &message))
        .await
        .map_err(e)?
        .map_err(e)
}

#[tauri::command]
pub async fn expand(core: Core<'_>, req: ExpandRequest) -> R<ExpandResult> {
    core.expand(req).await.map_err(e)
}

#[tauri::command]
pub fn parse_spec_cmd(spec: String) -> ParsedSpec {
    parse_spec(&spec)
}

#[tauri::command]
pub async fn prepare_implement(
    core: Core<'_>,
    root: String,
    spec: String,
    context_files: Vec<String>,
    full_context: Option<Vec<String>>,
    profile_id: String,
) -> R<Prepared> {
    core.prepare(&root, &spec, &context_files, &full_context.unwrap_or_default(), &profile_id)
        .await
        .map_err(e)
}

#[tauri::command]
pub async fn implement(core: Core<'_>, cmd: ImplementCommand) -> R<ImplementResponse> {
    core.implement(cmd).await.map_err(e)
}

#[tauri::command]
pub fn cancel_job(core: Core<'_>, id: String) -> bool {
    core.cancel(&id)
}

#[tauri::command]
pub fn accept_file(core: Core<'_>, run_id: String, path: String, content: Option<String>, force: bool) -> R<AcceptOutcome> {
    core.accept_file(&run_id, &path, content, force).map_err(e)
}

#[tauri::command]
pub fn reject_file(core: Core<'_>, run_id: String, path: String) -> R<()> {
    core.reject_file(&run_id, &path).map_err(e)
}

#[tauri::command]
pub async fn review_run(core: Core<'_>, run_id: String, job_id: String) -> R<Review> {
    core.review(&run_id, &job_id).await.map_err(e)
}

#[tauri::command]
pub fn history_list(core: Core<'_>, query: String) -> R<Vec<JobSummary>> {
    core.history.list_jobs(200, &query).map_err(e)
}

#[derive(Serialize)]
pub struct JobDetail {
    job: JobRecord,
    runs: Vec<RunRecord>,
}

#[tauri::command]
pub fn history_get(core: Core<'_>, id: String) -> R<Option<JobDetail>> {
    Ok(core.history.get_job(&id).map_err(e)?.map(|(job, runs)| JobDetail { job, runs }))
}

#[tauri::command]
pub fn history_delete(core: Core<'_>, id: String) -> R<()> {
    core.history.delete_job(&id).map_err(e)
}

#[tauri::command]
pub fn templates_list() -> Vec<TemplateInfo> {
    templates::list()
}

#[tauri::command]
pub fn template_save(name: String, text: String) -> R<()> {
    let t = Template::from_name(&name).ok_or("unknown template")?;
    templates::save(t, &text).map_err(e)
}

#[tauri::command]
pub fn template_reset(name: String) -> R<String> {
    let t = Template::from_name(&name).ok_or("unknown template")?;
    templates::reset(t).map_err(e)
}

#[tauri::command]
pub fn set_api_key(value: String) -> R<()> {
    claude::set_secret(claude::KEYRING_ANTHROPIC, value.trim()).map_err(e)
}

#[tauri::command]
pub fn set_endpoint_key(profile_id: String, value: String) -> R<()> {
    claude::set_secret(&AppCore::endpoint_key(&profile_id), value.trim()).map_err(e)
}

#[tauri::command]
pub fn has_endpoint_key(profile_id: String) -> bool {
    claude::get_secret(&AppCore::endpoint_key(&profile_id)).is_some()
}

#[tauri::command]
pub fn open_in_explorer(path: String) -> R<()> {
    let p = PathBuf::from(&path);
    let mut cmd = std::process::Command::new("explorer");
    if p.is_file() {
        cmd.arg(format!("/select,{}", p.display()));
    } else {
        cmd.arg(&p);
    }
    cmd.spawn().map(|_| ()).map_err(e)
}

#[tauri::command]
pub fn path_exists(path: String) -> bool {
    Path::new(&path).exists()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelFileStatus {
    path: String,
    exists: bool,
    size: u64,
    expected_size: u64,
    partial: u64,
}

#[tauri::command]
pub fn model_file_status(core: Core<'_>, profile_id: String) -> R<ModelFileStatus> {
    let p = core.profile(&profile_id).map_err(e)?;
    let path = p.resolved_gguf(Path::new(&core.settings().models_dir));
    let meta = std::fs::metadata(&path).ok();
    let partial = std::fs::metadata(format!("{}.part", path.display())).map(|m| m.len()).unwrap_or(0);
    Ok(ModelFileStatus {
        path: path.to_string_lossy().into_owned(),
        exists: meta.is_some(),
        size: meta.map(|m| m.len()).unwrap_or(0),
        expected_size: p.hf_size,
        partial,
    })
}

#[tauri::command]
pub async fn hf_list(core: Core<'_>, repo: String) -> R<Vec<HfFile>> {
    download::list_repo_ggufs(&core.http, &repo).await.map_err(e)
}

/// Download a GGUF for a profile. The UI must have shown the file name and size and received
/// the user's confirmation before calling this.
#[tauri::command]
pub async fn download_model(core: Core<'_>, profile_id: String, file: String, size: u64, sha256: Option<String>) -> R<DownloadOutcome> {
    let mut profile = core.profile(&profile_id).map_err(e)?;
    let models_dir = PathBuf::from(core.settings().models_dir);
    let rel = crate::pathsafe::normalize_rel(&file).map_err(e)?;
    let dest = models_dir.join(rel.replace('/', "\\"));
    let id = format!("model:{profile_id}");
    let cancel = core.register_job(&id);
    let url = download::hf_url(&profile.hf_repo, &file);
    let out = download::download(
        &core.stream_http,
        &id,
        &url,
        &dest,
        sha256.as_deref(),
        Some(size).filter(|s| *s > 0),
        &core.sink,
        &cancel,
    )
    .await;
    core.finish_job(&id);
    let out = out.map_err(e)?;
    if !out.cancelled {
        profile.hf_file = file;
        profile.hf_size = size;
        profile.hf_sha256 = sha256.unwrap_or_default();
        profile.gguf_path = String::new();
        let mut list = core.profiles();
        if let Some(p) = list.iter_mut().find(|p| p.id == profile_id) {
            *p = profile;
        }
        core.set_profiles(list).map_err(e)?;
    }
    Ok(out)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyOutcome {
    sha256: String,
    expected: String,
    matches: bool,
}

#[tauri::command]
pub async fn verify_model(core: Core<'_>, profile_id: String) -> R<VerifyOutcome> {
    let p = core.profile(&profile_id).map_err(e)?;
    let path = p.resolved_gguf(Path::new(&core.settings().models_dir));
    let sha = download::sha256_file(&path, &core.sink, &format!("verify:{profile_id}"))
        .await
        .map_err(e)?;
    Ok(VerifyOutcome {
        matches: !p.hf_sha256.is_empty() && sha.eq_ignore_ascii_case(&p.hf_sha256),
        expected: p.hf_sha256,
        sha256: sha,
    })
}

#[tauri::command]
pub async fn llama_release(core: Core<'_>) -> R<LlamaRelease> {
    download::latest_llama_release(&core.http).await.map_err(e)
}

/// Download + extract llama.cpp into %LOCALAPPDATA%\Relay\llama.cpp (after user confirmation).
#[tauri::command]
pub async fn download_llama(core: Core<'_>, release: LlamaRelease) -> R<String> {
    let dir = config::app_dir().join("llama.cpp");
    let zips = dir.join("zips");
    let cancel = core.register_job("llama");
    let mut parts = vec![release.server_zip.clone()];
    if let Some(c) = &release.cudart_zip {
        parts.push(c.clone());
    }
    for (name, url, size) in &parts {
        let dest = zips.join(name);
        let r = download::download(
            &core.stream_http,
            "llama",
            url,
            &dest,
            None,
            Some(*size).filter(|s| *s > 0),
            &core.sink,
            &cancel,
        )
        .await;
        if let Err(err) = r {
            core.finish_job("llama");
            return Err(e(err));
        }
        if cancel.is_cancelled() {
            core.finish_job("llama");
            return Err("Download cancelled".into());
        }
        let (d, z) = (dir.clone(), dest.clone());
        tokio::task::spawn_blocking(move || download::extract_zip(&z, &d))
            .await
            .map_err(e)?
            .map_err(e)?;
    }
    core.finish_job("llama");
    let exe = dir.join("llama-server.exe");
    if !exe.is_file() {
        return Err(format!("llama-server.exe not found after extracting into {}", dir.display()));
    }
    let mut s = core.settings();
    s.llama_server_path = exe.to_string_lossy().into_owned();
    core.set_settings(s).map_err(e)?;
    Ok(exe.to_string_lossy().into_owned())
}
