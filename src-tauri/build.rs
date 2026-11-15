use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    // Stamp the build with its commit so the app can show which version is running
    // (Settings header): `0d9e6d7 2026-11-15`, `-dirty` for uncommitted changes.
    let build = match (git(&["rev-parse", "--short", "HEAD"]), git(&["show", "-s", "--format=%cs", "HEAD"])) {
        (Some(hash), Some(date)) => {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            format!("{hash}{} {date}", if dirty { "-dirty" } else { "" })
        }
        _ => "unknown build".into(),
    };
    println!("cargo:rustc-env=RELAY_BUILD={build}");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/logs/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    tauri_build::build()
}
