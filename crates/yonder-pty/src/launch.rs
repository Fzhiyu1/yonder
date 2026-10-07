//! Launching detached supervisors and resolving commands.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};

use crate::SUP_LOG_FILE;

/// Everything a supervisor needs. Passed to the child as one base64(JSON) argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorArgs {
    pub id: String,
    pub dir: PathBuf,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    /// Unix: run through `$SHELL -lc` so the user's profile PATH applies.
    #[serde(default)]
    pub login_shell: bool,
    /// Seconds to keep serving after the child exits (then the supervisor quits).
    #[serde(default = "default_linger")]
    pub linger_secs: u64,
}

fn default_linger() -> u64 {
    600
}

impl SupervisorArgs {
    pub fn encode(&self) -> Result<String> {
        Ok(B64.encode(serde_json::to_vec(self)?))
    }

    pub fn decode(s: &str) -> Result<Self> {
        let bytes = B64.decode(s.trim()).context("supervisor args base64")?;
        serde_json::from_slice(&bytes).context("supervisor args json")
    }
}

/// Spawn `program argv_prefix... <encoded args>` fully detached from the caller.
/// Returns the supervisor pid. The supervisor writes its own log to `<dir>/supervisor.log`.
pub fn spawn_supervisor(program: &Path, argv_prefix: &[OsString], args: &SupervisorArgs) -> Result<u32> {
    let mut argv: Vec<OsString> = argv_prefix.to_vec();
    argv.push(args.encode()?.into());
    spawn_detached(program, &argv, &args.dir, &args.dir.join(SUP_LOG_FILE))
}

/// Spawn `program args...` in `cwd`, detached from the caller's session / console / job, with
/// stdin from null and stdout+stderr appended to `log`. Returns the pid.
/// Unix: `setsid`. Windows: `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW |
/// CREATE_BREAKAWAY_FROM_JOB`, retried without breakaway when the job forbids it.
pub fn spawn_detached(program: &Path, args: &[OsString], cwd: &Path, log: &Path) -> Result<u32> {
    std::fs::create_dir_all(cwd).with_context(|| format!("create {}", cwd.display()))?;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let open_log = || std::fs::OpenOptions::new().create(true).append(true).open(log).context("open log");
    let log_file = open_log()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file));
    detach(&mut cmd);
    match cmd.spawn() {
        Ok(child) => Ok(child.id()),
        Err(e) => {
            #[cfg(windows)]
            {
                // CREATE_BREAKAWAY_FROM_JOB is refused inside jobs that disallow breakaway.
                let log_file = open_log()?;
                let mut cmd = Command::new(program);
                cmd.args(args)
                    .current_dir(cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(log_file.try_clone()?))
                    .stderr(Stdio::from(log_file));
                windows_flags(&mut cmd, false);
                let child = cmd.spawn().with_context(|| format!("spawn {} (after {e})", program.display()))?;
                return Ok(child.id());
            }
            #[allow(unreachable_code)]
            Err(e).with_context(|| format!("spawn {}", program.display()))
        }
    }
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    windows_flags(cmd, true);
}

#[cfg(windows)]
fn windows_flags(cmd: &mut Command, breakaway: bool) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    if breakaway {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    cmd.creation_flags(flags);
}

/// Turn a user-level argv into what the PTY should actually execute.
///
/// - Windows: resolve via PATH/PATHEXT; `.cmd`/`.bat` run through `cmd.exe /d /s /c`,
///   `.ps1` through PowerShell.
/// - Unix with `login_shell`: `$SHELL -lc 'exec "$@"' yonder argv...`.
pub fn resolve_command(argv: &[String], login_shell: bool, env_path: Option<&str>) -> Result<Vec<String>> {
    anyhow::ensure!(!argv.is_empty(), "empty command");
    #[cfg(windows)]
    {
        let _ = login_shell;
        let prog = &argv[0];
        let resolved = resolve_windows_program(prog, env_path).unwrap_or_else(|| PathBuf::from(prog));
        let ext = resolved
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let rest = argv[1..].iter().cloned();
        let out: Vec<String> = match ext.as_str() {
            "cmd" | "bat" => {
                let mut v = vec!["cmd.exe".to_string(), "/d".into(), "/s".into(), "/c".into()];
                v.push(resolved.to_string_lossy().into_owned());
                v.extend(rest);
                v
            }
            "ps1" => {
                let mut v = vec![
                    "powershell.exe".to_string(),
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-File".into(),
                ];
                v.push(resolved.to_string_lossy().into_owned());
                v.extend(rest);
                v
            }
            _ => {
                let mut v = vec![resolved.to_string_lossy().into_owned()];
                v.extend(rest);
                v
            }
        };
        Ok(out)
    }
    #[cfg(unix)]
    {
        let _ = env_path;
        if login_shell {
            let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
            let mut v = vec![shell, "-lc".into(), "exec \"$@\"".into(), "yonder".into()];
            v.extend(argv.iter().cloned());
            Ok(v)
        } else {
            Ok(argv.to_vec())
        }
    }
}

/// Resolve a program name on Windows, preferring real executables and npm shims.
#[cfg(windows)]
pub fn resolve_windows_program(prog: &str, env_path: Option<&str>) -> Option<PathBuf> {
    let p = Path::new(prog);
    if p.is_absolute() && p.exists() {
        return Some(p.to_path_buf());
    }
    let path = env_path.map(OsString::from).or_else(|| std::env::var_os("PATH"))?;
    let exts = ["exe", "cmd", "bat", "ps1", "com"];
    for dir in std::env::split_paths(&path) {
        if p.extension().is_some() {
            let c = dir.join(p);
            if c.is_file() {
                return Some(c);
            }
            continue;
        }
        for ext in exts {
            let c = dir.join(format!("{prog}.{ext}"));
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_roundtrip() {
        let a = SupervisorArgs {
            id: "s1".into(),
            dir: "/tmp/x".into(),
            argv: vec!["sh".into(), "-c".into(), "echo 'hi'".into()],
            cwd: "/tmp".into(),
            env: BTreeMap::new(),
            cols: 80,
            rows: 24,
            login_shell: false,
            linger_secs: 5,
        };
        let b = SupervisorArgs::decode(&a.encode().unwrap()).unwrap();
        assert_eq!(b.argv, a.argv);
        assert_eq!(b.linger_secs, 5);
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_wrap() {
        let v = resolve_command(&["codex".into(), "--x".into()], true, None).unwrap();
        assert_eq!(v[1], "-lc");
        assert_eq!(&v[4..], &["codex".to_string(), "--x".to_string()]);
        let v = resolve_command(&["codex".into()], false, None).unwrap();
        assert_eq!(v, vec!["codex".to_string()]);
    }
}
