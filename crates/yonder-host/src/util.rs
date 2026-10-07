//! Small helpers shared by the daemon modules.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;

/// Random lowercase id (session ids, request correlation).
pub fn new_id(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = vec![0u8; len];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char).collect()
}

/// Random base64url token.
pub fn new_token() -> String {
    let mut bytes = [0u8; 18];
    let _ = getrandom::fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Stable short hex hash (FNV-1a 64), for pipe names.
pub fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:012x}", h & 0xffff_ffff_ffff)
}

pub fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    STANDARD.decode(s.trim()).map_err(|e| anyhow!("invalid base64: {e}"))
}

/// Create a directory (and parents) readable only by the current user.
pub fn ensure_private_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).with_context(|| format!("create {}", p.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", p.display()))?;
    }
    Ok(())
}

/// Atomically write a file readable only by the current user (0600 on Unix).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| anyhow!("no parent dir for {}", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", new_id(6)));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename to {}", path.display()));
    }
    Ok(())
}

/// Atomically write a regular file (default permissions).
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| anyhow!("no parent dir for {}", path.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", new_id(6)));
    std::fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename to {}", path.display()));
    }
    Ok(())
}

/// `macos`, `linux` or `windows` (other Unixes report their own name).
pub fn os_name() -> &'static str {
    std::env::consts::OS
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// First `n` characters of `s` (with an ellipsis when cut), single line.
pub fn one_line(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(n).collect();
    if flat.chars().count() > n {
        out.push('…');
    }
    out
}

/// The user's login shell (Unix), from `$SHELL` or the password database.
#[cfg(unix)]
pub fn user_shell() -> String {
    if let Some(s) = std::env::var("SHELL").ok().filter(|s| !s.is_empty()) {
        return s;
    }
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            if let Ok(s) = std::ffi::CStr::from_ptr((*pw).pw_shell).to_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    "/bin/sh".into()
}

/// argv for an interactive shell session.
pub fn default_shell_argv() -> Vec<String> {
    #[cfg(unix)]
    {
        vec![user_shell(), "-l".into()]
    }
    #[cfg(windows)]
    {
        if let Some(p) = yonder_agents::resolve_program("pwsh") {
            return vec![p.to_string_lossy().into_owned(), "-NoLogo".into()];
        }
        vec!["powershell.exe".into(), "-NoLogo".into()]
    }
}

/// Display name of the shell used for `shell` sessions.
pub fn shell_display() -> String {
    default_shell_argv().first().cloned().unwrap_or_default()
}

pub fn now_ms() -> u64 {
    yonder_proto::app::now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_text() {
        let a = new_id(10);
        assert_eq!(a.len(), 10);
        assert_ne!(a, new_id(10));
        assert_eq!(short_hash("abc"), short_hash("abc"));
        assert_ne!(short_hash("abc"), short_hash("abd"));
        assert_eq!(one_line("a\n  b   c", 10), "a b c");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(unb64(&b64(b"hi")).unwrap(), b"hi");
    }
}
