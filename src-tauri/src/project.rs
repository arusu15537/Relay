//! Target-project access: file tree, safe reads/writes (all through `pathsafe`), git status and
//! checkpoint commits, unified diffs.

use crate::editformat::FileSource;
use crate::pathsafe::{normalize_rel, resolve_in_project};
use crate::winproc;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MAX_TEXT_FILE: u64 = 2 * 1024 * 1024;
const ALWAYS_SKIP: &[&str] = &[".git", "node_modules", ".hg", ".svn"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileList {
    pub root: String,
    pub files: Vec<FileEntry>,
    pub truncated: bool,
}

/// List project files (respecting .gitignore, skipping .git and node_modules), sorted.
pub fn list_files(root: &Path, max: usize) -> Result<FileList> {
    if !root.is_dir() {
        bail!("Project folder does not exist: {}", root.display());
    }
    let mut files = Vec::new();
    let mut truncated = false;
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .git_ignore(true)
        .git_exclude(true)
        .filter_entry(|e| !ALWAYS_SKIP.iter().any(|s| e.file_name().eq_ignore_ascii_case(s)))
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if normalize_rel(&rel).is_err() {
            continue;
        }
        if files.len() >= max {
            truncated = true;
            break;
        }
        files.push(FileEntry {
            path: rel,
            size: entry.metadata().map(|m| m.len()).unwrap_or(0),
        });
    }
    files.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    Ok(FileList {
        root: root.to_string_lossy().into_owned(),
        files,
        truncated,
    })
}

/// Render a compact indented tree for prompts.
pub fn tree_text(list: &FileList, max_lines: usize) -> String {
    let mut out = Vec::new();
    let mut last_dirs: Vec<&str> = Vec::new();
    for f in &list.files {
        let parts: Vec<&str> = f.path.split('/').collect();
        let dirs = &parts[..parts.len() - 1];
        let common = last_dirs.iter().zip(dirs.iter()).take_while(|(a, b)| a == b).count();
        for (depth, d) in dirs.iter().enumerate().skip(common) {
            out.push(format!("{}{}/", "  ".repeat(depth), d));
        }
        out.push(format!("{}{}", "  ".repeat(dirs.len()), parts[parts.len() - 1]));
        last_dirs = dirs.to_vec();
        if out.len() >= max_lines {
            out.push(format!("… ({} files total, list truncated)", list.files.len()));
            break;
        }
    }
    if list.truncated {
        out.push("… (more files not listed)".into());
    }
    out.join("\n")
}

/// Read a text file inside the project. `Ok(None)` if it doesn't exist.
pub fn read_text(root: &Path, rel: &str) -> Result<Option<String>> {
    let abs = resolve_in_project(root, rel).map_err(|e| anyhow!("{e}"))?;
    let meta = match std::fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {rel}")),
    };
    if meta.is_dir() {
        bail!("`{rel}` is a directory");
    }
    if meta.len() > MAX_TEXT_FILE {
        bail!("`{rel}` is larger than {} MB", MAX_TEXT_FILE / 1024 / 1024);
    }
    let bytes = std::fs::read(&abs).with_context(|| format!("reading {rel}"))?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        bail!("`{rel}` looks like a binary file");
    }
    let text = String::from_utf8(bytes).map_err(|_| anyhow!("`{rel}` is not valid UTF-8"))?;
    Ok(Some(text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text)))
}

/// File source for the edit applier, enforcing path safety.
pub struct ProjectSource {
    pub root: PathBuf,
}

impl FileSource for ProjectSource {
    fn read(&self, path: &str) -> Result<Option<String>, String> {
        read_text(&self.root, path).map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum WriteConflict {
    /// The file changed on disk since the run read it.
    ChangedOnDisk,
    /// A new file was proposed but the path now exists.
    NowExists,
}

/// Write accepted content. `expected` is the content the run started from (`None` = new file).
/// Refuses to overwrite a file that changed on disk in the meantime unless `force`.
pub fn write_file(
    root: &Path,
    rel: &str,
    content: &str,
    expected: Option<&str>,
    force: bool,
) -> Result<std::result::Result<(), WriteConflict>> {
    let abs = resolve_in_project(root, rel).map_err(|e| anyhow!("{e}"))?;
    if !force {
        let current = read_text(root, rel).ok().flatten();
        match (expected, current.as_deref()) {
            (None, Some(_)) => return Ok(Err(WriteConflict::NowExists)),
            (Some(exp), Some(cur)) if exp != cur => return Ok(Err(WriteConflict::ChangedOnDisk)),
            (Some(_), None) => return Ok(Err(WriteConflict::ChangedOnDisk)),
            _ => {}
        }
    }
    if let Some(dir) = abs.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating folder for {rel}"))?;
        // Re-check after creating directories (a junction could have appeared).
        resolve_in_project(root, rel).map_err(|e| anyhow!("{e}"))?;
    }
    let tmp = abs.with_file_name(format!(".{}.relay-tmp", abs.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::write(&tmp, content.as_bytes()).with_context(|| format!("writing {rel}"))?;
    std::fs::rename(&tmp, &abs).with_context(|| format!("replacing {rel}"))?;
    Ok(Ok(()))
}

pub fn unified_diff(path: &str, old: Option<&str>, new: &str) -> String {
    let old = old.unwrap_or("");
    let diff = similar::TextDiff::from_lines(old, new);
    let a = if old.is_empty() {
        "/dev/null".to_string()
    } else {
        format!("a/{path}")
    };
    diff.unified_diff().context_radius(3).header(&a, &format!("b/{path}")).to_string()
}

// ---------------------------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatus {
    pub is_repo: bool,
    pub branch: String,
    pub dirty: bool,
    pub changes: Vec<String>,
    pub error: Option<String>,
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    winproc::hide_window_std(&mut cmd);
    let out = cmd.output().context("running git (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn git_status(root: &Path) -> GitStatus {
    match git(root, &["rev-parse", "--is-inside-work-tree"]) {
        Ok(s) if s.trim() == "true" => {}
        Ok(_) | Err(_) => return GitStatus::default(),
    }
    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    match git(root, &["status", "--porcelain=v1"]) {
        Ok(s) => {
            let changes: Vec<String> = s.lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect();
            GitStatus {
                is_repo: true,
                branch,
                dirty: !changes.is_empty(),
                changes,
                error: None,
            }
        }
        Err(e) => GitStatus {
            is_repo: true,
            branch,
            dirty: false,
            changes: vec![],
            error: Some(e.to_string()),
        },
    }
}

/// Commit everything in the working tree as a checkpoint. Returns the short hash.
pub fn git_checkpoint(root: &Path, message: &str) -> Result<String> {
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-m", message])?;
    Ok(git(root, &["rev-parse", "--short", "HEAD"])?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_respects_gitignore_and_skips_node_modules() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::create_dir_all(d.path().join("node_modules/x")).unwrap();
        std::fs::create_dir_all(d.path().join("build")).unwrap();
        std::fs::write(d.path().join(".gitignore"), "build/\n").unwrap();
        std::fs::write(d.path().join("src/a.ts"), "a").unwrap();
        std::fs::write(d.path().join("node_modules/x/i.js"), "x").unwrap();
        std::fs::write(d.path().join("build/out.js"), "x").unwrap();
        let l = list_files(d.path(), 100).unwrap();
        let paths: Vec<_> = l.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec![".gitignore", "src/a.ts"]);
        let t = tree_text(&l, 100);
        assert!(t.contains("src/\n  a.ts"));
    }

    #[test]
    fn read_rejects_binary_and_unsafe() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("b.bin"), [0u8, 1, 2]).unwrap();
        assert!(read_text(d.path(), "b.bin").is_err());
        assert!(read_text(d.path(), "../x").is_err());
        assert_eq!(read_text(d.path(), "missing.txt").unwrap(), None);
        std::fs::write(d.path().join("bom.txt"), "\u{feff}hi").unwrap();
        assert_eq!(read_text(d.path(), "bom.txt").unwrap().unwrap(), "hi");
    }

    #[test]
    fn write_detects_conflicts_and_creates_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "v1").unwrap();
        // Stale expectation → conflict.
        assert_eq!(
            write_file(d.path(), "a.txt", "v3", Some("v0"), false).unwrap(),
            Err(WriteConflict::ChangedOnDisk)
        );
        assert_eq!(write_file(d.path(), "a.txt", "v2", Some("v1"), false).unwrap(), Ok(()));
        assert_eq!(std::fs::read_to_string(d.path().join("a.txt")).unwrap(), "v2");
        // New file in nested dir.
        assert_eq!(write_file(d.path(), "x/y/new.txt", "n", None, false).unwrap(), Ok(()));
        assert_eq!(
            write_file(d.path(), "x/y/new.txt", "n2", None, false).unwrap(),
            Err(WriteConflict::NowExists)
        );
        assert!(write_file(d.path(), "../escape.txt", "n", None, true).is_err());
        assert!(write_file(d.path(), ".git/config", "n", None, true).is_err());
    }

    #[test]
    fn unified_diff_has_headers() {
        let d = unified_diff("a.txt", Some("x\ny\n"), "x\nz\n");
        assert!(d.contains("--- a/a.txt") && d.contains("+++ b/a.txt") && d.contains("-y") && d.contains("+z"));
        let n = unified_diff("n.txt", None, "hello\n");
        assert!(n.contains("--- /dev/null"));
    }
}
