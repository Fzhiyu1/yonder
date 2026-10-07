//! Local control socket: newline-delimited app JSON, no Noise.
//!
//! Trusted by OS permissions: a Unix socket (0600) inside the private data dir, or a
//! named pipe whose DACL only admits the owner and SYSTEM. Used by the `yonder` CLI
//! (`pair`, `status`, `ls`, `run`, `attach`, ...).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use interprocess::local_socket::tokio::{prelude::*, RecvHalf, SendHalf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use yonder_proto::app::{ApiError, ClientMsg, Event, HostMsg, Request, Response};

use crate::daemon::{Caller, Daemon};
use crate::sessions::ClientHandle;

/// Max bytes in one control line (file chunks are base64).
const MAX_LINE: usize = 48 * 1024 * 1024;

pub async fn serve(d: Arc<Daemon>) -> Result<()> {
    let name = d.paths.control_socket();
    let listener = yonder_pty::local::bind_named(&name).with_context(|| format!("control socket {name}"))?;
    tracing::info!("control socket: {name}");
    loop {
        tokio::select! {
            conn = listener.accept() => match conn {
                Ok(conn) => {
                    let d = d.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_conn(d, conn).await {
                            tracing::debug!("control client: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("control accept: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            },
            _ = d.shutdown.notified() => break,
        }
    }
    #[cfg(unix)]
    let _ = std::fs::remove_file(&name);
    Ok(())
}

async fn serve_conn(d: Arc<Daemon>, conn: interprocess::local_socket::tokio::Stream) -> Result<()> {
    let (rx, mut tx) = conn.split();
    let (client, mut out) = ClientHandle::new(d.next_client_id());
    d.sessions.add_client(client.clone());
    let writer = tokio::spawn(async move {
        while let Some(m) = out.recv().await {
            let Ok(mut line) = serde_json::to_vec(&m) else { continue };
            line.push(b'\n');
            if tx.write_all(&line).await.is_err() {
                break;
            }
        }
    });
    let caller = Caller::Local;
    let mut reader = BufReader::with_capacity(256 * 1024, rx);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            break;
        }
        if buf.len() > MAX_LINE {
            break;
        }
        let msg: ClientMsg = match serde_json::from_slice(&buf) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("control: bad message: {e}");
                continue;
            }
        };
        crate::relay_link::dispatch(&d, &caller, &client, msg).await;
    }
    d.sessions.remove_client(client.id);
    drop(client);
    writer.abort();
    Ok(())
}

/// Client side of the control socket (the `yonder` CLI).
pub struct ControlClient {
    tx: SendHalf,
    rx: BufReader<RecvHalf>,
    next_id: u64,
    /// Events received while waiting for a response.
    pub events: Vec<Event>,
}

impl ControlClient {
    pub async fn connect(socket: &str) -> Result<Self> {
        let s = yonder_pty::local::connect_named(socket).await.map_err(|e| anyhow!("daemon not running ({socket}): {e}"))?;
        let (rx, tx) = s.split();
        Ok(Self { tx, rx: BufReader::with_capacity(256 * 1024, rx), next_id: 1, events: Vec::new() })
    }

    pub async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        self.tx.write_all(&line).await?;
        Ok(())
    }

    /// Next message from the daemon (None on EOF).
    pub async fn recv(&mut self) -> Result<Option<HostMsg>> {
        let mut buf = Vec::new();
        let n = self.rx.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&buf).context("daemon message")?))
    }

    /// Send a request and wait for its response (events that arrive meanwhile are kept).
    pub async fn request(&mut self, req: Request) -> Result<Response> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&ClientMsg::Req { id, req }).await?;
        loop {
            match self.recv().await? {
                Some(HostMsg::Res { id: rid, ok, data, error }) if rid == id => {
                    if ok {
                        return data.ok_or_else(|| anyhow!("empty response"));
                    }
                    let e = error.unwrap_or_else(|| ApiError::internal("unknown error"));
                    bail!("{}: {}", e.code, e.message);
                }
                Some(HostMsg::Event { event }) => self.events.push(event),
                Some(_) => {}
                None => bail!("daemon closed the connection"),
            }
        }
    }

    /// Split for concurrent reading (events) and writing (input).
    pub fn into_split(self) -> (ControlWriter, ControlReader) {
        (ControlWriter { tx: self.tx, next_id: self.next_id }, ControlReader { rx: self.rx })
    }
}

pub struct ControlWriter {
    tx: SendHalf,
    next_id: u64,
}

impl ControlWriter {
    pub async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        self.tx.write_all(&line).await?;
        Ok(())
    }

    pub async fn request_nowait(&mut self, req: Request) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&ClientMsg::Req { id, req }).await?;
        Ok(id)
    }
}

pub struct ControlReader {
    rx: BufReader<RecvHalf>,
}

impl ControlReader {
    pub async fn recv(&mut self) -> Result<Option<HostMsg>> {
        let mut buf = Vec::new();
        let n = self.rx.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&buf).context("daemon message")?))
    }

    /// Forward decoded messages into a channel (for select loops).
    pub fn spawn_pump(mut self) -> mpsc::Receiver<HostMsg> {
        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(async move {
            while let Ok(Some(m)) = self.recv().await {
                if tx.send(m).await.is_err() {
                    break;
                }
            }
        });
        rx
    }
}
