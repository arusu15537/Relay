//! The staging copy: a mirror of the project under `%LOCALAPPDATA%\Relay\work\<name>-<hash>\src`
//! where the coder's edits are written and the verification commands run, so the real project
//! is untouched until files are accepted.
//!
//! The mirror is kept between runs: syncing copies only what changed (size or modification time)
//! and deletes files the project no longer has, while ignored paths (build output such as
//! `target/`, which holds the compile cache) stay. Files a run wrote are recorded in `dirty.txt`
//! next to the mirror and restored from the project at the next sync.

use crate::config::app_dir;
use crate::pathsafe::{normalize_rel, resolve_in_project};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Never copied into the mirror (and never deleted from it): VCS data, dependencies and caches.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
];
/// Projects with more files than this are not mirrored (staging would take too long).
pub const MAX_FILES: usize = 50_000;

#[derive(Debug, Clone)]
pub struct Staging {
    /// The real project.
    pub project: PathBuf,
    /// The mirror (where commands run).
    pub root: PathBuf,
    /// `dirty.txt`: paths a run wrote into the mirror.
    manifest: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStats {
    pub files: usize,
    pub copied: usize,
    pub deleted: usize,
    pub restored: usize,
    pub ms: u64,
}

fn key_for(project: &Path) -> String {
    use sha2::Digest;
    let canon = project
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    let hash = hex::encode(sha2::Sha256::digest(canon.as_bytes()));
    let name: String = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    format!("{name}-{}", &hash[..10])
}

/// A directory that is a build cache: Cargo, and other tools following the cache-directory
/// convention, put a `CACHEDIR.TAG` file in it.
fn is_cache_dir(path: &Path) -> bool {
    path.join("CACHEDIR.TAG").is_file()
}

/// Project files (relative, forward slashes) as the mirror should have them: .gitignore rules
/// apply, VCS, dependency and cache directories are skipped.
fn walk(root: &Path) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .git_ignore(true)
        .git_exclude(true)
        .filter_entry(|e| {
            let name = e.file_name();
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            !(is_dir && (SKIP_DIRS.iter().any(|s| name.eq_ignore_ascii_case(s)) || is_cache_dir(e.path())))
        })
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
        out.insert(rel);
        if out.len() > MAX_FILES {
            bail!("The project has more than {MAX_FILES} files; it is too large to verify in a staging copy.");
        }
    }
    Ok(out)
}

/// `canonicalize` returns verbatim paths (`\\?\C:\...`) on Windows; cmd.exe, npm and other build
/// tools mishandle them as a working directory, so plain drive paths are used.
fn plain(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p,
    }
}

fn same_file(src: &std::fs::Metadata, dst: &std::fs::Metadata) -> bool {
    src.len() == dst.len()
        && match (src.modified(), dst.modified()) {
            (Ok(s), Ok(d)) => s <= d,
            _ => false,
        }
}

impl Staging {
    /// The mirror for `project` (not created yet).
    pub fn for_project(project: &Path) -> Result<Self> {
        Self::at(project, &app_dir().join("work").join(key_for(project)))
    }

    /// A mirror in `dir` (tests use a temporary directory).
    pub fn at(project: &Path, dir: &Path) -> Result<Self> {
        let project = plain(
            project
                .canonicalize()
                .with_context(|| format!("project folder {}", project.display()))?,
        );
        std::fs::create_dir_all(dir)?;
        let dir = plain(dir.canonicalize()?);
        let root = dir.join("src");
        if root.starts_with(&project) || project.starts_with(&dir) {
            bail!(
                "The staging folder {} and the project {} must not contain each other.",
                root.display(),
                project.display()
            );
        }
        Ok(Self {
            project,
            root,
            manifest: dir.join("dirty.txt"),
        })
    }

    fn dirty(&self) -> BTreeSet<String> {
        std::fs::read_to_string(&self.manifest)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    }

    fn add_dirty(&self, rel: &str) -> Result<()> {
        let mut d = self.dirty();
        if d.insert(rel.to_string()) {
            std::fs::write(&self.manifest, d.into_iter().collect::<Vec<_>>().join("\n"))?;
        }
        Ok(())
    }

    /// Copy a project file into the mirror. `touch`: give it a fresh modification time instead of
    /// the project's (a file restored after a run's edit must look newer than the build output
    /// compiled from the edit, or incremental builds would keep the stale result).
    fn copy_in(&self, rel: &str, touch: bool) -> Result<()> {
        let src = resolve_in_project(&self.project, rel).map_err(|e| anyhow!("{e}"))?;
        let dst = self.root.join(rel.replace('/', "\\"));
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let r = if touch {
            std::fs::read(&src).and_then(|b| std::fs::write(&dst, b))
        } else {
            std::fs::copy(&src, &dst).map(|_| ())
        };
        r.with_context(|| format!("copying {rel} into the staging folder"))
    }

    /// Make the mirror match the project. Build output in the mirror is kept.
    pub fn sync(&self) -> Result<SyncStats> {
        let started = std::time::Instant::now();
        std::fs::create_dir_all(&self.root)?;
        let mut stats = SyncStats::default();
        let files = walk(&self.project)?;
        stats.files = files.len();
        // Files the last run wrote: restore (or remove) them whatever their timestamps say.
        for rel in self.dirty() {
            let dst = self.root.join(rel.replace('/', "\\"));
            if files.contains(&rel) {
                self.copy_in(&rel, true)?;
            } else if dst.is_file() {
                std::fs::remove_file(&dst)?;
            }
            stats.restored += 1;
        }
        let _ = std::fs::remove_file(&self.manifest);
        for rel in &files {
            let src = self.project.join(rel.replace('/', "\\"));
            let dst = self.root.join(rel.replace('/', "\\"));
            let fresh = match (std::fs::metadata(&src), std::fs::metadata(&dst)) {
                (Ok(s), Ok(d)) => same_file(&s, &d),
                _ => false,
            };
            if !fresh {
                self.copy_in(rel, false)?;
                stats.copied += 1;
            }
        }
        // Files the project no longer has (the mirror's own .gitignore copies keep build output).
        for rel in walk(&self.root)? {
            if !files.contains(&rel) {
                std::fs::remove_file(self.root.join(rel.replace('/', "\\")))?;
                stats.deleted += 1;
            }
        }
        self.link_node_modules();
        stats.ms = started.elapsed().as_millis() as u64;
        Ok(stats)
    }

    /// Node projects: use the project's installed dependencies through a junction instead of a copy.
    fn link_node_modules(&self) {
        let src = self.project.join("node_modules");
        let dst = self.root.join("node_modules");
        if !src.is_dir() || dst.exists() {
            return;
        }
        #[cfg(windows)]
        {
            let mut cmd = std::process::Command::new("cmd");
            cmd.arg("/C").arg("mklink").arg("/J").arg(&dst).arg(&src);
            crate::winproc::hide_window_std(&mut cmd);
            let _ = cmd.output();
        }
        #[cfg(not(windows))]
        {
            let _ = std::os::unix::fs::symlink(&src, &dst);
        }
    }

    /// Write a file of the change into the mirror (never outside it).
    pub fn write(&self, rel: &str, content: &str) -> Result<()> {
        let abs = resolve_in_project(&self.root, rel).map_err(|e| anyhow!("{e}"))?;
        let rel = normalize_rel(rel).map_err(|e| anyhow!("{e}"))?;
        self.add_dirty(&rel)?;
        if let Some(dir) = abs.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&abs, content).with_context(|| format!("writing {rel} into the staging folder"))?;
        Ok(())
    }

    /// Current text of a file in the mirror.
    pub fn read(&self, rel: &str) -> Result<Option<String>> {
        crate::project::read_text(&self.root, rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, Staging) {
        let p = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(p.path().join("src")).unwrap();
        std::fs::write(p.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(p.path().join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(p.path().join(".gitignore"), "/target\n*.log\n").unwrap();
        std::fs::write(p.path().join("debug.log"), "ignored\n").unwrap();
        std::fs::create_dir_all(p.path().join("target/debug")).unwrap();
        std::fs::write(p.path().join("target/debug/app.exe"), "binary").unwrap();
        let s = Staging::at(p.path(), w.path()).unwrap();
        (p, w, s)
    }

    #[test]
    fn sync_copies_the_project_without_ignored_files() {
        let (_p, _w, s) = setup();
        let st = s.sync().unwrap();
        assert_eq!((st.files, st.copied), (3, 3), "{st:?}");
        assert!(s.root.join("src/main.rs").is_file() && s.root.join(".gitignore").is_file());
        assert!(!s.root.join("debug.log").exists() && !s.root.join("target").exists());
        let again = s.sync().unwrap();
        assert_eq!((again.copied, again.deleted), (0, 0), "nothing changed: {again:?}");
    }

    #[test]
    fn edits_are_restored_and_build_output_is_kept() {
        let (p, _w, s) = setup();
        s.sync().unwrap();
        // A build in the mirror leaves output behind; a run edits and creates files.
        std::fs::create_dir_all(s.root.join("target/debug")).unwrap();
        std::fs::write(s.root.join("target/debug/cache.bin"), "cache").unwrap();
        s.write("src/main.rs", "fn main() { edited(); }\n").unwrap();
        s.write("src/new.rs", "pub fn x() {}\n").unwrap();
        assert_eq!(s.read("src/main.rs").unwrap().as_deref(), Some("fn main() { edited(); }\n"));
        // The project changes in the meantime: a file removed, one added.
        std::fs::remove_file(p.path().join("Cargo.toml")).unwrap();
        std::fs::write(p.path().join("README.md"), "hi\n").unwrap();
        let st = s.sync().unwrap();
        assert_eq!(st.restored, 2, "{st:?}");
        assert_eq!(std::fs::read_to_string(s.root.join("src/main.rs")).unwrap(), "fn main() {}\n");
        assert!(!s.root.join("src/new.rs").exists(), "a file the run created is removed");
        assert!(!s.root.join("Cargo.toml").exists() && s.root.join("README.md").is_file());
        assert!(s.root.join("target/debug/cache.bin").is_file(), "ignored build output survives");
    }

    #[test]
    fn writes_stay_inside_the_mirror() {
        let (_p, _w, s) = setup();
        s.sync().unwrap();
        assert!(s.write("../escape.txt", "x").is_err());
        assert!(s.write("C:/Windows/x.txt", "x").is_err());
    }

    #[test]
    fn mirror_and_project_must_not_nest() {
        let p = tempfile::tempdir().unwrap();
        assert!(Staging::at(p.path(), &p.path().join("work")).is_err());
        let w = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(w.path().join("proj")).unwrap();
        assert!(Staging::at(&w.path().join("proj"), w.path()).is_err());
    }

    #[test]
    fn cache_directories_are_skipped_even_without_gitignore() {
        let p = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(p.path().join("target")).unwrap();
        std::fs::write(p.path().join("target/CACHEDIR.TAG"), "Signature: 8a477f597d28d172789f06886806bc55").unwrap();
        std::fs::write(p.path().join("target/big.rlib"), "x").unwrap();
        let s = Staging::at(p.path(), w.path()).unwrap();
        assert_eq!(s.sync().unwrap().files, 1);
        assert_eq!(key_for(Path::new(r"C:\Users\x\My Game")).len(), "My_Game-".len() + 10);
        assert!(!s.root.to_string_lossy().starts_with(r"\\?\"), "plain path: {}", s.root.display());
        assert_eq!(plain(PathBuf::from(r"\\?\C:\a\b")), PathBuf::from(r"C:\a\b"));
        assert_eq!(plain(PathBuf::from(r"\\?\UNC\srv\share")), PathBuf::from(r"\\?\UNC\srv\share"));
    }
}
