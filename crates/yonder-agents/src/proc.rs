//! Spawning agent processes with JSONL stdio.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

pub struct AgentProc {
    pub child: Child,
    pub stdin: ChildStdin,
    /// Parsed JSON lines from stdout. Closed when stdout ends.
    pub lines: mpsc::Receiver<serde_json::Value>,
    /// Last lines of stderr (for error reporting).
    pub stderr_tail: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

/// Turn a user-level argv (`["codex", "app-server"]`) into the argv to execute.
///
/// - Bare program names are resolved on PATH plus common per-user install dirs.
/// - Windows npm shims (`.cmd` / `.ps1`) are unwrapped to `node <script>` (or the target
///   `.exe`) so arguments pass verbatim and killing the process kills the agent. Unknown
///   shims fall back to `cmd.exe /d /s /c` or PowerShell.
/// - Unix with `login_shell`: `$SHELL -lc 'exec "$@"' yonder argv...`.
pub fn resolve_argv(argv: &[String], login_shell: bool) -> Result<Vec<String>> {
    anyhow::ensure!(!argv.is_empty(), "empty argv");
    #[cfg(unix)]
    {
        if login_shell {
            let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
            let mut v = vec![shell, "-lc".into(), "exec \"$@\"".into(), "yonder".into()];
            v.extend(argv.iter().cloned());
            return Ok(v);
        }
        let prog = crate::detect::resolve_program(&argv[0]).unwrap_or_else(|| PathBuf::from(&argv[0]));
        let mut v = vec![prog.to_string_lossy().into_owned()];
        v.extend(argv[1..].iter().cloned());
        Ok(v)
    }
    #[cfg(windows)]
    {
        let _ = login_shell;
        let prog = crate::detect::resolve_program(&argv[0]).unwrap_or_else(|| PathBuf::from(&argv[0]));
        let ext = prog.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let mut v: Vec<String> = match ext.as_str() {
            "cmd" | "bat" | "ps1" => {
                let unwrapped = std::fs::read_to_string(&prog).ok().and_then(|c| parse_npm_shim(&prog, &c));
                match unwrapped {
                    Some(v) => v,
                    None if ext == "ps1" => vec![
                        "powershell.exe".into(),
                        "-NoLogo".into(),
                        "-NoProfile".into(),
                        "-ExecutionPolicy".into(),
                        "Bypass".into(),
                        "-File".into(),
                        prog.to_string_lossy().into_owned(),
                    ],
                    None => vec!["cmd.exe".into(), "/d".into(), "/s".into(), "/c".into(), prog.to_string_lossy().into_owned()],
                }
            }
            _ => vec![prog.to_string_lossy().into_owned()],
        };
        v.extend(argv[1..].iter().cloned());
        Ok(v)
    }
}

/// Extract the real target from an npm `cmd-shim` (`.cmd` or `.ps1`).
/// Returns `[node, script]` for JS targets or `[exe]` for native targets.
pub fn parse_npm_shim(shim: &Path, content: &str) -> Option<Vec<String>> {
    let dir = shim.parent()?;
    for marker in ["%dp0%\\", "%~dp0\\", "$basedir/", "$basedir\\"] {
        let mut rest = content;
        while let Some(i) = rest.find(marker) {
            let after = &rest[i + marker.len()..];
            let end = after.find(['"', '\'']).unwrap_or(after.len());
            let target = after[..end].trim();
            rest = &after[end..];
            let lower = target.to_ascii_lowercase();
            if lower.is_empty() || lower == "node" || lower == "node.exe" || lower.starts_with("node$") {
                continue;
            }
            let rel: PathBuf = target.split(['/', '\\']).collect();
            let full = dir.join(rel);
            if lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs") {
                let local_node = dir.join("node.exe");
                let node = if local_node.is_file() {
                    local_node
                } else {
                    crate::detect::resolve_program("node").unwrap_or_else(|| PathBuf::from("node"))
                };
                return Some(vec![node.to_string_lossy().into_owned(), full.to_string_lossy().into_owned()]);
            }
            if lower.ends_with(".exe") {
                return Some(vec![full.to_string_lossy().into_owned()]);
            }
        }
    }
    None
}

/// Build a tokio command for an agent process (stdio configured by the caller).
pub fn build_command(argv: &[String], cwd: &Path, env: &BTreeMap<String, String>, login_shell: bool) -> Result<Command> {
    let argv = resolve_argv(argv, login_shell)?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd).kill_on_drop(true);
    if !env.contains_key("PATH") {
        if let Some(p) = crate::detect::user_path() {
            cmd.env("PATH", p);
        }
    }
    cmd.envs(env);
    #[cfg(unix)]
    {
        // Own process group so kill_tree reaches the agent's children too.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd)
}

/// Blocking variant used for quick probes (`--version`).
pub fn build_std_command(argv: &[String], cwd: &Path) -> Result<std::process::Command> {
    let argv = resolve_argv(argv, false)?;
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd);
    if let Some(p) = crate::detect::user_path() {
        cmd.env("PATH", p);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd)
}

pub fn spawn_jsonl(mut cmd: Command) -> Result<AgentProc> {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn agent")?;
    let stdin = child.stdin.take().context("agent stdin")?;
    let stdout = child.stdout.take().context("agent stdout")?;
    let stderr = child.stderr.take().context("agent stderr")?;
    let (tx, rx) = mpsc::channel(1024);
    tokio::spawn(async move {
        // Split on '\n' only (JSON strings may contain U+2028/U+2029).
        let mut r = BufReader::with_capacity(256 * 1024, stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
                    let line = line.strip_suffix(b"\r").unwrap_or(line);
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_slice::<serde_json::Value>(line) {
                        Ok(v) => {
                            if tx.send(v).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => tracing::debug!("agent non-JSON stdout: {}", String::from_utf8_lossy(line)),
                    }
                }
            }
        }
    });
    let tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let tail = tail.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let l = String::from_utf8_lossy(&buf).trim_end().to_string();
                        if l.is_empty() {
                            continue;
                        }
                        let mut t = tail.lock().unwrap();
                        t.push(crate::common::strip_ansi(&l));
                        if t.len() > 40 {
                            t.remove(0);
                        }
                    }
                }
            }
        });
    }
    Ok(AgentProc { child, stdin, lines: rx, stderr_tail: tail })
}

pub async fn write_json(stdin: &mut ChildStdin, v: &serde_json::Value) -> Result<()> {
    let mut s = serde_json::to_vec(v)?;
    s.push(b'\n');
    stdin.write_all(&s).await?;
    stdin.flush().await?;
    Ok(())
}

pub fn stderr_summary(tail: &std::sync::Mutex<Vec<String>>) -> Option<String> {
    let t = tail.lock().unwrap();
    let n = t.len();
    let s = &t[n.saturating_sub(8)..];
    if s.is_empty() {
        None
    } else {
        Some(s.join("\n"))
    }
}

/// Kill a process and all of its children.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        // Agents run in their own process group (see build_command).
        libc::kill(-(pid as i32), libc::SIGTERM);
        libc::kill(pid as i32, libc::SIGTERM);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            libc::kill(-(pid as i32), libc::SIGKILL);
            libc::kill(pid as i32, libc::SIGKILL);
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_cmd_shim() {
        let cmd = r#"@ECHO off
GOTO start
:find_dp0
SET dp0=%~dp0
EXIT /b
:start
SETLOCAL
CALL :find_dp0

IF EXIST "%dp0%\node.exe" (
  SET "_prog=%dp0%\node.exe"
) ELSE (
  SET "_prog=node"
  SET PATHEXT=%PATHEXT:;.JS;=;%
)

endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\node_modules\@openai\codex\bin\codex.js" %*
"#;
        let v = parse_npm_shim(Path::new("/npm/codex.cmd"), cmd).unwrap();
        assert!(v[1].ends_with("codex.js"), "{v:?}");
        assert!(v[1].contains("node_modules"));

        let ps1 = r#"#!/usr/bin/env pwsh
$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent
$exe=""
if (Test-Path "$basedir/node$exe") {
  if ($MyInvocation.ExpectingInput) {
    $input | & "$basedir/node$exe"  "$basedir/node_modules/@anthropic-ai/claude-code/cli.js" $args
  } else {
    & "$basedir/node$exe"  "$basedir/node_modules/@anthropic-ai/claude-code/cli.js" $args
  }
  $ret=$LASTEXITCODE
}
"#;
        let v = parse_npm_shim(Path::new("/npm/claude.ps1"), ps1).unwrap();
        assert!(v[1].ends_with("cli.js"), "{v:?}");

        let native = "@\"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe\"   %*\r\n";
        let v = parse_npm_shim(Path::new("/npm/claude.cmd"), native).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].ends_with("claude.exe"));
        assert!(parse_npm_shim(Path::new("/x/y.cmd"), "echo hi").is_none());
    }
}
