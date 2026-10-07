//! Live check against Apple's Web Push service (network): `cargo test -p yonder-notify --test
//! apple_live -- --ignored`. The subscription token is random, so Apple can only answer
//! 400 BadWebPushToken if it accepted our VAPID JWT; a rejected JWT gives 403 BadJwtToken.

use yonder_notify::{Notification, NotifyConfig, NotifyKind, Notifier};

const UA_PUBLIC: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

async fn send(app_url: Option<&str>, subject: Option<&str>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let cfg = NotifyConfig { vapid_subject: subject.map(str::to_string), ..NotifyConfig::default() };
    let n = Notifier::new(cfg, dir.path()).unwrap();
    if let Some(u) = app_url {
        n.set_app_url(u);
    }
    let token: String = (0..43).map(|i| (b'a' + (i * 7 % 26) as u8) as char).collect();
    n.subscribe("dev", &format!("https://web.push.apple.com/{token}"), UA_PUBLIC, AUTH, None).unwrap();
    let r = n
        .notify(&Notification {
            kind: NotifyKind::Test,
            host: "live".into(),
            session: None,
            session_title: None,
            title: "t".into(),
            body: "b".into(),
            url: None,
            tag: None,
        })
        .await;
    format!("{:#}", r[0].1.as_ref().expect_err("a random token cannot be delivered"))
}

#[tokio::test]
#[ignore = "network: talks to web.push.apple.com"]
async fn apple_accepts_our_vapid_jwt() {
    for (app, subject) in [(Some("https://relay.example.com"), None), (None, None), (Some("https://localhost:2097"), None)] {
        let err = send(app, subject).await;
        assert!(err.contains("BadWebPushToken"), "app={app:?}: JWT should be accepted, got {err}");
    }
    let err = send(None, Some("mailto:yonder@localhost")).await;
    assert!(err.contains("BadJwtToken"), "placeholder contact should be rejected, got {err}");
}
