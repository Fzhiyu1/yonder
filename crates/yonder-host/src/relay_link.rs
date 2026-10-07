//! The host's connection to the relay and the encrypted links to devices.
//!
//! One WebSocket to the relay (reconnecting with backoff). Every `incoming` link gets its
//! own task: Noise IK responder handshake, authorization, then app messages in both
//! directions. The relay never sees plaintext; frames from unknown or misbehaving links
//! are dropped without blocking the connection.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use yonder_proto::app::{ClientMsg as AppClientMsg, HostMsg};
use yonder_proto::keys::PublicKey;
use yonder_proto::noise::{HostHello, Responder};
use yonder_proto::relay::{self, ClientMsg, Role, ServerMsg};
use yonder_proto::PROTOCOL_VERSION;

use crate::daemon::{Caller, Daemon};
use crate::sessions::ClientHandle;

/// Frames queued per link before the link is considered misbehaving.
const LINK_INBOX: usize = 512;
/// Time a device has to send its handshake after the link opened.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// No message from the relay for this long = dead connection.
const RELAY_SILENCE: Duration = Duration::from_secs(80);

enum Out {
    Frame(u32, Vec<u8>),
    Close(u32),
}

/// Run forever: connect, serve, reconnect.
pub async fn run(d: Arc<Daemon>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let cfg = d.config();
        let started = std::time::Instant::now();
        let r = tokio::select! {
            r = connect_and_serve(&d, &cfg.relay_url, cfg.relay_proxy.as_deref()) => r,
            _ = d.shutdown.notified() => return,
        };
        {
            let mut st = d.relay_state.lock().unwrap();
            st.connected = false;
            st.error = r.as_ref().err().map(|e| format!("{e:#}"));
        }
        let replaced = matches!(&r, Err(e) if e.to_string().contains("replaced"));
        match &r {
            Ok(()) => tracing::info!("relay connection closed"),
            Err(e) => tracing::warn!("relay: {e:#}"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        let mut wait = backoff;
        if replaced {
            // Another daemon with the same key took over; do not fight over the slot.
            wait = Duration::from_secs(60);
        }
        let jitter = Duration::from_millis(crate::util::now_ms() % 500);
        tokio::select! {
            _ = tokio::time::sleep(wait + jitter) => {}
            _ = d.shutdown.notified() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots_certs());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("tls versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

fn webpki_roots_certs() -> impl Iterator<Item = rustls::pki_types::TrustAnchor<'static>> {
    webpki_roots::TLS_SERVER_ROOTS.iter().cloned()
}

/// TCP connection to `host:port`, optionally through an HTTP CONNECT proxy.
async fn tcp_connect(host: &str, port: u16, proxy: Option<&str>) -> Result<TcpStream> {
    let Some(proxy) = proxy.filter(|p| !p.trim().is_empty()) else {
        let s = tokio::time::timeout(Duration::from_secs(15), TcpStream::connect((host, port)))
            .await
            .map_err(|_| anyhow!("connect {host}:{port}: timed out"))??;
        s.set_nodelay(true)?;
        return Ok(s);
    };
    let p = proxy.trim().trim_start_matches("http://").trim_end_matches('/');
    let mut s = tokio::time::timeout(Duration::from_secs(15), TcpStream::connect(p))
        .await
        .map_err(|_| anyhow!("connect proxy {p}: timed out"))?
        .with_context(|| format!("connect proxy {p}"))?;
    s.set_nodelay(true)?;
    let target = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).await?;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() > 8192 {
            bail!("proxy response too long");
        }
        let n = tokio::time::timeout(Duration::from_secs(15), s.read(&mut byte)).await.map_err(|_| anyhow!("proxy timed out"))??;
        if n == 0 {
            bail!("proxy closed the connection");
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf);
    let status = head.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        bail!("proxy refused CONNECT: {}", head.lines().next().unwrap_or(""));
    }
    Ok(s)
}

fn split_url(url: &str) -> Result<(bool, String, u16)> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        bail!("relay url must start with wss:// or ws://");
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = if let Some(h) = authority.strip_prefix('[') {
        let (h, p) = h.split_once(']').ok_or_else(|| anyhow!("bad IPv6 address"))?;
        (h.to_string(), p.strip_prefix(':').and_then(|p| p.parse().ok()))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()),
            None => (authority.to_string(), None),
        }
    };
    Ok((tls, host, port.unwrap_or(if tls { 443 } else { 80 })))
}

async fn connect_and_serve(d: &Arc<Daemon>, url: &str, proxy: Option<&str>) -> Result<()> {
    let (tls, host, port) = split_url(url)?;
    let tcp = tcp_connect(&host, port, proxy).await?;
    let ws_cfg = WebSocketConfig::default()
        .max_message_size(Some(relay::MAX_FRAME_LEN + 1024))
        .max_frame_size(Some(relay::MAX_FRAME_LEN + 1024));
    let connector = if tls {
        Some(tokio_tungstenite::Connector::Rustls(tls_config()))
    } else {
        Some(tokio_tungstenite::Connector::Plain)
    };
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(20),
        tokio_tungstenite::client_async_tls_with_config(url, tcp, Some(ws_cfg), connector),
    )
    .await
    .map_err(|_| anyhow!("websocket handshake timed out"))?
    .context("websocket handshake")?;
    let (mut sink, mut stream) = ws.split();

    // Challenge / auth / welcome.
    let next_json = |m: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>| -> Result<Option<ServerMsg>> {
        match m {
            Some(Ok(Message::Text(t))) => Ok(Some(serde_json::from_str(&t).context("relay json")?)),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => Ok(None),
            Some(Ok(other)) => bail!("unexpected relay message {other:?}"),
            Some(Err(e)) => Err(e.into()),
            None => bail!("relay closed during auth"),
        }
    };
    let challenge = loop {
        let m = tokio::time::timeout(Duration::from_secs(15), stream.next()).await.map_err(|_| anyhow!("no challenge"))?;
        if let Some(msg) = next_json(m)? {
            break msg;
        }
    };
    let ServerMsg::Challenge { relay_pub, nonce, protocol } = challenge else {
        bail!("expected challenge, got {challenge:?}");
    };
    if protocol != PROTOCOL_VERSION {
        bail!("relay speaks protocol {protocol}, we speak {PROTOCOL_VERSION}");
    }
    let nonce = relay::decode_nonce(&nonce)?;
    let proof = relay::auth_proof(&d.key, &relay_pub, &nonce, Role::Host)?;
    let auth = ClientMsg::Auth { role: Role::Host, public: d.key.public, proof, protocol: PROTOCOL_VERSION };
    sink.send(Message::Text(serde_json::to_string(&auth)?.into())).await?;
    let welcome = loop {
        let m = tokio::time::timeout(Duration::from_secs(15), stream.next()).await.map_err(|_| anyhow!("no welcome"))?;
        if let Some(msg) = next_json(m)? {
            break msg;
        }
    };
    match welcome {
        ServerMsg::Welcome { .. } => {}
        ServerMsg::Error { code, message } => bail!("relay refused: {code} {message}"),
        other => bail!("expected welcome, got {other:?}"),
    }
    {
        let mut st = d.relay_state.lock().unwrap();
        st.connected = true;
        st.error = None;
    }
    tracing::info!(relay = %url, host = %d.key.public, "connected to relay");

    let (out_tx, mut out_rx) = mpsc::channel::<Out>(4096);
    let links: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>> = Arc::default();
    let mut ping = tokio::time::interval(Duration::from_secs(25));
    ping.tick().await;
    let mut last_rx = std::time::Instant::now();
    let mut revoked = d.revoked();

    let result: Result<()> = loop {
        tokio::select! {
            m = stream.next() => {
                last_rx = std::time::Instant::now();
                match m {
                    Some(Ok(Message::Binary(b))) => {
                        let Some((link, payload)) = relay::decode_frame(&b) else { continue };
                        let tx = links.lock().unwrap().get(&link).cloned();
                        if let Some(tx) = tx {
                            if tx.try_send(payload.to_vec()).is_err() {
                                tracing::warn!(link, "link inbox full; closing link");
                                links.lock().unwrap().remove(&link);
                                let _ = out_tx.try_send(Out::Close(link));
                            }
                        }
                    }
                    Some(Ok(Message::Text(t))) => {
                        let Ok(msg) = serde_json::from_str::<ServerMsg>(&t) else { continue };
                        match msg {
                            ServerMsg::Incoming { link, from } => {
                                let (tx, rx) = mpsc::channel(LINK_INBOX);
                                links.lock().unwrap().insert(link, tx);
                                let d2 = d.clone();
                                let out = out_tx.clone();
                                let links2 = links.clone();
                                tokio::spawn(async move {
                                    let r = link_task(d2, link, from, rx, out.clone()).await;
                                    if let Err(e) = r {
                                        tracing::info!(link, "link ended: {e:#}");
                                    }
                                    if links2.lock().unwrap().remove(&link).is_some() {
                                        let _ = out.send(Out::Close(link)).await;
                                    }
                                });
                            }
                            ServerMsg::Closed { link, reason } => {
                                tracing::debug!(link, %reason, "link closed by relay");
                                links.lock().unwrap().remove(&link);
                            }
                            ServerMsg::Error { code, message } => {
                                if code == "replaced" {
                                    break Err(anyhow!("replaced by another connection with this host key"));
                                }
                                tracing::warn!(%code, %message, "relay error");
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(e.into()),
                }
            }
            o = out_rx.recv() => {
                let Some(o) = o else { break Ok(()) };
                let r = match o {
                    Out::Frame(link, bytes) => sink.send(Message::Binary(relay::encode_frame(link, &bytes).into())).await,
                    Out::Close(link) => {
                        links.lock().unwrap().remove(&link);
                        let m = serde_json::to_string(&ClientMsg::Close { link }).unwrap_or_default();
                        sink.send(Message::Text(m.into())).await
                    }
                };
                if let Err(e) = r {
                    break Err(e.into());
                }
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > RELAY_SILENCE {
                    break Err(anyhow!("relay silent for {}s", last_rx.elapsed().as_secs()));
                }
                let m = serde_json::to_string(&ClientMsg::Ping { ts: crate::util::now_ms() }).unwrap_or_default();
                if let Err(e) = sink.send(Message::Text(m.into())).await {
                    break Err(e.into());
                }
            }
            _ = revoked.changed() => {
                // Link tasks watch revocations themselves; nothing to do here.
            }
            _ = d.shutdown.notified() => {
                let _ = sink.send(Message::Close(None)).await;
                break Ok(());
            }
        }
    };
    links.lock().unwrap().clear();
    result
}

/// One encrypted link: handshake, authorize, then serve app messages.
async fn link_task(
    d: Arc<Daemon>,
    link: u32,
    from: PublicKey,
    mut inbox: mpsc::Receiver<Vec<u8>>,
    out: mpsc::Sender<Out>,
) -> Result<()> {
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, inbox.recv())
        .await
        .map_err(|_| anyhow!("handshake timeout"))?
        .ok_or_else(|| anyhow!("closed before handshake"))?;
    let mut responder = Responder::new(&d.key)?;
    let (remote, hello) = responder.read_hello(&first).context("noise handshake")?;
    let cfg = d.config();
    let mut host_hello = HostHello {
        protocol: PROTOCOL_VERSION,
        ok: false,
        error: None,
        host_name: cfg.name.clone(),
        os: crate::util::os_name().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        permissions: Vec::new(),
    };
    let auth: Result<Vec<String>, &str> = if remote != from {
        // The relay-authenticated key must be the Noise static key.
        Err("not_paired")
    } else if hello.protocol != PROTOCOL_VERSION {
        Err("protocol_mismatch")
    } else {
        d.authorize(&remote, &hello.device_name, &hello.client, hello.pair_token.as_deref())
    };
    match &auth {
        Ok(perms) => {
            host_hello.ok = true;
            host_hello.permissions = perms.clone();
        }
        Err(code) => host_hello.error = Some(code.to_string()),
    }
    let (channel, m2) = responder.write_response(&host_hello)?;
    out.send(Out::Frame(link, m2)).await.map_err(|_| anyhow!("relay gone"))?;
    let permissions = match auth {
        Ok(p) => p,
        Err(code) => {
            tracing::info!(link, device = %remote, code, "device rejected");
            // Let the response reach the device before closing.
            tokio::time::sleep(Duration::from_millis(500)).await;
            return Ok(());
        }
    };
    tracing::info!(link, device = %remote, name = %hello.device_name, "device connected");
    d.touch_device(&remote);

    let caller = Caller::Device { key: remote, permissions };
    let (client, mut rx) = ClientHandle::new(d.next_client_id());
    d.sessions.add_client(client.clone());
    let channel = Arc::new(Mutex::new(channel));

    // Writer: host messages -> encrypt -> relay.
    let writer = {
        let channel = channel.clone();
        let out = out.clone();
        let d2 = d.clone();
        let client2 = client.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let frames = {
                    let Ok(json) = serde_json::to_vec(&msg) else { continue };
                    match channel.lock().unwrap().encrypt(&json) {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::warn!("encrypt: {e}");
                            break;
                        }
                    }
                };
                for f in frames {
                    if out.send(Out::Frame(link, f)).await.is_err() {
                        return;
                    }
                }
                // The client fell behind earlier: once the queue drained, resync its views.
                if rx.is_empty() && client2.lagged.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    d2.sessions.resync_client(&client2).await;
                }
            }
        })
    };

    let mut revoked = d.revoked();
    let mut touch = tokio::time::interval(Duration::from_secs(60));
    let result: Result<()> = loop {
        tokio::select! {
            m = inbox.recv() => {
                let Some(bytes) = m else { break Ok(()) };
                let plain = match channel.lock().unwrap().decrypt(&bytes) {
                    Ok(p) => p,
                    Err(e) => break Err(anyhow!("decrypt failed: {e}")),
                };
                let Some(plain) = plain else { continue };
                let msg: AppClientMsg = match serde_json::from_slice(&plain) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!("bad app message: {e}");
                        continue;
                    }
                };
                dispatch(&d, &caller, &client, msg).await;
            }
            _ = revoked.changed() => {
                if revoked.borrow().contains(&remote) {
                    let _ = client.tx.send(HostMsg::event(yonder_proto::app::Event::Notice {
                        level: yonder_proto::app::NoticeLevel::Error,
                        message: "此设备已被撤销授权".into(),
                    })).await;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    break Ok(());
                }
            }
            _ = touch.tick() => d.touch_device(&remote),
        }
    };
    d.sessions.remove_client(client.id);
    drop(client);
    writer.abort();
    result
}

/// Handle one app message. Requests run concurrently (responses carry ids); terminal
/// input, resize and focus are applied in order.
pub async fn dispatch(d: &Arc<Daemon>, caller: &Caller, client: &ClientHandle, msg: AppClientMsg) {
    match msg {
        AppClientMsg::Req { id, req } => {
            let d = d.clone();
            let caller = caller.clone();
            let client = client.clone();
            tokio::spawn(async move {
                if let Some(res) = d.on_request(&caller, &client, id, req).await {
                    let _ = client.tx.send(res).await;
                }
            });
        }
        other => {
            let _ = d.on_client_msg(caller, client, other).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(split_url("wss://relay.example.com:2097/v1/ws").unwrap(), (true, "relay.example.com".into(), 2097));
        assert_eq!(split_url("ws://127.0.0.1:9/v1/ws").unwrap(), (false, "127.0.0.1".into(), 9));
        assert_eq!(split_url("wss://relay.example/v1/ws").unwrap(), (true, "relay.example".into(), 443));
        assert_eq!(split_url("wss://[::1]:8443/v1/ws").unwrap(), (true, "::1".into(), 8443));
        assert!(split_url("https://x").is_err());
    }
}
