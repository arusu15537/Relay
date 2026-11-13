//! Path-safety checks: nothing may be read or written outside the selected project folder.
//!
//! Two layers:
//! 1. `normalize_rel` — purely lexical. Rejects absolute paths, drive letters, UNC paths, `..`,
//!    alternate data streams, Windows reserved device names, trailing dots/spaces and `.git`.
//! 2. `resolve_in_project` — filesystem-aware. Canonicalises the deepest existing ancestor so
//!    symlinks and junctions that point outside the project are rejected too.

use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("absolute paths are not allowed: `{0}`")]
    Absolute(String),
    #[error("path escapes the project folder: `{0}`")]
    Escapes(String),
    #[error("path contains an invalid character or component: `{0}`")]
    Invalid(String),
    #[error("writing inside .git is not allowed: `{0}`")]
    GitDir(String),
    #[error("project folder is not accessible: {0}")]
    Root(String),
}

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4",
    "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$", "CONOUT$",
];

/// Normalise a model- or user-supplied relative path to `a/b/c` form, or reject it.
pub fn normalize_rel(path: &str) -> Result<String, PathError> {
    let raw = path.trim();
    if raw.is_empty() {
        return Err(PathError::Empty);
    }
    let s = raw.replace('\\', "/");
    if s.starts_with('/') || s.starts_with("//") {
        return Err(PathError::Absolute(raw.into()));
    }
    // Drive letters (C:...), alternate data streams (file.txt:stream) and URLs all contain ':'.
    if s.contains(':') {
        return Err(PathError::Absolute(raw.into()));
    }
    let mut parts: Vec<&str> = Vec::new();
    for comp in s.split('/') {
        match comp {
            "" | "." => continue,
            ".." => return Err(PathError::Escapes(raw.into())),
            c => {
                if c.chars()
                    .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | '"' | '|' | '?' | '*'))
                {
                    return Err(PathError::Invalid(raw.into()));
                }
                if c.ends_with('.') || c.ends_with(' ') || c.starts_with(' ') {
                    return Err(PathError::Invalid(raw.into()));
                }
                let stem = c.split('.').next().unwrap_or(c).to_ascii_uppercase();
                if RESERVED.contains(&stem.as_str()) {
                    return Err(PathError::Invalid(raw.into()));
                }
                if c.eq_ignore_ascii_case(".git") {
                    return Err(PathError::GitDir(raw.into()));
                }
                parts.push(c);
            }
        }
    }
    if parts.is_empty() {
        return Err(PathError::Empty);
    }
    Ok(parts.join("/"))
}

fn canon(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p)
}

fn starts_with_ci(child: &Path, root: &Path) -> bool {
    let c = child.to_string_lossy().to_lowercase().replace('/', "\\");
    let r = root.to_string_lossy().to_lowercase().replace('/', "\\");
    let r = r.trim_end_matches('\\');
    c == r || c.starts_with(&format!("{r}\\"))
}

/// Resolve `rel` inside `root`, following symlinks/junctions of existing components, and make
/// sure the result stays within the canonical project root.
pub fn resolve_in_project(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    let norm = normalize_rel(rel)?;
    let root_c = canon(root).map_err(|e| PathError::Root(format!("{}: {e}", root.display())))?;
    let joined = root_c.join(norm.replace('/', "\\"));
    // Find the deepest existing ancestor (or the path itself) and canonicalise it.
    let mut existing = joined.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.symlink_metadata().is_ok() {
            break;
        }
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return Err(PathError::Escapes(rel.into())),
        }
    }
    let existing_c = canon(&existing).map_err(|_| PathError::Escapes(rel.into()))?;
    if !starts_with_ci(&existing_c, &root_c) {
        return Err(PathError::Escapes(rel.into()));
    }
    let mut out = existing_c;
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    // Writing into the canonical .git dir is rejected even if reached via a junction.
    let git = root_c.join(".git");
    if starts_with_ci(&out, &git) {
        return Err(PathError::GitDir(rel.into()));
    }
    Ok(out)
}

/// Convert an absolute path inside the project back to the normalised relative form.
pub fn to_rel(root: &Path, abs: &Path) -> Option<String> {
    let root_c = canon(root).ok()?;
    let abs_c = canon(abs).unwrap_or_else(|_| abs.to_path_buf());
    if !starts_with_ci(&abs_c, &root_c) {
        return None;
    }
    let r = root_c.to_string_lossy().len();
    let s = abs_c.to_string_lossy();
    let tail = s.get(r..)?.trim_start_matches(['\\', '/']);
    normalize_rel(tail).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalisation() {
        assert_eq!(normalize_rel("src\\a.rs").unwrap(), "src/a.rs");
        assert_eq!(normalize_rel("./src//b/./c.ts").unwrap(), "src/b/c.ts");
        assert_eq!(normalize_rel("  src/x.py  ").unwrap(), "src/x.py");
    }

    #[test]
    fn lexical_rejections() {
        assert!(matches!(normalize_rel(""), Err(PathError::Empty)));
        assert!(matches!(normalize_rel("."), Err(PathError::Empty)));
        assert!(matches!(normalize_rel("/etc/passwd"), Err(PathError::Absolute(_))));
        assert!(matches!(normalize_rel("C:\\Windows\\x"), Err(PathError::Absolute(_))));
        assert!(matches!(normalize_rel("c:x"), Err(PathError::Absolute(_))));
        assert!(matches!(normalize_rel("\\\\server\\share\\x"), Err(PathError::Absolute(_))));
        assert!(matches!(normalize_rel("a.txt:stream"), Err(PathError::Absolute(_))));
        assert!(matches!(normalize_rel("../x"), Err(PathError::Escapes(_))));
        assert!(matches!(normalize_rel("a/../../x"), Err(PathError::Escapes(_))));
        assert!(matches!(normalize_rel("a/b/.."), Err(PathError::Escapes(_))));
        assert!(matches!(normalize_rel("src/NUL"), Err(PathError::Invalid(_))));
        assert!(matches!(normalize_rel("con.txt"), Err(PathError::Invalid(_))));
        assert!(matches!(normalize_rel("a/b./c"), Err(PathError::Invalid(_))));
        assert!(matches!(normalize_rel("a/b /c"), Err(PathError::Invalid(_))));
        assert!(matches!(normalize_rel("a/b?.txt"), Err(PathError::Invalid(_))));
        assert!(matches!(normalize_rel(".git/config"), Err(PathError::GitDir(_))));
        assert!(matches!(normalize_rel("sub/.GIT/hooks/x"), Err(PathError::GitDir(_))));
    }

    #[test]
    fn dotfiles_are_fine() {
        assert_eq!(normalize_rel(".gitignore").unwrap(), ".gitignore");
        assert_eq!(normalize_rel(".github/workflows/ci.yml").unwrap(), ".github/workflows/ci.yml");
    }

    #[test]
    fn resolves_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "x").unwrap();
        let p = resolve_in_project(dir.path(), "src/a.rs").unwrap();
        assert!(p.ends_with("src\\a.rs") || p.ends_with("src/a.rs"));
        // Non-existent nested path is fine as long as its existing ancestor is inside.
        let q = resolve_in_project(dir.path(), "src/new/deep/file.rs").unwrap();
        assert!(q.to_string_lossy().contains("new"));
        assert_eq!(to_rel(dir.path(), &p).unwrap(), "src/a.rs");
    }

    #[test]
    fn rejects_traversal_at_resolve_time() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_in_project(dir.path(), "../outside.txt").is_err());
        assert!(resolve_in_project(dir.path(), "C:/Windows/system32/x").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_junction_escape() {
        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        // Directory junctions don't need admin rights on Windows.
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(status.status.success(), "mklink failed: {status:?}");
        let r = resolve_in_project(root.path(), "link/evil.txt");
        assert!(matches!(r, Err(PathError::Escapes(_))), "{r:?}");
    }

    #[cfg(windows)]
    #[test]
    fn case_insensitive_root_prefix() {
        assert!(starts_with_ci(Path::new("C:\\Proj\\src\\a"), Path::new("c:\\proj")));
        assert!(!starts_with_ci(Path::new("C:\\Project2\\a"), Path::new("C:\\Proj")));
    }
}
