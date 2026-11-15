//! Editable prompt templates. Defaults are compiled in; the user's copies live in
//! `%LOCALAPPDATA%\Relay\templates\*.md` and are created on first use.

use crate::config::app_dir;
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    Expand,
    ImplementSystem,
    Review,
}

impl Template {
    pub const ALL: [Template; 3] = [Template::Expand, Template::ImplementSystem, Template::Review];

    pub fn file_name(self) -> &'static str {
        match self {
            Template::Expand => "expand.md",
            Template::ImplementSystem => "implement-system.md",
            Template::Review => "review.md",
        }
    }

    pub fn default_text(self) -> &'static str {
        match self {
            Template::Expand => include_str!("../templates/expand.md"),
            Template::ImplementSystem => include_str!("../templates/implement-system.md"),
            Template::Review => include_str!("../templates/review.md"),
        }
    }

    pub fn from_name(name: &str) -> Option<Template> {
        Template::ALL
            .into_iter()
            .find(|t| t.file_name() == name || t.file_name().trim_end_matches(".md") == name)
    }
}

pub fn templates_dir() -> PathBuf {
    app_dir().join("templates")
}

pub fn path_of(t: Template) -> PathBuf {
    templates_dir().join(t.file_name())
}

fn digest(text: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(text.replace("\r\n", "\n").as_bytes()))
}

/// Sidecar recording which default a copy was created from, so unedited copies can be upgraded
/// when Relay ships a better default (edited copies are never touched).
fn origin_path(t: Template) -> PathBuf {
    templates_dir().join(format!(".{}.origin", t.file_name()))
}

fn write_default(t: Template) {
    let _ = std::fs::create_dir_all(templates_dir());
    let _ = std::fs::write(path_of(t), t.default_text());
    let _ = std::fs::write(origin_path(t), digest(t.default_text()));
}

/// Load the user's template, creating it from the default if missing and upgrading it if it is
/// an unedited copy of an older default.
pub fn load(t: Template) -> String {
    let p = path_of(t);
    match std::fs::read_to_string(&p) {
        Ok(s) if !s.trim().is_empty() => {
            let unedited = std::fs::read_to_string(origin_path(t)).is_ok_and(|o| o.trim() == digest(&s));
            if unedited && s.replace("\r\n", "\n") != t.default_text().replace("\r\n", "\n") {
                write_default(t);
                return t.default_text().to_string();
            }
            s
        }
        _ => {
            write_default(t);
            t.default_text().to_string()
        }
    }
}

pub fn save(t: Template, text: &str) -> Result<()> {
    std::fs::create_dir_all(templates_dir())?;
    std::fs::write(path_of(t), text)?;
    Ok(())
}

pub fn reset(t: Template) -> Result<String> {
    write_default(t);
    Ok(t.default_text().to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInfo {
    pub name: String,
    pub path: String,
    pub text: String,
    pub is_default: bool,
}

pub fn list() -> Vec<TemplateInfo> {
    Template::ALL
        .into_iter()
        .map(|t| {
            let text = load(t);
            TemplateInfo {
                name: t.file_name().into(),
                path: path_of(t).to_string_lossy().into_owned(),
                is_default: text == t.default_text(),
                text,
            }
        })
        .collect()
}

/// Replace `{{key}}` placeholders. Unknown placeholders are left as-is.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    // Collapse the blank runs left by empty optional placeholders.
    while out.contains("\n\n\n\n") {
        out = out.replace("\n\n\n\n", "\n\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_replaces_known_placeholders() {
        let t = "A {{x}} B {{y}} C {{unknown}}";
        assert_eq!(render(t, &[("x", "1"), ("y", "2")]), "A 1 B 2 C {{unknown}}");
    }

    #[test]
    fn defaults_contain_expected_placeholders() {
        let e = Template::Expand.default_text();
        for k in [
            "{{instruction}}",
            "{{context_limit}}",
            "{{budget_guidance}}",
            "{{file_tree}}",
            "{{context_files}}",
        ] {
            assert!(e.contains(k), "expand template lacks {k}");
        }
        assert!(
            e.contains("Do not write the implementation"),
            "the expand template must forbid code"
        );
        assert!(!e.contains("include the code"), "the expand template must not invite code");
        assert!(e.contains("## Design") && e.contains("class diagram"), "specs are design-level");
        assert!(e.contains("no pseudo-code"), "no algorithms written as prose either");
        assert!(Template::Review.default_text().contains("=== FOLLOW-UP SPEC ==="));
        assert!(Template::Review.default_text().contains("do not write the code"));
        assert!(Template::ImplementSystem.default_text().contains("<<<<<<< SEARCH"));
    }
}
