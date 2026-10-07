//! One WebSocket peer: challenge/auth, then control messages and binary forwarding.

use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use yonder_proto::keys::{random_bytes, PublicKey};
use yonder_proto::relay::{self, ClientMsg, Role, ServerMsg, NONCE_LEN};
use yonder_proto::PROTOCOL_VERSION;

use crate::hub::{ForwardResult, Out};
use crate::AppState;

fn short(k: &PublicKey) -> String {
    k.to_b64()[..8].to_string()
}

pub async fn handle(st: Arc<AppState>, socket: WebSocket, ip: IpAddr) {
    let (mut sink, mut stream) = socket.split();
    let Ok(nonce) = random_bytes::<NONCE_LEN>() else { return };
    let challenge = ServerMsg::Challenge {
        relay_pub: st.cfg.key.public,
        nonce: relay::encode_nonce(&nonce),
        protocol: PROTOCOL_VERSION,
    };
    if sink.send(Message::Text(serde_json::to_string(&challenge).unwrap().into())).await.is_err() {
        return;
    }

    // Authentication.
    let auth = tokio::time::timeout(st.cfg.auth_timeout, async {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Text(t) => return serde_json::from_str::<ClientMsg>(&t).ok(),
                Message::Ping(_) | Message::Pong(_) => continue,
                _ => return None,
            }
        }
        None
    })
    .await;
    let (role, key) = match auth {
        Ok(Some(ClientMsg::Auth { role, public, proof, protocol })) => {
            if protocol != PROTOCOL_VERSION {
                let _ = send_err(&mut sink, "protocol_mismatch", &format!("relay speaks protocol {PROTOCOL_VERSION}")).await;
                return;
            }
            if !relay::verify_auth_proof(&st.cfg.key, &public, &nonce, role, &proof) {
                let _ = send_err(&mut sink, "auth_failed", "invalid proof").await;
                return;
            }
            (role, public)
        }
        _ => {
            let _ = send_err(&mut sink, "auth_required", "expected auth").await;
            return;
        }
    };

    let id = st.hub.next_conn_id();
    let (tx, mut rx) = mpsc::channel::<Out>(st.cfg.queue);
    let welcome = ServerMsg::Welcome { you: key, role };
    if sink.send(Message::Text(serde_json::to_string(&welcome).unwrap().into())).await.is_err() {
        return;
    }
    st.hub.register(id, role, key, tx.clone(), ip);
    info!(conn = id, role = ?role, key = %short(&key), %ip, "peer authenticated");

    // Writer task: queued messages + periodic pings.
    let ping_every = st.cfg.ping_interval;
    let writer = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(ping_every);
        ticker.tick().await;
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(Out::Json(msg)) => {
                        let s = serde_json::to_string(&msg).unwrap();
                        if sink.send(Message::Text(s.into())).await.is_err() { break; }
                    }
                    Some(Out::Binary(b)) => {
                        if sink.send(Message::Binary(b.into())).await.is_err() { break; }
                    }
                    Some(Out::Close) | None => {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                },
                _ = ticker.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
                }
            }
        }
    });

    let idle = st.cfg.idle_timeout;
    let mut unknown_reported: std::collections::HashSet<u32> = Default::default();
    loop {
        let next = tokio::time::timeout(idle, stream.next()).await;
        let msg = match next {
            Err(_) => {
                debug!(conn = id, "idle timeout");
                break;
            }
            Ok(None) | Ok(Some(Err(_))) => break,
            Ok(Some(Ok(m))) => m,
        };
        match msg {
            Message::Binary(b) => {
                let Some((link, _)) = relay::decode_frame(&b) else {
                    warn!(conn = id, len = b.len(), "bad frame");
                    continue;
                };
                match st.hub.forward(id, link, b.to_vec()) {
                    ForwardResult::Sent => {}
                    ForwardResult::PeerSlow(other) => {
                        warn!(conn = id, other, "peer queue full; disconnecting slow peer");
                        st.hub.kick(other);
                    }
                    ForwardResult::UnknownLink => {
                        if unknown_reported.insert(link) {
                            let _ = tx.try_send(Out::Json(ServerMsg::Closed { link, reason: "unknown_link".into() }));
                        }
                    }
                }
            }
            Message::Text(t) => {
                let Ok(cm) = serde_json::from_str::<ClientMsg>(&t) else {
                    let _ = tx.try_send(Out::Json(ServerMsg::Error { code: "bad_message".into(), message: "unparseable".into() }));
                    continue;
                };
                match cm {
                    ClientMsg::Open { req, to } => {
                        if role == Role::Device {
                            st.hub.open(id, req, to);
                        } else {
                            let _ = tx.try_send(Out::Json(ServerMsg::OpenFailed { req, to, reason: "invalid".into() }));
                        }
                    }
                    ClientMsg::Close { link } => {
                        unknown_reported.remove(&link);
                        st.hub.close(id, link)
                    }
                    ClientMsg::Watch { hosts } => st.hub.watch(id, hosts),
                    ClientMsg::Ping { ts } => {
                        let _ = tx.try_send(Out::Json(ServerMsg::Pong { ts }));
                    }
                    ClientMsg::Auth { .. } => {
                        let _ = tx.try_send(Out::Json(ServerMsg::Error { code: "already_authenticated".into(), message: String::new() }));
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }
    st.hub.unregister(id);
    let _ = tx.try_send(Out::Close);
    drop(tx);
    let _ = writer.await;
    info!(conn = id, key = %short(&key), "peer disconnected");
}

async fn send_err<S>(sink: &mut S, code: &str, message: &str) -> Result<(), axum::Error>
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    let m = ServerMsg::Error { code: code.into(), message: message.into() };
    sink.send(Message::Text(serde_json::to_string(&m).unwrap().into())).await?;
    sink.send(Message::Close(None)).await
}
