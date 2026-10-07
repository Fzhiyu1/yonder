//! Supervisor IPC: length-prefixed JSON frames (`u32` big-endian length, then JSON).
//! Binary payloads are standard base64 strings.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_IPC_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SupRequest {
    /// Ask for [`SupEvent::Info`].
    Hello,
    Input { data: String },
    Resize { cols: u16, rows: u16 },
    /// Kill the child process tree.
    Kill,
    /// Ask for a rendered screen snapshot.
    Snapshot,
    /// Stream output from `from_offset` (replayed from the log when available; otherwise a
    /// snapshot is sent first), then live output.
    Subscribe { from_offset: Option<u64> },
    /// Stop the supervisor after the child has exited (or kill it first).
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<String>,
    pub ended_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorInfo {
    pub id: String,
    pub supervisor_pid: u32,
    pub child_pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub started_at: u64,
    /// Offset right after the last byte of output so far.
    pub offset: u64,
    pub exited: Option<ExitInfo>,
    pub title: Option<String>,
    /// Last non-empty line of the current screen (for session lists).
    #[serde(default)]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SupEvent {
    Info { info: SupervisorInfo },
    Output { offset: u64, data: String },
    /// Bytes that repaint the current screen on a cleared terminal of `cols`x`rows`.
    Snapshot { data: String, offset: u64, cols: u16, rows: u16 },
    Resized { cols: u16, rows: u16 },
    Exited { exit: ExitInfo },
    /// The terminal title set by the child (OSC 0/2).
    Title { title: String },
    Error { message: String },
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    let mut buf = Vec::with_capacity(body.len() + 4);
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(r: &mut R) -> std::io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_IPC_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "ipc frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
