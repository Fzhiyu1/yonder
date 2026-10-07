use std::sync::{Arc, Mutex as StdMutex};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use hkdf::Hkdf;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::webpush::{b64d, b64e};
use super::*;

// RFC 8291 section 5 / appendix A.
const RFC_PLAINTEXT: &[u8] = b"When I grow up, I want to be a watermelon";
const RFC_AS_PRIVATE: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
const RFC_UA_PUBLIC: &str =
    "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const RFC_UA_PRIVATE: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
const RFC_AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
const RFC_SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
const RFC_BODY: &str = concat!(
    "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml",
    "mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT",
    "pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
);

/// Receiver-side RFC 8291 decryption, used to check our own output.
fn decrypt(body: &[u8], ua_private: &[u8], auth: &[u8]) -> Vec<u8> {
    let salt = &body[..16];
    let rs = u32::from_be_bytes(body[16..20].try_into().unwrap());
    assert_eq!(rs, 4096);
    let idlen = body[20] as usize;
    assert_eq!(idlen, 65);
    let as_pub = &body[21..21 + idlen];
    let ct = &body[21 + idlen..];

    let ua_sk = SecretKey::from_slice(ua_private).unwrap();
    let ua_pub = ua_sk.public_key().to_encoded_point(false);
    let as_pk = PublicKey::from_sec1_bytes(as_pub).unwrap();
    let shared = p256::ecdh::diffie_hellman(ua_sk.to_nonzero_scalar(), as_pk.as_affine());

    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(ua_pub.as_bytes());
    info.extend_from_slice(as_pub);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes().as_slice()).expand(&info, &mut ikm).unwrap();
    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).unwrap();
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();
    let mut pt = Aes128Gcm::new_from_slice(&cek)
        .unwrap()
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: ct, aad: &[] })
        .unwrap();
    assert_eq!(pt.pop(), Some(0x02), "last-record delimiter");
    pt
}

#[test]
fn rfc8291_vector_is_byte_exact() {
    let salt: [u8; 16] = b64d(RFC_SALT).unwrap().try_into().unwrap();
    let out = encrypt_aes128gcm(
        RFC_PLAINTEXT,
        &b64d(RFC_UA_PUBLIC).unwrap(),
        &b64d(RFC_AUTH).unwrap(),
        &b64d(RFC_AS_PRIVATE).unwrap(),
        &salt,
    )
    .unwrap();
    assert_eq!(b64e(&out), RFC_BODY);
    // 86-byte header + 41 + 1 delimiter + 16 tag. (The RFC's example header says 145,
    // but its base64 body decodes to 144 bytes.)
    assert_eq!(out.len(), 144);
}

#[test]
fn random_encryption_round_trips() {
    let ua_private = b64d(RFC_UA_PRIVATE).unwrap();
    let auth = b64d(RFC_AUTH).unwrap();
    let msg = br#"{"title":"mac \xc2\xb7 approval","body":"rm -rf build"}"#;
    let a = webpush::encrypt(msg, &b64d(RFC_UA_PUBLIC).unwrap(), &auth).unwrap();
    let b = webpush::encrypt(msg, &b64d(RFC_UA_PUBLIC).unwrap(), &auth).unwrap();
    assert_ne!(a, b, "fresh salt and sender key per message");
    assert_eq!(decrypt(&a, &ua_private, &auth), msg);
    assert_eq!(decrypt(&b, &ua_private, &auth), msg);

    let too_big = vec![b'x'; 4096];
    assert!(webpush::encrypt(&too_big, &b64d(RFC_UA_PUBLIC).unwrap(), &auth).is_err());
}

#[test]
fn vapid_jwt_verifies() {
    let key = VapidKey::generate().unwrap();
    let header = vapid_authorization(&key, "https://fcm.googleapis.com", 1_900_000_000, "https://relay.example.com").unwrap();
    let rest = header.strip_prefix("vapid t=").unwrap();
    let (jwt, k) = rest.split_once(", k=").unwrap();
    assert_eq!(k, key.public_b64());
    assert_eq!(b64d(k).unwrap().len(), 65);

    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let h: serde_json::Value = serde_json::from_slice(&b64d(parts[0]).unwrap()).unwrap();
    assert_eq!(h["alg"], "ES256");
    let c: serde_json::Value = serde_json::from_slice(&b64d(parts[1]).unwrap()).unwrap();
    assert_eq!(c["aud"], "https://fcm.googleapis.com");
    assert_eq!(c["exp"], 1_900_000_000u64);
    assert_eq!(c["sub"], "https://relay.example.com");

    let vk = VerifyingKey::from_sec1_bytes(&b64d(k).unwrap()).unwrap();
    let sig = Signature::from_slice(&b64d(parts[2]).unwrap()).unwrap();
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    vk.verify(signing_input.as_bytes(), &sig).unwrap();
    assert!(vk.verify(b"tampered", &sig).is_err());

    assert!(vapid_authorization(&key, "https://fcm.googleapis.com", 1, "yonder@example.com").is_err());
    assert!(vapid_authorization(&key, "https://fcm.googleapis.com", 1, "mailto:me@example.com").is_ok());
}

/// Apple's push service answers 403 BadJwtToken for placeholder contacts (checked against
/// web.push.apple.com on 2026-09-28: `mailto:x@localhost`, `https://localhost`, `.local`,
/// `.test`, `.invalid` and dotless hosts fail; IP addresses and real domains pass). Private
/// and loopback addresses are not useful contacts, so they fall back to the project URL too.
#[test]
fn vapid_subject_prefers_public_app_url() {
    let dir = tempfile::tempdir().unwrap();
    let n = Notifier::new(NotifyConfig::default(), dir.path()).unwrap();
    assert_eq!(n.vapid_subject(), DEFAULT_VAPID_SUBJECT);
    for (url, public) in [
        ("https://relay.example.com", true),
        ("https://yonder.example.org/", true),
        ("http://relay.example.com", false),
        ("https://127.0.0.1:2097", false),
        ("https://192.168.1.10:2097", false),
        ("https://localhost:2097", false),
        ("https://mybox", false),
        ("https://yonder.local", false),
        ("https://foo.test", false),
        ("https://yonder.invalid", false),
        ("https://[::1]:2097", false),
    ] {
        n.set_app_url(url);
        let want = if public { url.trim_end_matches('/').to_string() } else { DEFAULT_VAPID_SUBJECT.to_string() };
        assert_eq!(n.vapid_subject(), want, "{url}");
    }
    let cfg = NotifyConfig { vapid_subject: Some("mailto:me@example.com".into()), ..NotifyConfig::default() };
    n.set_config(cfg).unwrap();
    assert_eq!(n.vapid_subject(), "mailto:me@example.com", "explicit setting wins");
}

#[test]
fn vapid_key_persists_privately() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vapid.key");
    let a = VapidKey::load_or_create(&path).unwrap();
    let b = VapidKey::load_or_create(&path).unwrap();
    assert_eq!(a.public_bytes(), b.public_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn config_serde_defaults() {
    let cfg: NotifyConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg, NotifyConfig::default());
    assert!(cfg.web_push);
    assert!(!cfg.include_content);

    let cfg: NotifyConfig = serde_json::from_str(
        r#"{"channels":[
            {"type":"ntfy","topic":"yonder-abc"},
            {"type":"bark","key":"K"},
            {"type":"webhook","url":"https://example.com/hook","headers":{"X-Token":"t"}}
        ],"include_content":true,"web_push":false}"#,
    )
    .unwrap();
    assert_eq!(
        cfg.channels[0],
        ChannelConfig::Ntfy { server: "https://ntfy.sh".into(), topic: "yonder-abc".into(), token: None }
    );
    assert_eq!(cfg.channels[1], ChannelConfig::Bark { server: "https://api.day.app".into(), key: "K".into() });
    assert_eq!(cfg.channels[2].label(), "webhook:https://example.com");
    assert!(cfg.include_content && !cfg.web_push);

    let kind = serde_json::to_string(&NotifyKind::TurnDone).unwrap();
    assert_eq!(kind, r#""turn_done""#);
    let json = serde_json::to_value(ChannelConfig::Ntfy {
        server: "https://ntfy.sh".into(),
        topic: "t".into(),
        token: None,
    })
    .unwrap();
    assert_eq!(json, serde_json::json!({"type":"ntfy","server":"https://ntfy.sh","topic":"t"}));
}

#[derive(Debug, Clone)]
struct Captured {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Minimal HTTP/1.1 server that records requests and answers with `status`.
async fn mock_server(status: u16) -> (String, Arc<StdMutex<Vec<Captured>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let seen = seen2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let path = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("").to_string();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
                    .collect();
                let len: usize = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                while buf.len() < header_end + len {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = buf[header_end..(header_end + len).min(buf.len())].to_vec();
                seen.lock().unwrap().push(Captured { path, headers, body });
                let resp = format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

fn sample(kind: NotifyKind, session: &str) -> Notification {
    Notification {
        kind,
        host: "mac".into(),
        session: Some(session.into()),
        session_title: Some("fix build".into()),
        title: "Approval needed".into(),
        body: "cargo build --release".into(),
        url: Some("https://relay.example/#s=abc".into()),
        tag: Some(format!("approval-{session}")),
    }
}

#[tokio::test]
async fn webhook_debounce_and_content_policy() {
    let (base, seen) = mock_server(200).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = NotifyConfig {
        channels: vec![ChannelConfig::Webhook {
            url: format!("{base}/hook"),
            headers: [("X-Token".to_string(), "secret".to_string())].into(),
        }],
        include_content: false,
        web_push: true,
        proxy: None,
        vapid_subject: None,
    };
    let n = Notifier::new(cfg, dir.path()).unwrap();
    assert!(n.has_targets());

    let r = n.notify(&sample(NotifyKind::Approval, "s1")).await;
    assert_eq!(r.len(), 1);
    assert!(r[0].1.is_ok(), "{:?}", r[0].1);
    // Same kind + session within 5 s: debounced.
    assert!(n.notify(&sample(NotifyKind::Approval, "s1")).await.is_empty());
    // Different session is not debounced.
    assert_eq!(n.notify(&sample(NotifyKind::Approval, "s2")).await.len(), 1);
    // Tests are never debounced.
    assert_eq!(n.notify(&sample(NotifyKind::Test, "s1")).await.len(), 1);
    assert_eq!(n.notify(&sample(NotifyKind::Test, "s1")).await.len(), 1);

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0].path, "/hook");
    assert_eq!(seen[0].header("x-token"), Some("secret"));
    let v: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(v["kind"], "approval");
    assert_eq!(v["body"], "fix build: needs your approval", "content withheld by default");
    let t: serde_json::Value = serde_json::from_slice(&seen[2].body).unwrap();
    assert_eq!(t["body"], "cargo build --release", "test notifications carry their text");
}

#[tokio::test]
async fn ntfy_and_bark_payloads() {
    let (base, seen) = mock_server(200).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = NotifyConfig {
        channels: vec![
            ChannelConfig::Ntfy { server: base.clone(), topic: "yonder-t".into(), token: Some("tk".into()) },
            ChannelConfig::Bark { server: base.clone(), key: "BK".into() },
        ],
        include_content: true,
        web_push: false,
        proxy: None,
        vapid_subject: None,
    };
    let n = Notifier::new(cfg, dir.path()).unwrap();
    let r = n.notify(&sample(NotifyKind::Approval, "s1")).await;
    assert!(r.iter().all(|(_, r)| r.is_ok()), "{r:?}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    let ntfy = seen.iter().find(|c| c.path == "/").unwrap();
    assert_eq!(ntfy.header("authorization"), Some("Bearer tk"));
    let v: serde_json::Value = serde_json::from_slice(&ntfy.body).unwrap();
    assert_eq!(v["topic"], "yonder-t");
    assert_eq!(v["message"], "cargo build --release");
    assert_eq!(v["click"], "https://relay.example/#s=abc");
    assert_eq!(v["priority"], 4);
    let bark = seen.iter().find(|c| c.path == "/push").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bark.body).unwrap();
    assert_eq!(v["device_key"], "BK");
    assert_eq!(v["group"], "yonder");
    assert_eq!(v["level"], "timeSensitive");
}

fn rfc_subscription(endpoint: String) -> PushSubscription {
    PushSubscription {
        device: "dev1".into(),
        endpoint,
        p256dh: RFC_UA_PUBLIC.into(),
        auth: RFC_AUTH.into(),
        vapid_private: None,
        created_at: 0,
    }
}

#[tokio::test]
async fn web_push_request_decrypts_and_expired_subscription_is_dropped() {
    let (base, seen) = mock_server(201).await;
    let dir = tempfile::tempdir().unwrap();
    let n = Notifier::new(NotifyConfig::default(), dir.path()).unwrap();
    n.set_app_url("https://relay.example.com/");
    // The public API only accepts https endpoints; inject a local one directly.
    n.subs.lock().unwrap().push(rfc_subscription(format!("{base}/push/abc")));

    let r = n.notify(&sample(NotifyKind::Approval, "s1")).await;
    assert_eq!(r.len(), 1);
    assert!(r[0].1.is_ok(), "{:?}", r[0].1);
    let req = seen.lock().unwrap()[0].clone();
    assert_eq!(req.path, "/push/abc");
    assert_eq!(req.header("content-encoding"), Some("aes128gcm"));
    assert_eq!(req.header("ttl"), Some("86400"));
    assert_eq!(req.header("urgency"), Some("high"));
    let auth_header = req.header("authorization").unwrap();
    assert!(auth_header.starts_with("vapid t="));
    assert!(auth_header.ends_with(&format!(", k={}", n.vapid_public())));
    let jwt = auth_header.strip_prefix("vapid t=").unwrap().split(", k=").next().unwrap();
    let claims: serde_json::Value = serde_json::from_slice(&b64d(jwt.split('.').nth(1).unwrap()).unwrap()).unwrap();
    assert_eq!(claims["sub"], "https://relay.example.com", "the app URL is the VAPID contact");
    assert_eq!(claims["aud"], base.as_str());

    let pt = decrypt(&req.body, &b64d(RFC_UA_PRIVATE).unwrap(), &b64d(RFC_AUTH).unwrap());
    let v: serde_json::Value = serde_json::from_slice(&pt).unwrap();
    assert_eq!(v["title"], "mac · Approval needed");
    assert_eq!(v["body"], "cargo build --release", "push payload is end-to-end encrypted, full text");
    assert_eq!(v["session"], "s1");
    assert_eq!(v["kind"], "approval");

    // 410 Gone removes the subscription (and persists that).
    let (gone, _) = mock_server(410).await;
    let dir2 = tempfile::tempdir().unwrap();
    let n2 = Notifier::new(NotifyConfig::default(), dir2.path()).unwrap();
    n2.subs.lock().unwrap().push(rfc_subscription(format!("{gone}/push/x")));
    n2.save(&n2.subscriptions()).unwrap();
    let r = n2.notify(&sample(NotifyKind::TurnDone, "s1")).await;
    assert!(r[0].1.is_err());
    assert!(n2.subscriptions().is_empty());
    let reloaded = Notifier::new(NotifyConfig::default(), dir2.path()).unwrap();
    assert!(reloaded.subscriptions().is_empty());
    assert_eq!(reloaded.vapid_public(), n2.vapid_public(), "VAPID key is stable across restarts");
}

#[test]
fn subscribe_validates_and_upserts() {
    let dir = tempfile::tempdir().unwrap();
    let n = Notifier::new(NotifyConfig::default(), dir.path()).unwrap();
    assert!(!n.has_targets());
    assert!(n.subscribe("d", "http://push.example/x", RFC_UA_PUBLIC, RFC_AUTH, None).is_err());
    assert!(n.subscribe("d", "https://push.example/x", "AAAA", RFC_AUTH, None).is_err());
    assert!(n.subscribe("d", "https://push.example/x", RFC_UA_PUBLIC, "AAAA", None).is_err());
    n.subscribe("d", "https://push.example/x", RFC_UA_PUBLIC, RFC_AUTH, None).unwrap();
    n.subscribe("d", "https://push.example/x", RFC_UA_PUBLIC, RFC_AUTH, None).unwrap();
    n.subscribe("e", "https://push.example/y", RFC_UA_PUBLIC, RFC_AUTH, None).unwrap();
    assert_eq!(n.subscriptions().len(), 2);
    assert!(n.has_targets());
    n.remove_device("d").unwrap();
    assert_eq!(n.subscriptions().len(), 1);
    let reloaded = Notifier::new(NotifyConfig::default(), dir.path()).unwrap();
    assert_eq!(reloaded.subscriptions()[0].device, "e");
}
