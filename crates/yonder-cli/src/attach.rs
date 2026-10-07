//! `yonder run` / `yonder attach`: a raw-mode terminal client over the control socket.
//!
//! Detach with Ctrl-] (the session keeps running). The local terminal size drives the
//! PTY while this client is the most recently active one.

use std::io::Write;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use crossterm::terminal;
use tokio::io::AsyncReadExt;
use yonder_host::control::ControlClient;
use yonder_proto::app::{ClientMsg, Event, HostMsg, Request, Response, SessionKind, SessionState};

const DETACH: u8 = 0x1d; // Ctrl-]

struct RawGuard;

impl RawGuard {
    fn enable() -> Result<Self> {
        terminal::enable_raw_mode().map_err(|e| anyhow!("raw mode: {e}"))?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Attach to `session` until it exits or the user detaches. Returns the exit code.
pub async fn attach(socket: &str, session: &str) -> Result<Option<i32>> {
    let mut c = ControlClient::connect(socket).await?;
    let resp = c.request(Request::Attach { session: session.to_string(), since: None }).await?;
    let Response::Attached { session: info, terminal: snap, .. } = resp else {
        bail!("unexpected response to attach");
    };
    if info.kind != SessionKind::Terminal {
        bail!("{} is a chat session; open it from the web app", info.id);
    }
    let snap = snap.ok_or_else(|| anyhow!("no terminal snapshot"))?;
    let mut offset = snap.offset;
    let mut stdout = std::io::stdout();
    let _raw = RawGuard::enable()?;
    if snap.reset {
        stdout.write_all(b"\x1b[0m\x1b[2J\x1b[H")?;
    }
    stdout.write_all(&B64.decode(&snap.data).unwrap_or_default())?;
    stdout.flush()?;
    let pending_events = std::mem::take(&mut c.events);
    let (mut w, r) = c.into_split();
    let mut rx = r.spawn_pump();

    // Size: take over the PTY with our terminal size.
    let (cols, rows) = terminal::size().unwrap_or((info.cols, info.rows));
    w.send(&ClientMsg::Resize { session: session.to_string(), cols, rows }).await?;
    let mut last_size = (cols, rows);

    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 8192];
    let mut size_tick = tokio::time::interval(Duration::from_millis(250));
    let mut exit_code = None;
    let mut exited = info.state != SessionState::Running && info.state != SessionState::Starting;

    let handle = |ev: Event, offset: &mut u64, out: &mut std::io::Stdout| -> Result<Option<Option<i32>>> {
        match ev {
            Event::PtyOutput { session: s, offset: off, data } if s == session => {
                let bytes = B64.decode(data).unwrap_or_default();
                let end = off + bytes.len() as u64;
                if end > *offset {
                    let skip = offset.saturating_sub(off) as usize;
                    out.write_all(&bytes[skip.min(bytes.len())..])?;
                    out.flush()?;
                    *offset = end;
                }
            }
            Event::PtySnapshot { session: s, snapshot } if s == session => {
                out.write_all(b"\x1b[0m\x1b[2J\x1b[H")?;
                out.write_all(&B64.decode(snapshot.data).unwrap_or_default())?;
                out.flush()?;
                *offset = snapshot.offset;
            }
            Event::SessionUpdated { session: s } if s.id == session => {
                if matches!(s.state, SessionState::Exited | SessionState::Failed) {
                    return Ok(Some(s.exit_code));
                }
            }
            Event::SessionRemoved { session: s } if s == session => return Ok(Some(None)),
            _ => {}
        }
        Ok(None)
    };

    for ev in pending_events {
        if let Some(code) = handle(ev, &mut offset, &mut stdout)? {
            exit_code = code;
            exited = true;
        }
    }
    if exited {
        drop(_raw);
        return Ok(exit_code);
    }

    loop {
        tokio::select! {
            n = stdin.read(&mut buf) => {
                let n = n?;
                if n == 0 {
                    break;
                }
                let data = &buf[..n];
                if let Some(i) = data.iter().position(|b| *b == DETACH) {
                    if i > 0 {
                        w.send(&ClientMsg::Input { session: session.to_string(), data: B64.encode(&data[..i]) }).await?;
                    }
                    drop(_raw);
                    eprintln!("\r\n[detached from {session}; `yonder attach {session}` to return]");
                    return Ok(None);
                }
                w.send(&ClientMsg::Input { session: session.to_string(), data: B64.encode(data) }).await?;
            }
            m = rx.recv() => {
                let Some(m) = m else {
                    drop(_raw);
                    bail!("daemon connection closed");
                };
                if let HostMsg::Event { event } = m {
                    if let Some(code) = handle(event, &mut offset, &mut stdout)? {
                        exit_code = code;
                        break;
                    }
                }
            }
            _ = size_tick.tick() => {
                if let Ok(sz) = terminal::size() {
                    if sz != last_size {
                        last_size = sz;
                        w.send(&ClientMsg::Resize { session: session.to_string(), cols: sz.0, rows: sz.1 }).await?;
                    }
                }
            }
        }
    }
    drop(_raw);
    Ok(exit_code)
}
