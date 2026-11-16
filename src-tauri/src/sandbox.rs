//! Autopilot secure mode.
//!
//! Without the spec review nobody approves what a run executes: the planner's `## Verification`
//! commands, and the code the coder wrote (build scripts, tests, the program itself). Secure mode
//! puts two guards around that:
//!
//! * **Command policy:** an autopilot run only starts when every verification command is a known
//!   build/test tool used in a verification way (`cargo build`, `npm test`, `python -m pytest`,
//!   a program inside the project, …) with no shell operators. Otherwise autopilot stops at the
//!   spec for the user to review.
//! * **Sandbox:** the commands run with a restricted token at *Low integrity* (no privileges).
//!   Windows' mandatory integrity control then denies every write to files, folders and registry
//!   keys that are not labelled Low, i.e. everything except the staging copy and Relay's sandbox
//!   folder (tool caches, TEMP). Low processes also cannot open or send window messages to the
//!   user's normal processes, and the job blocks the clipboard, shutdown and display/system
//!   settings. Reading files and the network stay available (dependency downloads need them).

use crate::config::app_dir;
use crate::verify::VerifyCommand;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The prefix of the error an autopilot run fails with when the policy blocks it (the UI keys
/// on it to pause at the spec instead of reporting a failed run).
pub const AUTOPILOT_BLOCKED: &str = "Autopilot secure mode";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Blocked {
    pub command: String,
    pub reason: String,
}

// ---------------------------------------------------------------------------------------------
// Command policy
// ---------------------------------------------------------------------------------------------

/// How the arguments of an allowed tool are checked.
#[derive(Clone, Copy)]
enum Rule {
    /// Any arguments.
    Any,
    /// The first argument that is not an option must be one of these subcommands.
    Sub(&'static [&'static str]),
    /// `npm` / `pnpm`: verification subcommands; `install` / `ci` only without package names.
    Npm(&'static [&'static str]),
    /// `yarn`: as `Npm`, and a bare `yarn` (install).
    Yarn,
    /// `npx`: only tools a project has as dev dependencies, never `--yes` / `--package`.
    Npx,
    /// `node`: a script, not inline code.
    Node,
    /// `python`: `-m` with a test/check module, or a script inside the project; never `-c`.
    Python,
    /// `cmake`: anything but `-E` (its file and process commands).
    Cmake,
}

const CARGO_SUBS: &[&str] = &[
    "build", "b", "check", "c", "test", "t", "run", "r", "clippy", "fmt", "bench", "doc", "nextest", "metadata", "tree",
];
const NPM_SUBS: &[&str] = &["ci", "install", "i", "test", "t", "tst", "run", "run-script", "start"];
const PNPM_SUBS: &[&str] = &["install", "i", "test", "t", "run", "start"];
const YARN_SUBS: &[&str] = &["install", "test", "run", "build", "start"];
const NPX_TOOLS: &[&str] = &["tsc", "vitest", "jest", "eslint", "prettier", "mocha", "tsx", "ts-node", "vite"];
const PY_MODULES: &[&str] = &[
    "pytest",
    "unittest",
    "compileall",
    "py_compile",
    "mypy",
    "ruff",
    "flake8",
    "pyflakes",
    "pylint",
    "black",
    "doctest",
];

/// Programs a verification command may start (lower case, without `.exe` / `.cmd` / `.bat`).
const TOOLS: &[(&str, Rule)] = &[
    ("cargo", Rule::Sub(CARGO_SUBS)),
    ("npm", Rule::Npm(NPM_SUBS)),
    ("pnpm", Rule::Npm(PNPM_SUBS)),
    ("yarn", Rule::Yarn),
    ("npx", Rule::Npx),
    ("node", Rule::Node),
    ("deno", Rule::Sub(&["test", "check", "lint", "fmt", "run", "task"])),
    ("bun", Rule::Sub(&["test", "run", "install", "build"])),
    ("tsc", Rule::Any),
    ("vitest", Rule::Any),
    ("jest", Rule::Any),
    ("eslint", Rule::Any),
    ("python", Rule::Python),
    ("python3", Rule::Python),
    ("py", Rule::Python),
    ("pytest", Rule::Any),
    ("mypy", Rule::Any),
    ("ruff", Rule::Any),
    ("flake8", Rule::Any),
    ("pylint", Rule::Any),
    ("go", Rule::Sub(&["build", "test", "vet", "run"])),
    ("dotnet", Rule::Sub(&["build", "test", "run", "restore"])),
    ("cmake", Rule::Cmake),
    ("ctest", Rule::Any),
    ("msbuild", Rule::Any),
    ("make", Rule::Any),
    ("mingw32-make", Rule::Any),
    ("nmake", Rule::Any),
    ("ninja", Rule::Any),
    ("mvn", Rule::Any),
    ("mvnw", Rule::Any),
    ("gradle", Rule::Any),
    ("gradlew", Rule::Any),
    ("zig", Rule::Sub(&["build", "test", "run"])),
];

/// Characters that make `cmd` do more than start one program: chaining, pipes, redirection,
/// escapes and variable expansion.
const SHELL_OPERATORS: &[char] = &['&', '|', '<', '>', '^', '%', '!', '\n', '\r'];
/// Extensions `cmd` would hand to a script host or installer instead of running a build step.
const SCRIPT_HOSTS: &[&str] = &[
    ".ps1", ".psm1", ".vbs", ".vbe", ".js", ".jse", ".wsf", ".wsh", ".hta", ".lnk", ".msi", ".msp", ".reg", ".scr", ".cpl",
];

/// Split a command line into words (`"…"` groups, the quotes removed).
fn words(command: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for ch in command.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if quoted {
        return Err("it has an unmatched quote".into());
    }
    if any {
        out.push(cur);
    }
    Ok(out)
}

/// A relative path that stays inside the project (no drive, no root, no `..`).
fn inside_project(path: &str) -> bool {
    let p = path.replace('/', "\\");
    !p.is_empty() && !p.starts_with('\\') && !p.contains(':') && p.split('\\').all(|part| part != "..")
}

fn is_option(w: &str) -> bool {
    w.starts_with('-') || w.starts_with('+') || (w.starts_with('/') && w.len() <= 4 && !w[1..].contains('/'))
}

/// Why `command` may not run unattended, or `Ok` if secure mode allows it.
pub fn check(command: &str) -> Result<(), String> {
    if let Some(op) = command.chars().find(|c| SHELL_OPERATORS.contains(c)) {
        let shown = match op {
            '\n' | '\r' => "a line break".to_string(),
            c => format!("`{c}`"),
        };
        return Err(format!(
            "it uses the shell operator {shown}; secure mode runs one program per command, without chaining, pipes, redirection or variables"
        ));
    }
    let w = words(command)?;
    let Some(first) = w.first() else {
        return Err("it is empty".into());
    };
    let lower = first.to_ascii_lowercase();
    if let Some(ext) = SCRIPT_HOSTS.iter().find(|e| lower.ends_with(*e)) {
        return Err(format!("`{ext}` files are opened by a script host, not run as a build step"));
    }
    let args = &w[1..];
    if lower.contains(['\\', '/', ':']) {
        // A program or script of the project itself (e.g. `target\debug\app.exe`), which the
        // sandbox contains like any other code of the change.
        return if inside_project(first) {
            Ok(())
        } else {
            Err(format!("`{first}` is outside the project"))
        };
    }
    let name = [".exe", ".cmd", ".bat", ".com"]
        .iter()
        .find_map(|e| lower.strip_suffix(e))
        .unwrap_or(&lower);
    let Some((_, rule)) = TOOLS.iter().find(|(t, _)| *t == name) else {
        return Err(format!("`{first}` is not one of the build and test tools secure mode allows"));
    };
    let positional: Vec<&str> = args.iter().map(String::as_str).filter(|a| !is_option(a)).collect();
    let sub = positional.first().map(|s| s.to_ascii_lowercase());
    let need_sub = |allowed: &[&str]| -> Result<(), String> {
        match &sub {
            Some(s) if allowed.contains(&s.as_str()) => Ok(()),
            Some(s) => Err(format!("`{name} {s}` is not a build, test or run subcommand")),
            None => Err(format!("`{name}` needs a subcommand ({})", allowed.join(", "))),
        }
    };
    match rule {
        Rule::Any => Ok(()),
        Rule::Sub(allowed) => need_sub(allowed),
        Rule::Npm(allowed) => {
            need_sub(allowed)?;
            let s = sub.unwrap_or_default();
            if ["install", "i", "ci"].contains(&s.as_str()) && positional.len() > 1 {
                return Err(format!(
                    "`{name} {s} {}` adds a package; secure mode only installs what the project already lists",
                    positional[1]
                ));
            }
            Ok(())
        }
        Rule::Yarn => match &sub {
            None => Ok(()),
            Some(s) if s == "install" && positional.len() > 1 => Err(format!("`yarn install {}` adds a package", positional[1])),
            Some(_) => need_sub(YARN_SUBS),
        },
        Rule::Npx => {
            if let Some(f) = args.iter().find(|a| {
                let a = a.to_ascii_lowercase();
                a == "-y" || a == "--yes" || a == "-p" || a.starts_with("--package") || a == "-c" || a.starts_with("--call")
            }) {
                return Err(format!("`npx {f}` downloads or runs arbitrary packages"));
            }
            need_sub(NPX_TOOLS)
        }
        Rule::Node => {
            if let Some(f) = args
                .iter()
                .find(|a| ["-e", "--eval", "-p", "--print", "-i", "--interactive"].contains(&a.to_ascii_lowercase().as_str()))
            {
                return Err(format!("`node {f}` runs inline code"));
            }
            match positional.first() {
                Some(p) if !inside_project(p) => Err(format!("`{p}` is outside the project")),
                Some(_) => Ok(()),
                None if args.iter().any(|a| a == "--test") => Ok(()),
                None => Err("`node` needs a script of the project".into()),
            }
        }
        Rule::Python => {
            let mut it = args.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-c" => return Err(format!("`{name} -c` runs inline code")),
                    "-m" => {
                        let m = it.next().map(|m| m.to_ascii_lowercase()).unwrap_or_default();
                        return if PY_MODULES.contains(&m.as_str()) {
                            Ok(())
                        } else {
                            Err(format!("`{name} -m {m}` is not a test or check module"))
                        };
                    }
                    s if s.starts_with('-') => {}
                    script => {
                        return if !inside_project(script) {
                            Err(format!("`{script}` is outside the project"))
                        } else if !script.to_ascii_lowercase().ends_with(".py") {
                            Err(format!("`{script}` is not a Python script"))
                        } else {
                            Ok(())
                        };
                    }
                }
            }
            Err(format!("`{name}` needs `-m <module>` or a script of the project"))
        }
        Rule::Cmake => {
            if args.iter().any(|a| a == "-E" || a == "-P") {
                Err("`cmake -E` / `cmake -P` run file and process commands".into())
            } else {
                Ok(())
            }
        }
    }
}

/// The commands secure mode would not run unattended, with the reason.
pub fn blocked(cmds: &[VerifyCommand]) -> Vec<Blocked> {
    cmds.iter()
        .filter_map(|c| {
            check(&c.command).err().map(|reason| Blocked {
                command: c.command.clone(),
                reason,
            })
        })
        .collect()
}

/// The error an autopilot run is refused with.
pub fn blocked_message(blocked: &[Blocked]) -> String {
    let list: Vec<String> = blocked.iter().map(|b| format!("`{}` ({})", b.command, b.reason)).collect();
    format!(
        "{AUTOPILOT_BLOCKED}: the spec's Verification would run {} unattended. Review the spec, then press Implement.",
        list.join("; ")
    )
}

// ---------------------------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------------------------

/// Relay's Low-integrity folder: tool caches and TEMP for sandboxed commands.
pub fn sandbox_home() -> PathBuf {
    app_dir().join("sandbox")
}

/// What a sandboxed command gets: Low integrity, and environment variables that move the tool
/// caches (which normally live in the user profile, where a Low process cannot write) into the
/// sandbox folder.
#[derive(Debug, Clone)]
pub struct Sandbox {
    pub home: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    /// Variables removed from the environment.
    pub unset: Vec<OsString>,
}

/// Cache folders inside the sandbox home, with the variable that points the tool there.
const CACHES: &[(&str, &str)] = &[
    ("TEMP", "tmp"),
    ("TMP", "tmp"),
    ("CARGO_HOME", "cargo"),
    ("npm_config_cache", "npm-cache"),
    ("YARN_CACHE_FOLDER", "yarn-cache"),
    ("PIP_CACHE_DIR", "pip-cache"),
    ("PYTHONPYCACHEPREFIX", "pycache"),
    ("GOCACHE", "go\\cache"),
    ("GOMODCACHE", "go\\mod"),
    ("GOPATH", "go\\path"),
    ("NUGET_PACKAGES", "nuget"),
    ("DOTNET_CLI_HOME", "dotnet"),
    ("GRADLE_USER_HOME", "gradle"),
    ("DENO_DIR", "deno"),
    ("BUN_INSTALL_CACHE_DIR", "bun-cache"),
];

impl Sandbox {
    /// Set the sandbox up for commands that write into `writable` (the staging copy).
    pub fn prepare(writable: &Path) -> Result<Self> {
        Self::prepare_in(&sandbox_home(), writable)
    }

    /// As `prepare`, with the sandbox home in `home` (tests use a temporary folder).
    pub fn prepare_in(home: &Path, writable: &Path) -> Result<Self> {
        std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
        let mut env: Vec<(OsString, OsString)> = Vec::new();
        for (var, sub) in CACHES {
            let dir = home.join(sub);
            std::fs::create_dir_all(&dir)?;
            env.push((var.into(), dir.into_os_string()));
        }
        // Maven has no variable for its repository; Cargo's own config (registries, linkers)
        // is copied fresh each run, so nothing a sandboxed build wrote into it carries over.
        let m2 = home.join("m2");
        std::fs::create_dir_all(&m2)?;
        let maven = format!(
            "-Dmaven.repo.local=\"{}\" {}",
            m2.display(),
            std::env::var("MAVEN_OPTS").unwrap_or_default()
        );
        env.push(("MAVEN_OPTS".into(), maven.trim().into()));
        copy_cargo_config(&home.join("cargo"));
        env.extend(
            [
                ("npm_config_update_notifier", "false"),
                ("DOTNET_NOLOGO", "1"),
                ("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1"),
                ("RELAY_SANDBOX", "low-integrity"),
            ]
            .map(|(k, v)| (k.into(), v.into())),
        );
        imp::label_low(home)?;
        imp::label_low(writable)?;
        Ok(Self {
            home: home.to_path_buf(),
            env,
            // A shared target folder would be outside the staging copy (and not writable).
            unset: vec!["CARGO_TARGET_DIR".into(), "CARGO_BUILD_TARGET_DIR".into()],
        })
    }

    /// The full environment of a sandboxed command: Relay's, with `extra` and the cache folders.
    pub fn environment(&self, extra: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(k, _)| !self.unset.iter().any(|u| u.eq_ignore_ascii_case(k)))
            .collect();
        let mut set = |k: OsString, v: OsString| {
            vars.retain(|(old, _)| !old.eq_ignore_ascii_case(&k));
            vars.push((k, v));
        };
        for (k, v) in extra {
            set(k.into(), v.into());
        }
        for (k, v) in &self.env {
            set(k.clone(), v.clone());
        }
        vars.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
        vars
    }
}

fn copy_cargo_config(dst_home: &Path) {
    let src_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".cargo")));
    for name in ["config.toml", "config"] {
        let dst = dst_home.join(name);
        match src_home.as_ref().map(|h| h.join(name)).filter(|s| s.is_file()) {
            Some(src) => {
                let _ = std::fs::copy(src, &dst);
            }
            None => {
                let _ = std::fs::remove_file(&dst);
            }
        }
    }
}

pub use imp::{LowChild, is_low, spawn_low};

#[cfg(windows)]
mod imp {
    use anyhow::{Result, bail};
    use std::ffi::{OsStr, OsString, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
    use std::path::Path;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HANDLE_FLAG_INHERIT, LocalFree, SetHandleInformation};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetNamedSecurityInfoW, SDDL_REVISION_1,
        SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, GetAce, GetLengthSid,
        GetSecurityDescriptorSacl, GetSidSubAuthority, GetSidSubAuthorityCount, INHERITED_ACE, LABEL_SECURITY_INFORMATION,
        OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES, SYSTEM_MANDATORY_LABEL_ACE, SetTokenInformation,
        TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetCurrentProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
        UpdateProcThreadAttribute,
    };

    const SE_GROUP_INTEGRITY: u32 = 0x20;
    const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;
    const LOW_RID: u32 = 0x1000;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// A handle closed on drop.
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// Memory from a Win32 allocator that must be released with `LocalFree`.
    struct Local(*mut c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }

    /// `path` carries an explicit, inheritable Low mandatory label.
    pub fn is_low(path: &Path) -> bool {
        let p = wide(path.as_os_str());
        unsafe {
            let mut sacl: *mut ACL = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let err = GetNamedSecurityInfoW(
                p.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sacl,
                &mut sd,
            );
            let _sd = Local(sd);
            if err != ERROR_SUCCESS || sacl.is_null() {
                return false;
            }
            for i in 0..(*sacl).AceCount as u32 {
                let mut ace: *mut c_void = std::ptr::null_mut();
                if GetAce(sacl, i, &mut ace) == 0 {
                    continue;
                }
                let header = &*(ace as *const ACE_HEADER);
                let inherit = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
                if header.AceType != SYSTEM_MANDATORY_LABEL_ACE_TYPE
                    || header.AceFlags & inherit != inherit
                    || header.AceFlags & INHERITED_ACE as u8 != 0
                {
                    continue;
                }
                let label = &*(ace as *const SYSTEM_MANDATORY_LABEL_ACE);
                let sid = &label.SidStart as *const u32 as PSID;
                let n = *GetSidSubAuthorityCount(sid);
                if n > 0 && *GetSidSubAuthority(sid, n as u32 - 1) == LOW_RID {
                    return true;
                }
            }
            false
        }
    }

    /// Give `dir` an inheritable Low mandatory label ("no write up"), so Low processes may write
    /// in it. Windows propagates the label to what the folder already holds; it does not follow
    /// junctions out of it (checked in the tests).
    pub fn label_low(dir: &Path) -> Result<()> {
        if is_low(dir) {
            return Ok(());
        }
        let sddl = wide(OsStr::new("S:(ML;OICI;NW;;;LW)"));
        let p = wide(dir.as_os_str());
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
                bail!("building the Low integrity label failed: {}", std::io::Error::last_os_error());
            }
            let _sd = Local(sd);
            let (mut present, mut defaulted) = (0, 0);
            let mut sacl: *mut ACL = std::ptr::null_mut();
            if GetSecurityDescriptorSacl(sd, &mut present, &mut sacl, &mut defaulted) == 0 || present == 0 {
                bail!("reading the Low integrity label failed: {}", std::io::Error::last_os_error());
            }
            let err = SetNamedSecurityInfoW(
                p.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                sacl,
            );
            if err != ERROR_SUCCESS {
                bail!(
                    "labelling {} for the sandbox failed: {}",
                    dir.display(),
                    std::io::Error::from_raw_os_error(err as i32)
                );
            }
        }
        Ok(())
    }

    /// A process started by `spawn_low`.
    pub struct LowChild {
        pub process: OwnedHandle,
        pub pid: u32,
        /// The read end of the pipe that receives stdout and stderr.
        pub output: std::io::PipeReader,
    }

    /// The current token without privileges, at Low integrity.
    fn low_token() -> std::io::Result<Handle> {
        unsafe {
            let mut tok: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
                &mut tok,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let tok = Handle(tok);
            let mut restricted: HANDLE = std::ptr::null_mut();
            if CreateRestrictedToken(
                tok.0,
                DISABLE_MAX_PRIVILEGE,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut restricted,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let restricted = Handle(restricted);
            let low = wide(OsStr::new("S-1-16-4096"));
            let mut sid: PSID = std::ptr::null_mut();
            if ConvertStringSidToSidW(low.as_ptr(), &mut sid) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _sid = Local(sid);
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: SE_GROUP_INTEGRITY,
                },
            };
            if SetTokenInformation(
                restricted.0,
                TokenIntegrityLevel,
                &label as *const _ as *const c_void,
                (std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32) + GetLengthSid(sid),
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(restricted)
        }
    }

    fn environment_block(env: &[(OsString, OsString)]) -> Vec<u16> {
        let mut block = Vec::new();
        for (k, v) in env {
            block.extend(k.encode_wide());
            block.push('=' as u16);
            block.extend(v.encode_wide());
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        block
    }

    /// Start `cmd.exe /D /S /C "<command>"` in `cwd` with a Low integrity token, no console
    /// window, stdin from NUL and stdout + stderr into one pipe. Only those two handles are
    /// inherited. The process is created suspended and put into `job` before it runs, so its
    /// whole tree is in the job from the first instruction.
    pub fn spawn_low(command: &str, cwd: &Path, env: &[(OsString, OsString)], job: &crate::winproc::TreeJob) -> std::io::Result<LowChild> {
        let token = low_token()?;
        let (reader, writer) = std::io::pipe()?;
        let nul = std::fs::OpenOptions::new().read(true).open("NUL")?;
        let writer: RawHandle = writer.into_raw_handle();
        let nul: RawHandle = nul.into_raw_handle();
        let _w = Handle(writer as HANDLE);
        let _n = Handle(nul as HANDLE);
        let inherit = [nul as HANDLE, writer as HANDLE];
        unsafe {
            for h in inherit {
                if SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let mut size = 0usize;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
            // usize elements: the list must be pointer-aligned.
            let mut attr_buf = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
            let attrs = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            struct Attrs(LPPROC_THREAD_ATTRIBUTE_LIST);
            impl Drop for Attrs {
                fn drop(&mut self) {
                    unsafe { DeleteProcThreadAttributeList(self.0) };
                }
            }
            let _attrs = Attrs(attrs);
            if UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherit.as_ptr() as *const c_void,
                std::mem::size_of_val(&inherit),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let mut si: STARTUPINFOEXW = std::mem::zeroed();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = nul as HANDLE;
            si.StartupInfo.hStdOutput = writer as HANDLE;
            si.StartupInfo.hStdError = writer as HANDLE;
            si.lpAttributeList = attrs;

            let comspec = std::env::var_os("ComSpec").filter(|c| Path::new(c).is_file()).unwrap_or_else(|| {
                let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
                Path::new(&root).join("System32\\cmd.exe").into_os_string()
            });
            let app = wide(&comspec);
            let mut line = wide(OsStr::new(&format!("cmd.exe /D /S /C \"{command}\"")));
            let block = environment_block(env);
            let dir = wide(cwd.as_os_str());
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessAsUserW(
                token.0,
                app.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED | crate::winproc::CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                block.as_ptr() as *const c_void,
                dir.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            );
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let process = OwnedHandle::from_raw_handle(pi.hProcess as RawHandle);
            let thread = Handle(pi.hThread);
            if !job.assign_handle(pi.hProcess as RawHandle) || !job.restrict_ui() {
                let e = std::io::Error::last_os_error();
                windows_sys::Win32::System::Threading::TerminateProcess(pi.hProcess, 1);
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("putting the sandboxed process into its job failed: {e}"),
                ));
            }
            ResumeThread(thread.0);
            Ok(LowChild {
                process,
                pid: pi.dwProcessId,
                output: reader,
            })
        }
    }

    /// Wait for the process to end; its exit code.
    pub fn wait(process: &OwnedHandle) -> Option<i32> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
        unsafe {
            let h = process.as_raw_handle() as HANDLE;
            WaitForSingleObject(h, INFINITE);
            let mut code = 0u32;
            (GetExitCodeProcess(h, &mut code) != 0).then_some(code as i32)
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{Result, bail};
    use std::ffi::OsString;
    use std::path::Path;

    pub struct LowChild {
        pub process: std::os::fd::OwnedFd,
        pub pid: u32,
        pub output: std::io::PipeReader,
    }
    pub fn is_low(_path: &Path) -> bool {
        false
    }
    pub fn label_low(_dir: &Path) -> Result<()> {
        bail!("the sandbox needs Windows")
    }
    pub fn spawn_low(
        _command: &str,
        _cwd: &Path,
        _env: &[(OsString, OsString)],
        _job: &crate::winproc::TreeJob,
    ) -> std::io::Result<LowChild> {
        Err(std::io::Error::other("the sandbox needs Windows"))
    }
    pub fn wait(_process: &std::os::fd::OwnedFd) -> Option<i32> {
        None
    }
}

pub use imp::wait as wait_low;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{VerifyKind, detect};

    #[test]
    fn allows_build_and_test_commands() {
        for c in [
            "cargo build",
            "cargo +nightly test --release -q",
            "cargo run --bin app -- --help",
            "npm ci",
            "npm install --no-audit",
            "npm run build",
            "npm test",
            "npx tsc --noEmit",
            "yarn",
            "pnpm run lint",
            "python -m pytest -q",
            "python -m compileall -q .",
            "py -3 -m unittest discover -q",
            "python app.py --selftest",
            "node --test",
            "node scripts/check.js",
            "go test ./...",
            "dotnet build",
            "cmake -S . -B build",
            "cmake --build build",
            "gradlew.bat build -x test",
            "mvn -q compile",
            "target\\debug\\app.exe --version",
            ".\\run_tests.bat",
            "\"build\\out app.exe\"",
        ] {
            assert_eq!(check(c), Ok(()), "{c}");
        }
    }

    #[test]
    fn blocks_everything_else() {
        for (c, why) in [
            ("cargo build && del /q src", "shell operator `&`"),
            ("cargo test | findstr ok", "`|`"),
            ("echo %USERPROFILE%", "`%`"),
            ("type secrets.txt > out.txt", "`>`"),
            ("curl https://example.com/x.sh", "`curl` is not one of"),
            ("powershell -c Remove-Item x", "`powershell` is not one of"),
            ("del /s /q C:\\", "`del` is not one of"),
            ("cargo install ripgrep", "`cargo install` is not"),
            ("cargo", "needs a subcommand"),
            ("npm install left-pad", "adds a package"),
            ("npm publish", "`npm publish`"),
            ("npx -y cowsay hi", "downloads or runs"),
            ("npx cowsay", "`npx cowsay`"),
            ("node -e \"require('fs').rmSync('x')\"", "runs inline code"),
            ("python -c \"import os\"", "runs inline code"),
            ("python -m pip install requests", "`python -m pip`"),
            ("python ..\\other\\x.py", "outside the project"),
            ("C:\\Windows\\System32\\cmd.exe /c dir", "outside the project"),
            ("..\\tool.exe", "outside the project"),
            ("cmake -E remove_directory src", "`cmake -E`"),
            ("setup.ps1", "`.ps1` files"),
            ("git push", "`git` is not one of"),
            ("cargo build \"unterminated", "unmatched quote"),
        ] {
            let err = check(c).expect_err(c);
            assert!(err.contains(why), "{c}: {err}");
        }
    }

    #[test]
    fn every_detected_command_is_allowed() {
        let projects: &[&[(&str, &str)]] = &[
            &[("Cargo.toml", "[package]\nname='x'\n"), ("src/lib.rs", "#[test]\nfn t() {}\n")],
            &[
                (
                    "package.json",
                    r#"{"scripts":{"build":"tsc","test":"vitest"},"devDependencies":{"typescript":"5"}}"#,
                ),
                ("package-lock.json", "{}"),
            ],
            &[
                ("package.json", r#"{"devDependencies":{"typescript":"5"}}"#),
                ("tsconfig.json", "{}"),
            ],
            &[("requirements.txt", "pytest\n"), ("test_app.py", "def test_a(): pass\n")],
            &[("app.py", "print(1)\n"), ("test_app.py", "import unittest\n")],
            &[("go.mod", "module x\n"), ("a_test.go", "package x\n")],
            &[("x.csproj", "<Project>Microsoft.NET.Test.Sdk</Project>")],
            &[("CMakeLists.txt", "project(x)\n")],
            &[("pom.xml", "<project/>")],
            &[("gradlew.bat", "@echo off\n")],
        ];
        for files in projects {
            let d = tempfile::tempdir().unwrap();
            for (p, body) in *files {
                let path = d.path().join(p);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, body).unwrap();
            }
            let det = detect(d.path());
            assert!(!det.commands.is_empty(), "{files:?}");
            assert_eq!(blocked(&det.commands), vec![], "{files:?}");
        }
    }

    #[test]
    fn blocked_lists_reasons() {
        let cmds = vec![
            VerifyCommand::new(VerifyKind::Build, "cargo build"),
            VerifyCommand::new(VerifyKind::Run, "start notepad"),
        ];
        let b = blocked(&cmds);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].command, "start notepad");
        let msg = blocked_message(&b);
        assert!(msg.starts_with(AUTOPILOT_BLOCKED) && msg.contains("`start notepad`"), "{msg}");
    }

    #[test]
    fn environment_moves_caches_into_the_sandbox() {
        let home = tempfile::tempdir().unwrap();
        // `unset` is matched without case, like Windows does (PATH is always set).
        let sb = Sandbox {
            home: home.path().to_path_buf(),
            env: vec![("CARGO_HOME".into(), home.path().join("cargo").into_os_string())],
            unset: vec!["path".into()],
        };
        let env = sb.environment(&[("CI", "1"), ("cargo_home", "ignored")]);
        let get = |k: &str| env.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        assert_eq!(get("CI"), Some("1".into()));
        assert_eq!(get("CARGO_HOME"), Some(home.path().join("cargo").into_os_string()));
        assert_eq!(get("PATH"), None);
        assert_eq!(env.iter().filter(|(n, _)| n.eq_ignore_ascii_case("cargo_home")).count(), 1);
        let keys: Vec<String> = env.iter().map(|(k, _)| k.to_string_lossy().to_uppercase()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "the environment block must be sorted");
    }

    #[cfg(windows)]
    mod windows {
        use super::super::*;
        use crate::verify::{VerifyKind, run_command};
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        fn run(cmd: &str, cwd: &Path, sb: &Sandbox) -> crate::verify::CommandResult {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let c = VerifyCommand::new(VerifyKind::Build, cmd);
            rt.block_on(run_command(
                &c,
                cwd,
                Duration::from_secs(30),
                &CancellationToken::new(),
                &mut |_| {},
                Some(sb),
            ))
        }

        #[test]
        fn sandboxed_commands_run_at_low_integrity_and_only_write_the_staging_copy() {
            let base = tempfile::tempdir().unwrap();
            let staging = base.path().join("staging");
            let outside = base.path().join("project");
            std::fs::create_dir_all(staging.join("sub")).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(staging.join("sub\\old.txt"), "old").unwrap();
            std::fs::write(outside.join("keep.txt"), "keep").unwrap();
            let sb = Sandbox::prepare_in(&base.path().join("home"), &staging).unwrap();
            assert!(is_low(&staging));

            // System32's whoami: an MSYS one (Git for Windows) on PATH can't start at Low integrity.
            let r = run("%SystemRoot%\\System32\\whoami.exe /groups", &staging, &sb);
            assert!(r.passed, "{}", r.output);
            assert!(r.output.contains("S-1-16-4096"), "not Low integrity:\n{}", r.output);

            let r = run("echo made> new.txt && echo changed> sub\\old.txt && echo %TEMP%", &staging, &sb);
            assert!(r.passed, "{}", r.output);
            assert_eq!(std::fs::read_to_string(staging.join("new.txt")).unwrap().trim(), "made");
            assert_eq!(std::fs::read_to_string(staging.join("sub\\old.txt")).unwrap().trim(), "changed");
            assert!(r.output.contains(&*base.path().join("home\\tmp").to_string_lossy()), "{}", r.output);

            let target = outside.join("bad.txt");
            let r = run(&format!("echo x> \"{}\"", target.display()), &staging, &sb);
            assert!(!r.passed && !target.exists(), "wrote outside the sandbox: {}", r.output);
            let r = run(&format!("del /q \"{}\"", outside.join("keep.txt").display()), &staging, &sb);
            assert!(outside.join("keep.txt").exists(), "deleted outside the sandbox: {}", r.output);
        }

        #[test]
        fn labelling_does_not_follow_junctions_out_of_the_staging_copy() {
            let base = tempfile::tempdir().unwrap();
            let staging = base.path().join("staging");
            let deps = base.path().join("project-node_modules");
            std::fs::create_dir_all(&staging).unwrap();
            std::fs::create_dir_all(deps.join("pkg")).unwrap();
            let link = staging.join("node_modules");
            let mut mk = std::process::Command::new("cmd");
            mk.arg("/C").arg("mklink").arg("/J").arg(&link).arg(&deps);
            assert!(mk.output().unwrap().status.success());
            let sb = Sandbox::prepare_in(&base.path().join("home"), &staging).unwrap();
            let r = run("echo x> node_modules\\pkg\\written.txt", &staging, &sb);
            assert!(
                !r.passed && !deps.join("pkg\\written.txt").exists(),
                "wrote through the junction: {}",
                r.output
            );
        }

        #[test]
        fn sandboxed_cargo_builds_and_tests_a_crate() {
            if which::which("cargo").is_err() {
                return;
            }
            let base = tempfile::tempdir().unwrap();
            let staging = base.path().join("staging");
            std::fs::create_dir_all(staging.join("src")).unwrap();
            std::fs::write(
                staging.join("Cargo.toml"),
                "[package]\nname = \"sbx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            std::fs::write(
                staging.join("src\\lib.rs"),
                "pub fn two() -> u32 { 2 }\n#[test]\nfn t() { assert_eq!(two(), 2); }\n",
            )
            .unwrap();
            let sb = Sandbox::prepare_in(&base.path().join("home"), &staging).unwrap();
            let r = run("cargo test -q", &staging, &sb);
            assert!(r.passed, "sandboxed cargo test failed:\n{}", r.output);
            assert!(staging.join("target").is_dir());
        }
    }
}
