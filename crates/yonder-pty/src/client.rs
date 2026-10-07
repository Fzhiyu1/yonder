//! Async client used by the daemon to talk to a supervisor.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use interprocess::local_socket::tokio::{prelude::*, RecvHalf, SendHalf};
use tokio::sync::Mutex;

use crate::ipc::{read_frame, write_frame, SupEvent, SupRequest, SupervisorInfo};

pub struct SupervisorClient {
    tx: Mutex<SendHalf>,
    rx: Mutex<RecvHalf>,
}

use crate::local::connect as connect_stream;

impl SupervisorClient {
    pub async fn connect(dir: &Path, id: &str) -> Result<Self> {
        let stream = connect_stream(dir, id).await.with_context(|| format!("connect supervisor {id}"))?;
        let (rx, tx) = stream.split();
        Ok(Self { tx: Mutex::new(tx), rx: Mutex::new(rx) })
    }

    /// Retry connecting for up to `timeout` (useful right after spawning a supervisor).
    pub async fn connect_retry(dir: &Path, id: &str, timeout: Duration) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match Self::connect(dir, id).await {
                Ok(c) => return Ok(c),
                Err(e) if tokio::time::Instant::now() >= deadline => return Err(e),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    pub async fn send(&self, req: &SupRequest) -> Result<()> {
        let mut tx = self.tx.lock().await;
        write_frame(&mut *tx, req).await.context("send to supervisor")
    }

    /// Next event, or None when the supervisor closed the connection.
    pub async fn recv(&self) -> Result<Option<SupEvent>> {
        let mut rx = self.rx.lock().await;
        Ok(read_frame(&mut *rx).await?)
    }

    pub async fn info(&self) -> Result<SupervisorInfo> {
        self.send(&SupRequest::Hello).await?;
        loop {
            match self.recv().await? {
                Some(SupEvent::Info { info }) => return Ok(info),
                Some(_) => continue,
                None => anyhow::bail!("supervisor closed"),
            }
        }
    }

    pub async fn input(&self, bytes: &[u8]) -> Result<()> {
        self.send(&SupRequest::Input { data: B64.encode(bytes) }).await
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.send(&SupRequest::Resize { cols, rows }).await
    }

    pub async fn kill(&self) -> Result<()> {
        self.send(&SupRequest::Kill).await
    }

    pub async fn subscribe(&self, from_offset: Option<u64>) -> Result<()> {
        self.send(&SupRequest::Subscribe { from_offset }).await
    }

    /// Split into independent halves for a reader task and a writer.
    pub fn into_split(self) -> (SupervisorWriter, SupervisorReader) {
        (SupervisorWriter { tx: self.tx }, SupervisorReader { rx: self.rx.into_inner() })
    }
}

pub struct SupervisorWriter {
    tx: Mutex<SendHalf>,
}

impl SupervisorWriter {
    pub async fn send(&self, req: &SupRequest) -> Result<()> {
        let mut tx = self.tx.lock().await;
        write_frame(&mut *tx, req).await.context("send to supervisor")
    }
}

pub struct SupervisorReader {
    rx: RecvHalf,
}

impl SupervisorReader {
    pub async fn recv(&mut self) -> Result<Option<SupEvent>> {
        Ok(read_frame(&mut self.rx).await?)
    }
}

/// True if a supervisor answers on the session's socket.
pub async fn supervisor_alive(dir: &Path, id: &str) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), async {
            let c = SupervisorClient::connect(dir, id).await.ok()?;
            c.info().await.ok()
        })
        .await,
        Ok(Some(_))
    )
}
