//! Locate agent executables and report availability.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use yonder_proto::app::{AgentAvailability, AgentKind};

pub fn program_for(agent: AgentKind) -> Option<&'static str> {
    match agent {
        AgentKind::Codex => Some("codex"),
        AgentKind::Claude => Some("claude"),
        AgentKind::Pi => Some("pi"),
        _ => None,
    }
}

/// PATH as seen by the user's login shell (Unix), captured once. Service managers
/// (launchd, systemd --user) start the daemon with a minimal PATH that misses nvm, bun,
/// Homebrew and ~/.local/bin, where agents usually live.
pub fn login_shell_path() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            #[cfg(unix)]
            {
                let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
                let mut child = std::process::Command::new(shell)
                    .args(["-lic", "printf '\\n__YONDER_PATH__%s\\n' \"$PATH\""])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()?;
                let deadline = std::time::Instant::now() + Duration::from_secs(8);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(30)),
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return None;
                        }
                    }
                }
                let mut out = String::new();
                use std::io::Read;
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                out.lines().find_map(|l| l.strip_prefix("__YONDER_PATH__")).map(str::to_string).filter(|s| !s.is_empty())
            }
            #[cfg(windows)]
            {
                None
            }
        })
        .clone()
}

fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for d in [".local/bin", ".npm-global/bin", ".bun/bin", ".cargo/bin", "bin", ".volta/bin", ".deno/bin"] {
            dirs.push(home.join(d));
        }
        #[cfg(windows)]
        {
            dirs.push(home.join("AppData").join("Roaming").join("npm"));
            dirs.push(home.join("AppData").join("Local").join("Programs").join("nodejs"));
            dirs.push(home.join(".local").join("bin"));
        }
    }
    #[cfg(unix)]
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        dirs.push(PathBuf::from(d));
    }
    #[cfg(windows)]
    {
        if let Some(n) = std::env::var_os("NVM_SYMLINK") {
            dirs.push(PathBuf::from(n));
        }
        if let Some(a) = std::env::var_os("APPDATA") {
            dirs.push(PathBuf::from(a).join("npm"));
        }
        dirs.push(PathBuf::from(r"C:\nvm4w\nodejs"));
        dirs.push(PathBuf::from(r"C:\Program Files\nodejs"));
    }
    dirs
}

/// Directories to search: login-shell PATH, current PATH, well-known install dirs.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !p.as_os_str().is_empty() && !dirs.contains(&p) {
            dirs.push(p);
        }
    };
    if let Some(p) = login_shell_path() {
        for d in std::env::split_paths(&p) {
            push(d);
        }
    }
    if let Some(p) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&p) {
            push(d);
        }
    }
    for d in extra_dirs() {
        push(d);
    }
    dirs
}

/// PATH for child processes: [`search_dirs`] joined.
pub fn user_path() -> Option<String> {
    std::env::join_paths(search_dirs()).ok().map(|s| s.to_string_lossy().into_owned())
}

/// Find a program on the user's PATH plus common per-user install dirs.
pub fn resolve_program(name: &str) -> Option<PathBuf> {
    let p = std::path::Path::new(name);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    #[cfg(windows)]
    let exts: &[&str] = &["exe", "cmd", "bat", "ps1", "com"];
    #[cfg(unix)]
    let exts: &[&str] = &[""];
    for d in search_dirs() {
        if p.extension().is_some() || exts == [""] {
            let c = d.join(name);
            if c.is_file() && is_executable(&c) {
                return Some(c);
            }
            if exts == [""] {
                continue;
            }
        }
        for ext in exts {
            let c = d.join(format!("{name}.{ext}"));
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(_p: &std::path::Path) -> bool {
    true
}

/// First line of `<program> --version` (8 s timeout).
fn version_of(path: &std::path::Path) -> Option<String> {
    let mut cmd = crate::proc::build_std_command(&[path.to_string_lossy().into_owned(), "--version".into()], &std::env::temp_dir()).ok()?;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    use std::io::Read;
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let line = out.lines().map(str::trim).find(|l| !l.is_empty())?.to_string();
    Some(line)
}

/// Detect installed agents. Blocking (runs `--version`); call from `spawn_blocking`.
pub fn detect_agents() -> Vec<AgentAvailability> {
    let agents = [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi];
    let handles: Vec<_> = agents
        .iter()
        .map(|&agent| {
            std::thread::spawn(move || {
                let path = program_for(agent).and_then(resolve_program);
                let version = path.as_deref().and_then(version_of);
                AgentAvailability {
                    agent,
                    available: path.is_some(),
                    version,
                    path: path.map(|p| p.to_string_lossy().into_owned()),
                    chat: true,
                    models: default_models(agent),
                    default_model: None,
                    default_approval: None,
                }
            })
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

fn default_models(agent: AgentKind) -> Vec<String> {
    match agent {
        AgentKind::Claude => vec!["opus".into(), "sonnet".into(), "haiku".into()],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_sh() {
        #[cfg(unix)]
        assert!(resolve_program("sh").is_some());
        assert!(resolve_program("definitely-not-a-program-yonder").is_none());
        assert!(user_path().is_some());
    }
}
