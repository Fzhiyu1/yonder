//! A device-side client (what the web app does, in Rust): relay auth, open a link, Noise
//! IK handshake, then app requests and events. Used by end-to-end tests and diagnostics.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use yonder_proto::app::{ClientMsg as AppClientMsg, Event, HostMsg, Request, Response};
use yonder_proto::keys::{Keypair, PublicKey};
use yonder_proto::noise::{DeviceHello, HostHello, Initiator};
use yonder_proto::relay::{self, ClientMsg, Role, ServerMsg};
use yonder_proto::{FEATURE_SUBAGENTS, PROTOCOL_VERSION};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<HostMsg>>>>;

pub struct DeviceClient {
    pub host_hello: HostHello,
    out: mpsc::Sender<Vec<u8>>,
    pending: Pending,
    next_id: Mutex<u64>,
    pub events: tokio::sync::Mutex<mpsc::Receiver<Event>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for DeviceClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl DeviceClient {
    /// Connect through `relay_url` to `host` and complete the handshake.
    pub async fn connect(relay_url: &str, device: &Keypair, host: &PublicKey, name: &str, pair_token: Option<&str>) -> Result<Self> {
        let tls = rustls_config();
        let connector = if relay_url.starts_with("wss://") {
            Some(tokio_tungstenite::Connector::Rustls(tls))
        } else {
            Some(tokio_tungstenite::Connector::Plain)
        };
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(20),
            tokio_tungstenite::connect_async_tls_with_config(relay_url, None, true, connector),
        )
        .await
        .map_err(|_| anyhow!("connect timed out"))?
        .context("connect relay")?;
        let (mut sink, mut stream) = ws.split();
        let ServerMsg::Challenge { relay_pub, nonce, .. } = next_ctl(&mut stream).await? else { bail!("expected challenge") };
        let proof = relay::auth_proof(device, &relay_pub, &relay::decode_nonce(&nonce)?, Role::Device)?;
        let auth = ClientMsg::Auth { role: Role::Device, public: device.public, proof, protocol: PROTOCOL_VERSION };
        sink.send(Message::Text(serde_json::to_string(&auth)?.into())).await?;
        match next_ctl(&mut stream).await? {
            ServerMsg::Welcome { .. } => {}
            other => bail!("auth failed: {other:?}"),
        }
        sink.send(Message::Text(serde_json::to_string(&ClientMsg::Open { req: 1, to: *host })?.into())).await?;
        let link = loop {
            match next_ctl(&mut stream).await? {
                ServerMsg::Opened { link, .. } => break link,
                ServerMsg::OpenFailed { reason, .. } => bail!("open failed: {reason}"),
                _ => continue,
            }
        };
        let mut ini = Initiator::new(device, host)?;
        let hello = DeviceHello {
            protocol: PROTOCOL_VERSION,
            device_name: name.into(),
            client: "cli".into(),
            pair_token: pair_token.map(str::to_string),
            features: Some(vec![FEATURE_SUBAGENTS.into()]),
        };
        let m1 = ini.write_hello(&hello)?;
        sink.send(Message::Binary(relay::encode_frame(link, &m1).into())).await?;
        let m2 = loop {
            match tokio::time::timeout(Duration::from_secs(20), stream.next()).await {
                Err(_) => bail!("handshake timeout"),
                Ok(None) => bail!("relay closed during handshake"),
                Ok(Some(Err(e))) => return Err(anyhow!(e)),
                Ok(Some(Ok(Message::Binary(b)))) => {
                    let (l, p) = relay::decode_frame(&b).ok_or_else(|| anyhow!("bad frame"))?;
                    if l == link {
                        break p.to_vec();
                    }
                }
                Ok(Some(Ok(Message::Text(t)))) => {
                    if let Ok(ServerMsg::Closed { link: l, reason }) = serde_json::from_str::<ServerMsg>(&t) {
                        if l == link {
                            bail!("link closed during handshake: {reason}");
                        }
                    }
                }
                Ok(Some(Ok(_))) => {}
            }
        };
        let (channel, host_hello) = ini.read_response(&m2)?;
        if !host_hello.ok {
            bail!("host refused: {}", host_hello.error.clone().unwrap_or_default());
        }
        let channel = Arc::new(Mutex::new(channel));
        let pending: Pending = Arc::default();
        let (ev_tx, ev_rx) = mpsc::channel::<Event>(100_000);
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(1024);
        let task = {
            let channel = channel.clone();
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut ping = tokio::time::interval(Duration::from_secs(20));
                loop {
                    tokio::select! {
                        m = stream.next() => {
                            let Some(Ok(m)) = m else { break };
                            let Message::Binary(b) = m else { continue };
                            let Some((l, p)) = relay::decode_frame(&b) else { continue };
                            if l != link { continue; }
                            let plain = match channel.lock().unwrap().decrypt(p) {
                                Ok(Some(x)) => x,
                                Ok(None) => continue,
                                Err(_) => break,
                            };
                            let Ok(msg) = serde_json::from_slice::<HostMsg>(&plain) else { continue };
                            match msg {
                                HostMsg::Res { id, .. } => {
                                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                        let _ = tx.send(msg);
                                    }
                                }
                                HostMsg::Event { event } => {
                                    let _ = ev_tx.try_send(event);
                                }
                            }
                        }
                        o = out_rx.recv() => {
                            let Some(json) = o else { break };
                            let frames = match channel.lock().unwrap().encrypt(&json) {
                                Ok(f) => f,
                                Err(_) => break,
                            };
                            for f in frames {
                                if sink.send(Message::Binary(relay::encode_frame(link, &f).into())).await.is_err() {
                                    return;
                                }
                            }
                        }
                        _ = ping.tick() => {
                            let m = serde_json::to_string(&ClientMsg::Ping { ts: 0 }).unwrap_or_default();
                            if sink.send(Message::Text(m.into())).await.is_err() { break; }
                        }
                    }
                }
            })
        };
        Ok(Self { host_hello, out: out_tx, pending, next_id: Mutex::new(1), events: tokio::sync::Mutex::new(ev_rx), task })
    }

    pub async fn send(&self, msg: &AppClientMsg) -> Result<()> {
        self.out.send(serde_json::to_vec(msg)?).await.map_err(|_| anyhow!("link closed"))
    }

    /// Request with a timeout; returns the response or the host's error.
    pub async fn request(&self, req: Request) -> Result<Response> {
        self.request_timeout(req, Duration::from_secs(60)).await
    }

    pub async fn request_timeout(&self, req: Request, timeout: Duration) -> Result<Response> {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(&AppClientMsg::Req { id, req }).await?;
        let msg = tokio::time::timeout(timeout, rx).await.map_err(|_| anyhow!("request {id} timed out"))??;
        match msg {
            HostMsg::Res { ok: true, data: Some(d), .. } => Ok(d),
            HostMsg::Res { error: Some(e), .. } => bail!("{}: {}", e.code, e.message),
            other => bail!("bad response {other:?}"),
        }
    }

    /// Wait for the first event matching `f`.
    pub async fn wait_event<T>(&self, timeout: Duration, mut f: impl FnMut(&Event) -> Option<T>) -> Result<T> {
        let mut rx = self.events.lock().await;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let ev = tokio::time::timeout_at(deadline, rx.recv()).await.map_err(|_| anyhow!("timed out waiting for event"))?;
            let ev = ev.ok_or_else(|| anyhow!("link closed"))?;
            if let Some(t) = f(&ev) {
                return Ok(t);
            }
        }
    }
}

/// TLS config trusting the bundled web PKI roots.
pub fn rustls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("tls versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

async fn next_ctl<S>(stream: &mut S) -> Result<ServerMsg>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match tokio::time::timeout(Duration::from_secs(15), stream.next()).await {
            Err(_) => bail!("relay timeout"),
            Ok(None) => bail!("relay closed"),
            Ok(Some(Err(e))) => return Err(anyhow!(e)),
            Ok(Some(Ok(Message::Text(t)))) => return Ok(serde_json::from_str::<ServerMsg>(&t)?),
            Ok(Some(Ok(_))) => continue,
        }
    }
}
