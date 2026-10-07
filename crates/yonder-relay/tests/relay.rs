use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use yonder_proto::keys::{Keypair, PublicKey};
use yonder_proto::noise::{DeviceHello, HostHello, Initiator, Responder};
use yonder_proto::relay::{self, ClientMsg, Role, ServerMsg};
use yonder_relay::{tls, AppState, Hub, Limits, RelayConfig};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start() -> (String, Arc<AppState>) {
    start_with(Limits::default()).await
}

async fn start_with(limits: Limits) -> (String, Arc<AppState>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut cfg = RelayConfig::new(Keypair::generate().unwrap());
    cfg.auth_timeout = Duration::from_secs(2);
    let state = Arc::new(AppState { hub: Hub::new(limits), cfg });
    let st = state.clone();
    tokio::spawn(async move { yonder_relay::serve_with_state(tls::plain(listener).unwrap(), st).await });
    (format!("ws://{addr}{}", relay::WS_PATH), state)
}

async fn recv_json(ws: &mut Ws) -> ServerMsg {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("timeout").expect("eof").unwrap();
        match m {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}

async fn recv_bin(ws: &mut Ws) -> Vec<u8> {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("timeout").expect("eof").unwrap();
        match m {
            Message::Binary(b) => return b.to_vec(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}

async fn send(ws: &mut Ws, m: &ClientMsg) {
    ws.send(Message::Text(serde_json::to_string(m).unwrap().into())).await.unwrap();
}

async fn connect(url: &str, kp: &Keypair, role: Role) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let ServerMsg::Challenge { relay_pub, nonce, protocol } = recv_json(&mut ws).await else { panic!("no challenge") };
    assert_eq!(protocol, 1);
    let nonce = relay::decode_nonce(&nonce).unwrap();
    let proof = relay::auth_proof(kp, &relay_pub, &nonce, role).unwrap();
    send(&mut ws, &ClientMsg::Auth { role, public: kp.public, proof, protocol: 1 }).await;
    match recv_json(&mut ws).await {
        ServerMsg::Welcome { you, role: r } => {
            assert_eq!(you, kp.public);
            assert_eq!(r, role);
        }
        other => panic!("expected welcome, got {other:?}"),
    }
    ws
}

async fn open(dev: &mut Ws, host: &mut Ws, host_pub: PublicKey, dev_pub: PublicKey, req: u32) -> u32 {
    send(dev, &ClientMsg::Open { req, to: host_pub }).await;
    let link = match recv_json(host).await {
        ServerMsg::Incoming { link, from } => {
            assert_eq!(from, dev_pub);
            link
        }
        other => panic!("expected incoming, got {other:?}"),
    };
    match recv_json(dev).await {
        ServerMsg::Opened { req: r, link: l, to } => {
            assert_eq!((r, l, to), (req, link, host_pub));
        }
        other => panic!("expected opened, got {other:?}"),
    }
    link
}

#[tokio::test]
async fn auth_rejects_bad_proof() {
    let (url, _) = start().await;
    let kp = Keypair::generate().unwrap();
    let other = Keypair::generate().unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let ServerMsg::Challenge { relay_pub, nonce, .. } = recv_json(&mut ws).await else { panic!() };
    let nonce = relay::decode_nonce(&nonce).unwrap();
    // Proof made with a different key than the claimed public key.
    let proof = relay::auth_proof(&other, &relay_pub, &nonce, Role::Host).unwrap();
    send(&mut ws, &ClientMsg::Auth { role: Role::Host, public: kp.public, proof, protocol: 1 }).await;
    match recv_json(&mut ws).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "auth_failed"),
        other => panic!("{other:?}"),
    }
    // Wrong protocol version.
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let ServerMsg::Challenge { relay_pub, nonce, .. } = recv_json(&mut ws).await else { panic!() };
    let nonce = relay::decode_nonce(&nonce).unwrap();
    let proof = relay::auth_proof(&kp, &relay_pub, &nonce, Role::Host).unwrap();
    send(&mut ws, &ClientMsg::Auth { role: Role::Host, public: kp.public, proof, protocol: 99 }).await;
    match recv_json(&mut ws).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "protocol_mismatch"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn end_to_end_noise_through_relay() {
    let (url, _) = start().await;
    let host_kp = Keypair::generate().unwrap();
    let dev_kp = Keypair::generate().unwrap();
    let mut host = connect(&url, &host_kp, Role::Host).await;
    let mut dev = connect(&url, &dev_kp, Role::Device).await;
    let link = open(&mut dev, &mut host, host_kp.public, dev_kp.public, 1).await;

    // Real Noise IK handshake through the relay.
    let mut ini = Initiator::new(&dev_kp, &host_kp.public).unwrap();
    let m1 = ini
        .write_hello(&DeviceHello {
            protocol: 1,
            device_name: "t".into(),
            client: "cli".into(),
            pair_token: None,
            features: None,
        })
        .unwrap();
    dev.send(Message::Binary(relay::encode_frame(link, &m1).into())).await.unwrap();
    let f = recv_bin(&mut host).await;
    let (l, payload) = relay::decode_frame(&f).unwrap();
    assert_eq!(l, link);
    let mut resp = Responder::new(&host_kp).unwrap();
    let (who, _) = resp.read_hello(payload).unwrap();
    assert_eq!(who, dev_kp.public);
    let (mut hch, m2) = resp.write_response(&HostHello { protocol: 1, ok: true, ..Default::default() }).unwrap();
    host.send(Message::Binary(relay::encode_frame(link, &m2).into())).await.unwrap();
    let f = recv_bin(&mut dev).await;
    let (mut dch, hh) = ini.read_response(relay::decode_frame(&f).unwrap().1).unwrap();
    assert!(hh.ok);

    // Large fragmented message device -> host.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
    for frag in dch.encrypt(&big).unwrap() {
        dev.send(Message::Binary(relay::encode_frame(link, &frag).into())).await.unwrap();
    }
    let mut got = None;
    while got.is_none() {
        let f = recv_bin(&mut host).await;
        got = hch.decrypt(relay::decode_frame(&f).unwrap().1).unwrap();
    }
    assert_eq!(got.unwrap(), big);
    // And back.
    for frag in hch.encrypt(b"ack").unwrap() {
        host.send(Message::Binary(relay::encode_frame(link, &frag).into())).await.unwrap();
    }
    let f = recv_bin(&mut dev).await;
    assert_eq!(dch.decrypt(relay::decode_frame(&f).unwrap().1).unwrap().unwrap(), b"ack");

    // Close from device propagates to host.
    send(&mut dev, &ClientMsg::Close { link }).await;
    match recv_json(&mut host).await {
        ServerMsg::Closed { link: l, reason } => {
            assert_eq!(l, link);
            assert_eq!(reason, "peer_closed");
        }
        other => panic!("{other:?}"),
    }
    // Frames on a closed link are not forwarded; sender learns the link is unknown.
    dev.send(Message::Binary(relay::encode_frame(link, b"zzz").into())).await.unwrap();
    match recv_json(&mut dev).await {
        ServerMsg::Closed { reason, .. } => assert_eq!(reason, "unknown_link"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn presence_offline_and_replacement() {
    let (url, _) = start().await;
    let host_kp = Keypair::generate().unwrap();
    let dev_kp = Keypair::generate().unwrap();
    let mut dev = connect(&url, &dev_kp, Role::Device).await;
    send(&mut dev, &ClientMsg::Watch { hosts: vec![host_kp.public] }).await;
    match recv_json(&mut dev).await {
        ServerMsg::Presence { host, online } => {
            assert_eq!(host, host_kp.public);
            assert!(!online);
        }
        other => panic!("{other:?}"),
    }
    send(&mut dev, &ClientMsg::Open { req: 5, to: host_kp.public }).await;
    match recv_json(&mut dev).await {
        ServerMsg::OpenFailed { req, reason, .. } => {
            assert_eq!(req, 5);
            assert_eq!(reason, "host_offline");
        }
        other => panic!("{other:?}"),
    }

    let mut host1 = connect(&url, &host_kp, Role::Host).await;
    match recv_json(&mut dev).await {
        ServerMsg::Presence { online, .. } => assert!(online),
        other => panic!("{other:?}"),
    }
    let link = open(&mut dev, &mut host1, host_kp.public, dev_kp.public, 6).await;

    // A second connection with the same host key replaces the first.
    let _host2 = connect(&url, &host_kp, Role::Host).await;
    match recv_json(&mut host1).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "replaced"),
        other => panic!("{other:?}"),
    }
    match recv_json(&mut dev).await {
        ServerMsg::Closed { link: l, reason } => {
            assert_eq!(l, link);
            assert_eq!(reason, "host_replaced");
        }
        other => panic!("{other:?}"),
    }
    drop(_host2);
    // The replacement re-announces online; eventually the host goes offline.
    loop {
        match recv_json(&mut dev).await {
            ServerMsg::Presence { online: true, .. } => continue,
            ServerMsg::Presence { online: false, .. } => break,
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn host_disconnect_closes_links_and_limits() {
    let (url, _) = start_with(Limits { opens_per_minute: 3, ..Limits::default() }).await;
    let host_kp = Keypair::generate().unwrap();
    let dev_kp = Keypair::generate().unwrap();
    let mut host = connect(&url, &host_kp, Role::Host).await;
    let mut dev = connect(&url, &dev_kp, Role::Device).await;
    let link = open(&mut dev, &mut host, host_kp.public, dev_kp.public, 1).await;
    // Hosts cannot open links.
    send(&mut host, &ClientMsg::Open { req: 9, to: dev_kp.public }).await;
    match recv_json(&mut host).await {
        ServerMsg::OpenFailed { reason, .. } => assert_eq!(reason, "invalid"),
        other => panic!("{other:?}"),
    }
    // Ping/pong.
    send(&mut dev, &ClientMsg::Ping { ts: 42 }).await;
    match recv_json(&mut dev).await {
        ServerMsg::Pong { ts } => assert_eq!(ts, 42),
        other => panic!("{other:?}"),
    }
    host.close(None).await.unwrap();
    match recv_json(&mut dev).await {
        ServerMsg::Closed { link: l, reason } => {
            assert_eq!(l, link);
            assert_eq!(reason, "peer_gone");
        }
        other => panic!("{other:?}"),
    }
    // Rate limit: 3 opens per minute (1 used above).
    let mut host = connect(&url, &host_kp, Role::Host).await;
    open(&mut dev, &mut host, host_kp.public, dev_kp.public, 2).await;
    open(&mut dev, &mut host, host_kp.public, dev_kp.public, 3).await;
    send(&mut dev, &ClientMsg::Open { req: 4, to: host_kp.public }).await;
    match recv_json(&mut dev).await {
        ServerMsg::OpenFailed { reason, .. } => assert_eq!(reason, "rate_limited"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn tls_from_pem_files() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let dir = std::env::temp_dir().join(format!("yonder-relay-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_pem = ck.cert.pem();
    std::fs::write(dir.join("cert.pem"), &cert_pem).unwrap();
    std::fs::write(dir.join("key.pem"), ck.signing_key.serialize_pem()).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let incoming = tls::pem_files(listener, dir.join("cert.pem"), dir.join("key.pem"), Duration::from_secs(60)).unwrap();
    let kp = Keypair::generate().unwrap();
    tokio::spawn(yonder_relay::serve(incoming, RelayConfig::new(kp)));

    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let cfg = tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let connector = tokio_tungstenite::Connector::Rustls(Arc::new(cfg));
    let url = format!("wss://localhost:{port}{}", relay::WS_PATH);
    let (mut ws, _) = tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector)).await.unwrap();
    match recv_json(&mut ws).await {
        ServerMsg::Challenge { protocol, .. } => assert_eq!(protocol, 1),
        other => panic!("{other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}
