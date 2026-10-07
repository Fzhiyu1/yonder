//! Cross-platform PTY sessions owned by detached supervisor processes.
//!
//! A session is a directory (`session_dir`) plus one supervisor process that owns the
//! PTY and the child. The supervisor survives the process that spawned it (the yonder
//! daemon), so agents keep running across daemon restarts and upgrades.
//!
//! - [`spawn_supervisor`] starts a detached supervisor (`supervisor_main` in the child).
//! - [`SupervisorClient`] talks to a running supervisor over a local socket
//!   (Unix domain socket `<dir>/sup.sock`, Windows named pipe `yonder-sup-<id>`).
//! - Output is appended to `<dir>/pty.log`; offsets are global byte offsets.
//! - `<dir>/exit.json` is written when the child exits.
//!
//! See README.md for the IPC protocol.

pub mod client;
pub mod ipc;
pub mod launch;
pub mod local;
pub mod log;
pub mod supervisor;

pub use client::{supervisor_alive, SupervisorClient};
pub use ipc::{ExitInfo, SupEvent, SupRequest, SupervisorInfo};
pub use launch::{resolve_command, spawn_supervisor, SupervisorArgs};
pub use log::{read_exit, PtyLog};
pub use supervisor::supervisor_main;

use std::path::{Path, PathBuf};

pub const SOCKET_FILE: &str = "sup.sock";
pub const LOG_FILE: &str = "pty.log";
pub const LOG_BASE_FILE: &str = "pty.base";
pub const EXIT_FILE: &str = "exit.json";
pub const SUP_LOG_FILE: &str = "supervisor.log";

/// Local socket name for a session (path on Unix, pipe name on Windows).
pub fn socket_name(dir: &Path, id: &str) -> String {
    if cfg!(windows) {
        let _ = dir;
        format!("yonder-sup-{id}")
    } else {
        let _ = id;
        dir.join(SOCKET_FILE).to_string_lossy().into_owned()
    }
}

/// Session directories under `root` (each containing a `meta.json` managed by the caller).
pub fn list_session_dirs(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
