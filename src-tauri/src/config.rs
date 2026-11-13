//! Settings and model profiles, persisted as JSON under `%LOCALAPPDATA%\Relay`.

use crate::reasoning::ReasoningControl;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 39281;

/// `%LOCALAPPDATA%\Relay` — settings, profiles, history, templates, pid files.
pub fn app_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RELAY_APP_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("Relay")
}

pub fn default_models_dir() -> PathBuf {
    app_dir().join("models")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ClaudeBackend {
    #[default]
    Cli,
    Api,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub models_dir: String,
    /// Explicit llama-server.exe; empty → auto-locate.
    pub llama_server_path: String,
    pub server_port: u16,
    pub active_profile: String,
    pub claude_backend: ClaudeBackend,
    pub claude_model: String,
    /// Effort for Claude (`--effort` on the CLI, `output_config.effort` on the API).
    pub claude_effort: String,
    /// Let the Claude CLI read the project with read-only tools (Read/Glob/Grep) while writing the spec.
    pub claude_cli_explore: bool,
    pub review_enabled: bool,
    pub auto_fallback: bool,
    pub fallback_profile: String,
    /// Send large read-only context files to the local model as outlines (bodies omitted).
    pub outline_context: bool,
    pub last_project: String,
    pub load_timeout_secs: u64,
    /// Measured load times per profile id (seconds), used for the load-progress estimate.
    pub load_stats: std::collections::BTreeMap<String, f64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            models_dir: default_models_dir().to_string_lossy().into_owned(),
            llama_server_path: String::new(),
            server_port: DEFAULT_PORT,
            active_profile: "qwen3.8-27b".into(),
            claude_backend: ClaudeBackend::Cli,
            claude_model: "claude-opus-5-5".into(),
            claude_effort: "high".into(),
            claude_cli_explore: true,
            review_enabled: false,
            auto_fallback: false,
            fallback_profile: "gpt-oss-20b".into(),
            outline_context: true,
            last_project: String::new(),
            load_timeout_secs: 600,
            load_stats: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ProfileKind {
    #[default]
    Managed,
    External,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: i32,
    pub min_p: f32,
    pub presence_penalty: f32,
    pub repeat_penalty: f32,
}

impl Default for Sampling {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_p: 0.8,
            top_k: 20,
            min_p: 0.0,
            presence_penalty: 0.0,
            repeat_penalty: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ExternalEndpoint {
    /// OpenAI-compatible base URL including `/v1`, e.g. `http://localhost:1234/v1`.
    pub base_url: String,
    /// Model name to send in requests (LM Studio/Ollama need it; llama-server ignores it).
    pub model: String,
    /// If true, an API key is stored in Windows Credential Manager for this profile.
    pub use_api_key: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Profile {
    pub id: String,
    pub builtin: bool,
    pub name: String,
    pub role: String,
    pub kind: ProfileKind,
    /// Absolute path to the GGUF. Empty → `<models_dir>/<hf_file>`.
    pub gguf_path: String,
    pub hf_repo: String,
    pub hf_file: String,
    pub hf_size: u64,
    pub hf_sha256: String,
    /// Extra llama-server arguments. `-m`, `--host`, `--port`, `-c` and `-np` are added by Relay.
    pub launch_args: String,
    pub context_limit: u32,
    pub max_output_tokens: u32,
    pub sampling: Sampling,
    pub reasoning: ReasoningControl,
    pub external: ExternalEndpoint,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: String::new(),
            builtin: false,
            name: "New profile".into(),
            role: String::new(),
            kind: ProfileKind::Managed,
            gguf_path: String::new(),
            hf_repo: String::new(),
            hf_file: String::new(),
            hf_size: 0,
            hf_sha256: String::new(),
            launch_args: "-ngl all -fa on".into(),
            context_limit: 32768,
            max_output_tokens: 8192,
            sampling: Sampling::default(),
            reasoning: ReasoningControl::qwen(),
            external: ExternalEndpoint::default(),
        }
    }
}

impl Profile {
    pub fn resolved_gguf(&self, models_dir: &Path) -> PathBuf {
        if !self.gguf_path.trim().is_empty() {
            PathBuf::from(self.gguf_path.trim())
        } else {
            models_dir.join(&self.hf_file)
        }
    }
}

/// The three shipped profiles. Values verified against Hugging Face and measured on an
/// RTX 5070 Ti (16 GB) with llama.cpp b12360 — see README.
pub fn builtin_profiles() -> Vec<Profile> {
    vec![
        Profile {
            id: "qwen3.8-27b".into(),
            builtin: true,
            name: "Qwen3.8-27B".into(),
            role: "Best quality (default) — dense, fully on GPU".into(),
            hf_repo: "unsloth/Qwen3.8-27B-GGUF".into(),
            hf_file: "Qwen3.8-27B-UD-IQ4_XS.gguf".into(),
            hf_size: 14_352_911_264,
            hf_sha256: "4b194b34ecec99fdfb74caef84445ccd01c987de87d96ae8ca50dc434b4b0a38".into(),
            // q8_0 KV cache: 32k context at f16 would need 2 GiB and not fit next to the weights.
            launch_args: "-ngl all -fa on -ctk q8_0 -ctv q8_0".into(),
            context_limit: 32768,
            max_output_tokens: 8192,
            sampling: Sampling {
                temperature: 0.7,
                top_p: 0.8,
                top_k: 20,
                min_p: 0.0,
                presence_penalty: 0.0,
                repeat_penalty: 1.0,
            },
            reasoning: ReasoningControl::qwen(),
            ..Default::default()
        },
        Profile {
            id: "qwen3.6-35b-a3b".into(),
            builtin: true,
            name: "Qwen3.6-35B-A3B".into(),
            role: "Fast, long context — MoE, experts partly in RAM".into(),
            hf_repo: "ggml-org/Qwen3.6-35B-A3B-GGUF".into(),
            hf_file: "Qwen3.6-35B-A3B-Q4_K_M.gguf".into(),
            hf_size: 20_542_732_576,
            hf_sha256: "f4ffe03c9139e6a905a5d346f77d9fabdd179367c3280dfd005d1e75ca5ed8c8".into(),
            // --n-cpu-moe 18 keeps total VRAM ≈14.5 GB (measured); 16 ≈15.4 GB, 20 ≈13.7 GB.
            launch_args: "-ngl all --n-cpu-moe 18 -fa on --load-mode none".into(),
            context_limit: 65536,
            max_output_tokens: 8192,
            sampling: Sampling {
                temperature: 0.7,
                top_p: 0.8,
                top_k: 20,
                min_p: 0.0,
                presence_penalty: 0.0,
                repeat_penalty: 1.0,
            },
            reasoning: ReasoningControl::qwen(),
            ..Default::default()
        },
        Profile {
            id: "gpt-oss-20b".into(),
            builtin: true,
            name: "gpt-oss-20b".into(),
            role: "Most reliable format-following — fallback".into(),
            hf_repo: "ggml-org/gpt-oss-20b-GGUF".into(),
            hf_file: "gpt-oss-20b-MXFP4.gguf".into(),
            hf_size: 12_109_566_624,
            hf_sha256: "27cd6c432c7672cb812a92f611cf3ba7bbc35928262bb1e1253ff4ee6ae35901".into(),
            launch_args: "-ngl all -fa on".into(),
            context_limit: 65536,
            max_output_tokens: 8192,
            // OpenAI recommends temperature 1.0 / top_p 1.0 for gpt-oss.
            sampling: Sampling {
                temperature: 1.0,
                top_p: 1.0,
                top_k: 0,
                min_p: 0.0,
                presence_penalty: 0.0,
                repeat_penalty: 1.0,
            },
            reasoning: ReasoningControl::gpt_oss(),
            ..Default::default()
        },
    ]
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(
            serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load_settings() -> Settings {
    read_json(&app_dir().join("settings.json")).ok().flatten().unwrap_or_default()
}

pub fn save_settings(s: &Settings) -> Result<()> {
    write_json(&app_dir().join("settings.json"), s)
}

/// Load profiles; built-ins that are missing from the file are added back.
pub fn load_profiles() -> Vec<Profile> {
    let mut list: Vec<Profile> = read_json(&app_dir().join("profiles.json")).ok().flatten().unwrap_or_default();
    for b in builtin_profiles().into_iter().rev() {
        if !list.iter().any(|p| p.id == b.id) {
            list.insert(0, b);
        }
    }
    list
}

pub fn save_profiles(list: &[Profile]) -> Result<()> {
    write_json(&app_dir().join("profiles.json"), &list)
}

/// Split a launch-argument string into arguments, honouring double quotes.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_args_handles_quotes() {
        assert_eq!(split_args("-ngl all  -fa on"), vec!["-ngl", "all", "-fa", "on"]);
        assert_eq!(
            split_args(r#"--chat-template-kwargs "{\"a\":1}" -x"#.replace("\\\"", "'").as_str()),
            vec!["--chat-template-kwargs", "{'a':1}", "-x"]
        );
        assert_eq!(split_args(r#"--alias "my model""#), vec!["--alias", "my model"]);
        assert_eq!(split_args(r#"-a """#), vec!["-a", ""]);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn builtins_are_consistent() {
        let b = builtin_profiles();
        assert_eq!(b.len(), 3);
        assert!(b.iter().all(|p| p.builtin && !p.hf_sha256.is_empty() && p.hf_size > 0));
        assert_eq!(b[2].reasoning.mode, crate::reasoning::ReasoningMode::Effort);
        assert!(b[1].launch_args.contains("--n-cpu-moe"));
    }

    #[test]
    fn profile_json_roundtrip_with_missing_fields() {
        let p: Profile = serde_json::from_str(r#"{"id":"x","name":"Custom"}"#).unwrap();
        assert_eq!(p.context_limit, 32768);
        assert_eq!(p.kind, ProfileKind::Managed);
    }
}
