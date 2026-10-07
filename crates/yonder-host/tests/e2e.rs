//! End-to-end: relay (in process) + `yonder daemon` (real binary, temp dirs) + a device
//! client speaking Noise through the relay. Uses a fake pi agent (node) so no LLM is needed.
//!
//! Run: `cargo test -p yonder-host --test e2e -- --nocapture`

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use yonder_host::chatlog::{ChatEv, ChatState};
use yonder_host::control::ControlClient;
use yonder_host::device::DeviceClient;
use yonder_proto::app::{
    AgentKind, ApprovalMode, ChatItemKind, ChatStatus, ClientMsg, Event, ItemStatus, Request, Response, SessionKind, SessionSpec,
    SessionState, SubagentStatus,
};
use yonder_proto::keys::Keypair;
use yonder_relay::{tls, AppState, Hub, Limits, RelayConfig};

fn yonder_bin() -> PathBuf {
    // Integration tests of yonder-host do not build other packages' binaries: build it.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new(env!("CARGO"))
        .args(["build", "-q", "-p", "yonder-cli"])
        .current_dir(&root)
        .status()
        .expect("cargo build yonder-cli");
    assert!(status.success());
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
    let exe = if cfg!(windows) { "yonder.exe" } else { "yonder" };
    target.join("debug").join(exe)
}

async fn start_relay() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = RelayConfig::new(Keypair::generate().unwrap());
    let state = Arc::new(AppState { hub: Hub::new(Limits::default()), cfg });
    tokio::spawn(async move { yonder_relay::serve_with_state(tls::plain(listener).unwrap(), state).await });
    format!("ws://{addr}/v1/ws")
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    data: PathBuf,
    work: PathBuf,
    bin: PathBuf,
}

impl Env {
    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env("YONDER_CONFIG_DIR", &self.config)
            .env("YONDER_DATA_DIR", &self.data)
            .env("YONDER_PRIVATE_TRASH", "1")
            .env("YONDER_LINGER_SECS", "30")
            .env("YONDER_LOG_STDERR", "1")
            .env("YONDER_LOG", "info,yonder_host=debug");
        c
    }

    fn start_daemon(&self) -> Daemon {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.data.join("test-daemon.log")).unwrap();
        let child = self
            .cmd()
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        Daemon { child }
    }

    fn socket(&self) -> String {
        std::env::set_var("YONDER_CONFIG_DIR", &self.config);
        std::env::set_var("YONDER_DATA_DIR", &self.data);
        yonder_host::Paths::from_env().unwrap().control_socket()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.data.join("test-daemon.log")).unwrap_or_default()
    }
}

async fn control(env: &Env) -> ControlClient {
    let sock = env.socket();
    for _ in 0..200 {
        if let Ok(c) = ControlClient::connect(&sock).await {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon did not start; log:\n{}", env.log());
}

async fn wait_relay_connected(c: &mut ControlClient, env: &Env) {
    for _ in 0..200 {
        if let Ok(Response::Status { status }) = c.request(Request::Status).await {
            if status.relay_connected {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon never connected to the relay; log:\n{}", env.log());
}

fn fake_pi() -> Vec<String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-pi.mjs");
    vec!["node".into(), script.to_string_lossy().into_owned()]
}

fn fake_codex() -> Vec<String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-codex.mjs");
    vec!["node".into(), script.to_string_lossy().into_owned()]
}

/// Terminal output collected from attach + pty_output, honoring offsets.
struct Term {
    text: Vec<u8>,
    offset: u64,
}

impl Term {
    fn apply(&mut self, session: &str, ev: &Event) {
        match ev {
            Event::PtyOutput { session: s, offset, data } if s == session => {
                let b = B64.decode(data).unwrap();
                let end = offset + b.len() as u64;
                assert!(*offset <= self.offset, "gap in terminal stream: have {}, got {}", self.offset, offset);
                if end > self.offset {
                    let skip = (self.offset - offset) as usize;
                    self.text.extend_from_slice(&b[skip..]);
                    self.offset = end;
                }
            }
            Event::PtySnapshot { session: s, snapshot } if s == session => {
                self.text = B64.decode(&snapshot.data).unwrap();
                self.offset = snapshot.offset;
            }
            _ => {}
        }
    }
    fn has(&self, needle: &str) -> bool {
        String::from_utf8_lossy(&self.text).contains(needle)
    }
}

async fn term_until(dev: &DeviceClient, term: &mut Term, session: &str, needle: &str) {
    let r = dev
        .wait_event(Duration::from_secs(20), |ev| {
            term.apply(session, ev);
            term.has(needle).then_some(())
        })
        .await;
    if r.is_err() {
        panic!("terminal never showed {needle:?}; got:\n{}", String::from_utf8_lossy(&term.text));
    }
}

/// Chat view maintained exactly like a client: snapshot, then seq-ordered events.
async fn chat_until(dev: &DeviceClient, st: &mut ChatState, session: &str, what: &str, mut done: impl FnMut(&ChatState) -> bool) {
    if done(st) {
        return;
    }
    let r = dev
        .wait_event(Duration::from_secs(30), |ev| {
            if let Event::ChatSnapshot { session: s, snapshot } = ev {
                if s == session {
                    *st = ChatState::from_snapshot(snapshot);
                }
            } else if let Some((s, seq, ch)) = ChatEv::from_event(ev) {
                if s == session && seq > st.seq {
                    assert_eq!(seq, st.seq + 1, "chat seq gap");
                    st.apply(seq, &ch);
                }
            }
            done(st).then_some(())
        })
        .await;
    if r.is_err() {
        panic!("chat never reached: {what}; status {:?}, items {:#?}", st.status, st.items());
    }
}

fn agent_texts(st: &ChatState) -> Vec<String> {
    st.items().iter().filter(|i| i.kind == ChatItemKind::Agent).filter_map(|i| i.text.clone()).collect()
}

fn shell_script() -> (Vec<String>, &'static str) {
    if cfg!(windows) {
        (
            vec!["cmd.exe".into(), "/d".into(), "/q".into(), "/k".into(), "echo READY-yonder".into()],
            "\r\n",
        )
    } else {
        (vec!["/bin/sh".into(), "-c".into(), "echo READY-yonder; exec /bin/sh".into()], "\n")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_flow_through_relay() {
    let bin = yonder_bin();
    let tmp = tempfile::tempdir().unwrap();
    let env = Env {
        config: tmp.path().join("config"),
        data: tmp.path().join("data"),
        work: tmp.path().join("work"),
        _tmp: tmp,
        bin,
    };
    std::fs::create_dir_all(&env.work).unwrap();
    std::fs::write(env.work.join("hello.txt"), b"hello from host\n").unwrap();
    let relay_url = start_relay().await;

    // Configure: relay, file root = work dir, fake pi.
    let st = env
        .cmd()
        .args(["init", "--name", "e2e-host", "--relay", &relay_url, "--root"])
        .arg(&env.work)
        .output()
        .unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    let cfg_path = env.config.join("config.toml");
    let mut cfg = std::fs::read_to_string(&cfg_path).unwrap();
    cfg.push_str(&format!(
        "\n[agents.pi]\nprogram = {}\n\n[agents.codex]\nprogram = {}\n",
        toml_array(&fake_pi()),
        toml_array(&fake_codex())
    ));
    cfg = cfg.replace("login_shell = true", "login_shell = false");
    std::fs::write(&cfg_path, cfg).unwrap();

    let mut daemon = env.start_daemon();
    let mut ctl = control(&env).await;
    wait_relay_connected(&mut ctl, &env).await;
    let Response::Status { status } = ctl.request(Request::Status).await.unwrap() else { panic!() };
    let host_pub = yonder_proto::keys::PublicKey::from_b64(&status.host_pub).unwrap();

    // ---- pairing
    let device = Keypair::generate().unwrap();
    let stranger = Keypair::generate().unwrap();
    let err = DeviceClient::connect(&relay_url, &stranger, &host_pub, "stranger", None).await.err().expect("unpaired must fail");
    assert!(err.to_string().contains("not_paired"), "{err:#}");
    let err = DeviceClient::connect(&relay_url, &stranger, &host_pub, "stranger", Some("bogus")).await.err().unwrap();
    assert!(err.to_string().contains("pair_token_invalid"), "{err:#}");

    let Response::Pairing { payload, url } = ctl
        .request(Request::CreatePairing { permissions: vec![], ttl_secs: Some(300) })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(url.contains("#pair="));
    assert_eq!(payload.relay, relay_url);
    let dev = DeviceClient::connect(&relay_url, &device, &host_pub, "test-phone", Some(&payload.token)).await.unwrap();
    assert!(dev.host_hello.ok);
    assert_eq!(dev.host_hello.host_name, "e2e-host");
    // One-time token.
    let err = DeviceClient::connect(&relay_url, &stranger, &host_pub, "stranger", Some(&payload.token)).await.err().unwrap();
    assert!(err.to_string().contains("pair_token_invalid"), "{err:#}");
    // Re-connect without token works now.
    drop(dev);
    let dev = DeviceClient::connect(&relay_url, &device, &host_pub, "test-phone", None).await.unwrap();

    let Response::HostInfo { info } = dev.request(Request::HostInfo).await.unwrap() else { panic!() };
    assert_eq!(info.name, "e2e-host");
    let pi = info.agents.iter().find(|a| a.agent == AgentKind::Pi).unwrap();
    assert!(pi.available && pi.chat, "{pi:?}");
    assert!(info.vapid_public.is_some());

    // ---- terminal session
    let (argv, nl) = shell_script();
    let spec = SessionSpec {
        kind: Some(SessionKind::Terminal),
        agent: Some(AgentKind::Custom),
        command: Some(argv),
        cwd: Some(env.work.to_string_lossy().into_owned()),
        cols: Some(100),
        rows: Some(30),
        ..Default::default()
    };
    let Response::Session { session: term_s } = dev.request(Request::CreateSession { spec }).await.unwrap() else { panic!() };
    let tid = term_s.id.clone();
    let Response::Attached { terminal: Some(snap), .. } = dev.request(Request::Attach { session: tid.clone(), since: None }).await.unwrap() else {
        panic!()
    };
    let mut term = Term { text: B64.decode(&snap.data).unwrap(), offset: snap.offset };
    term_until(&dev, &mut term, &tid, "READY-yonder").await;
    dev.send(&ClientMsg::Resize { session: tid.clone(), cols: 120, rows: 40 }).await.unwrap();
    dev.wait_event(Duration::from_secs(10), |ev| matches!(ev, Event::PtyResized { cols: 120, rows: 40, .. }).then_some(()))
        .await
        .unwrap();
    let input = format!("echo out-$((6*7)){nl}");
    let input = if cfg!(windows) { format!("echo out-42{nl}") } else { input };
    dev.send(&ClientMsg::Input { session: tid.clone(), data: B64.encode(input.as_bytes()) }).await.unwrap();
    term_until(&dev, &mut term, &tid, "out-42").await;

    // Re-attach incrementally from our offset: nothing is lost or duplicated.
    let Response::Attached { terminal: Some(snap2), .. } =
        dev.request(Request::Attach { session: tid.clone(), since: Some(term.offset) }).await.unwrap()
    else {
        panic!()
    };
    assert!(!snap2.reset);
    assert!(snap2.offset >= term.offset);

    // ---- chat session (fake pi) with approval, interrupt and attachments
    let spec = SessionSpec {
        kind: Some(SessionKind::Chat),
        agent: Some(AgentKind::Pi),
        cwd: Some(env.work.to_string_lossy().into_owned()),
        approval: Some(ApprovalMode::Ask),
        prompt: Some("hello world".into()),
        ..Default::default()
    };
    let Response::Session { session: chat_s } = dev.request(Request::CreateSession { spec }).await.unwrap() else { panic!() };
    let cid = chat_s.id.clone();
    assert_eq!(chat_s.title, "hello world");
    let Response::Attached { chat: Some(csnap), .. } = dev.request(Request::Attach { session: cid.clone(), since: None }).await.unwrap() else {
        panic!()
    };
    let mut chat = ChatState::from_snapshot(&csnap);
    chat_until(&dev, &mut chat, &cid, "first reply", |st| {
        agent_texts(st).iter().any(|t| t == "echo: hello world") && st.status == ChatStatus::Idle
    })
    .await;
    assert!(chat.items().iter().any(|i| i.kind == ChatItemKind::User && i.text.as_deref() == Some("hello world")));
    assert!(chat.items().iter().any(|i| i.kind == ChatItemKind::Command && i.title.as_deref() == Some("echo fake")));

    // Approval: ask -> approval_requested -> respond allow -> "approved".
    dev.request(Request::ChatSend { session: cid.clone(), text: "ask please".into(), attachments: vec![] }).await.unwrap();
    chat_until(&dev, &mut chat, &cid, "approval pending", |st| {
        !st.approvals.is_empty() && st.status == ChatStatus::AwaitingApproval
    })
    .await;
    let approval = chat.approvals[0].clone();
    let Response::Sessions { sessions } = dev.request(Request::ListSessions).await.unwrap() else { panic!() };
    assert_eq!(sessions.iter().find(|s| s.id == cid).unwrap().pending_approvals, 1);
    let allow = approval.options.iter().find(|o| o.id == "allow").unwrap();
    dev.request(Request::ApprovalRespond { session: cid.clone(), approval: approval.id.clone(), option: allow.id.clone() })
        .await
        .unwrap();
    chat_until(&dev, &mut chat, &cid, "approved reply", |st| {
        st.approvals.is_empty() && agent_texts(st).iter().any(|t| t == "approved") && st.status == ChatStatus::Idle
    })
    .await;
    // A stale approval id is rejected.
    assert!(dev
        .request(Request::ApprovalRespond { session: cid.clone(), approval: approval.id.clone(), option: "allow".into() })
        .await
        .is_err());

    // Interrupt a streaming turn.
    dev.request(Request::ChatSend { session: cid.clone(), text: "slow story".into(), attachments: vec![] }).await.unwrap();
    chat_until(&dev, &mut chat, &cid, "streaming", |st| {
        st.status == ChatStatus::Working && st.items().iter().any(|i| i.text.as_deref().unwrap_or("").contains("chunk2"))
    })
    .await;
    dev.request(Request::ChatInterrupt { session: cid.clone() }).await.unwrap();
    chat_until(&dev, &mut chat, &cid, "interrupted", |st| st.status == ChatStatus::Idle).await;
    assert!(!chat.items().iter().any(|i| i.text.as_deref().unwrap_or("").contains("chunk39")));

    // Attachment via upload_temp.
    let Response::Path { path: up } =
        dev.request(Request::UploadTemp { name: "note.txt".into(), data: B64.encode(b"attached!") }).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(std::fs::read(&up).unwrap(), b"attached!");
    dev.request(Request::ChatSend { session: cid.clone(), text: "with file".into(), attachments: vec![up.clone()] })
        .await
        .unwrap();
    chat_until(&dev, &mut chat, &cid, "reply with attachment", |st| {
        agent_texts(st).iter().any(|t| t.starts_with("echo: with file") && t.contains("note.txt"))
    })
    .await;
    // pi has no approval modes.
    let e = dev.request(Request::SetApprovalMode { session: cid.clone(), mode: ApprovalMode::Yolo }).await.unwrap_err();
    assert!(e.to_string().starts_with("unsupported"), "{e:#}");
    // Attachments outside the allowed roots are refused.
    assert!(dev
        .request(Request::ChatSend { session: cid.clone(), text: "x".into(), attachments: vec![env.config.join("host.key").to_string_lossy().into_owned()] })
        .await
        .is_err());

    // ---- approval modes (fake codex): switch a running chat to full access and back
    let spec = SessionSpec {
        kind: Some(SessionKind::Chat),
        agent: Some(AgentKind::Codex),
        cwd: Some(env.work.to_string_lossy().into_owned()),
        approval: Some(ApprovalMode::Ask),
        ..Default::default()
    };
    let Response::Session { session: cx } = dev.request(Request::CreateSession { spec }).await.unwrap() else { panic!() };
    let xid = cx.id.clone();
    assert_eq!(cx.approval, Some(ApprovalMode::Ask));
    let Response::Attached { chat: Some(xsnap), session: xs, .. } = dev.request(Request::Attach { session: xid.clone(), since: None }).await.unwrap()
    else {
        panic!()
    };
    assert!(xs.approval_live, "{xs:?}");
    let mut xchat = ChatState::from_snapshot(&xsnap);
    // A pre-capability-negotiation client still completes the Noise handshake, but receives
    // only the legacy-compatible projection of sub-agent events.
    let legacy = DeviceClient::connect_with_features(&relay_url, &device, &host_pub, "old-client", None, None).await.unwrap();
    let Response::Attached { chat: Some(legacy_snap), .. } =
        legacy.request(Request::Attach { session: xid.clone(), since: None }).await.unwrap()
    else {
        panic!()
    };
    assert!(legacy_snap.items.iter().all(|i| i.thread.is_none() && i.subagent.is_none()));
    dev.request(Request::ChatSend { session: xid.clone(), text: "run touch a".into(), attachments: vec![] }).await.unwrap();
    chat_until(&dev, &mut xchat, &xid, "codex approval pending", |st| !st.approvals.is_empty()).await;
    // Switching to full access answers the pending approval.
    let Response::Session { session: after } =
        dev.request(Request::SetApprovalMode { session: xid.clone(), mode: ApprovalMode::Yolo }).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(after.approval, Some(ApprovalMode::Yolo));
    chat_until(&dev, &mut xchat, &xid, "pending approval accepted", |st| {
        st.approvals.is_empty() && agent_texts(st).iter().any(|t| t.starts_with("ran touch a after approval"))
    })
    .await;
    // The next turn runs without asking, in full access.
    dev.request(Request::ChatSend { session: xid.clone(), text: "run touch b".into(), attachments: vec![] }).await.unwrap();
    chat_until(&dev, &mut xchat, &xid, "ran without asking", |st| {
        agent_texts(st).iter().any(|t| t.starts_with("ran touch b without asking") && t.contains("sandbox=danger-full-access"))
    })
    .await;
    assert!(xchat.approvals.is_empty());
    // The list shows the mode; it survives in meta.json.
    let Response::Sessions { sessions } = dev.request(Request::ListSessions).await.unwrap() else { panic!() };
    assert_eq!(sessions.iter().find(|s| s.id == xid).unwrap().approval, Some(ApprovalMode::Yolo));
    // Back to asking.
    dev.request(Request::SetApprovalMode { session: xid.clone(), mode: ApprovalMode::Ask }).await.unwrap();
    dev.request(Request::ChatSend { session: xid.clone(), text: "run touch c".into(), attachments: vec![] }).await.unwrap();
    chat_until(&dev, &mut xchat, &xid, "asks again", |st| !st.approvals.is_empty()).await;
    let deny = xchat.approvals[0].options.iter().find(|o| o.id == "deny").unwrap().id.clone();
    dev.request(Request::ApprovalRespond { session: xid.clone(), approval: xchat.approvals[0].id.clone(), option: deny }).await.unwrap();
    chat_until(&dev, &mut xchat, &xid, "denied", |st| agent_texts(st).iter().any(|t| t.starts_with("skipped touch c"))).await;
    let audit = std::fs::read_to_string(env.config.join("audit.log")).unwrap();
    assert!(audit.contains("\"op\":\"approval_mode\""), "{audit}");
    // Without a mode, a new chat follows the agent's configured default (none for this fake: ask).
    let spec = SessionSpec { kind: Some(SessionKind::Chat), agent: Some(AgentKind::Codex), cwd: Some(env.work.to_string_lossy().into_owned()), ..Default::default() };
    let Response::Session { session: cy } = dev.request(Request::CreateSession { spec }).await.unwrap() else { panic!() };
    assert_eq!(cy.approval, Some(ApprovalMode::Ask));
    dev.request(Request::Kill { session: cy.id.clone() }).await.unwrap();

    // ---- sub-agents (fake codex): a card in the chat, its thread on request, its approval here
    dev.request(Request::SetApprovalMode { session: xid.clone(), mode: ApprovalMode::Ask }).await.unwrap();
    dev.request(Request::ChatSend { session: xid.clone(), text: "spawn touch sub".into(), attachments: vec![] }).await.unwrap();
    let legacy_item = legacy
        .wait_event(Duration::from_secs(20), |ev| {
            match ev {
                Event::ChatItem { session, item, .. } if *session == xid => {
                    assert!(item.thread.is_none());
                    assert!(item.subagent.is_none());
                    (item.kind == ChatItemKind::Tool).then_some(item.clone())
                }
                Event::ChatDelta { session, thread, .. } if *session == xid => {
                    assert!(thread.is_none(), "legacy client received a thread delta");
                    None
                }
                Event::ApprovalRequested { session, approval, .. } if *session == xid => {
                    assert!(approval.thread.is_none());
                    assert!(approval.thread_name.is_none());
                    None
                }
                _ => None,
            }
        })
        .await
        .expect("legacy client should receive a downgraded sub-agent card");
    assert!(legacy_item.thread.is_none());
    assert!(legacy_item.subagent.is_none());
    legacy
        .wait_event(Duration::from_secs(20), |ev| match ev {
            Event::ApprovalRequested { session, approval, .. } if *session == xid => {
                assert!(approval.thread.is_none());
                assert!(approval.thread_name.is_none());
                Some(())
            }
            Event::ChatItem { session, item, .. } if *session == xid => {
                assert!(item.thread.is_none());
                assert!(item.subagent.is_none());
                None
            }
            Event::ChatDelta { session, thread, .. } if *session == xid => {
                assert!(thread.is_none(), "legacy client received a thread delta");
                None
            }
            _ => None,
        })
        .await
        .expect("legacy client should receive an answerable un-attributed approval");
    let card_of = |st: &ChatState| st.items().iter().find(|i| i.kind == ChatItemKind::Subagent).cloned();
    chat_until(&dev, &mut xchat, &xid, "sub-agent card named", |st| {
        card_of(st).and_then(|c| c.subagent).is_some_and(|s| s.name.as_deref() == Some("Fakey") && s.status == SubagentStatus::Running)
    })
    .await;
    let card = card_of(&xchat).unwrap();
    let kid = card.subagent.as_ref().unwrap().id.clone();
    assert!(card.text.as_deref().unwrap().contains("touch sub"));
    // Its approval reaches this chat, attributed to it.
    chat_until(&dev, &mut xchat, &xid, "sub-agent approval", |st| !st.approvals.is_empty()).await;
    let a = xchat.approvals[0].clone();
    assert_eq!(a.thread.as_deref(), Some(kid.as_str()));
    assert_eq!(a.thread_name.as_deref(), Some("Fakey"));
    // The chat shows none of the sub-agent's own items; its thread has them.
    assert!(xchat.items().iter().all(|i| i.thread.is_none()));
    let Response::ChatThread { items, more, seq } =
        dev.request(Request::ChatThread { session: xid.clone(), thread: kid.clone(), before: None, limit: None }).await.unwrap()
    else {
        panic!()
    };
    assert!(!more);
    assert!(seq > 0);
    assert!(items.iter().all(|i| i.thread.as_deref() == Some(kid.as_str())), "{items:?}");
    assert!(items.iter().any(|i| i.kind == ChatItemKind::User && i.text.as_deref().unwrap_or("").contains("touch sub")), "{items:?}");
    assert!(items.iter().any(|i| i.kind == ChatItemKind::Command && i.status == ItemStatus::InProgress), "{items:?}");
    // Live items of the thread stream to attached clients with `thread` set.
    let allow = a.options.iter().find(|o| o.id == "allow").unwrap().id.clone();
    dev.request(Request::ApprovalRespond { session: xid.clone(), approval: a.id.clone(), option: allow }).await.unwrap();
    let live = dev
        .wait_event(Duration::from_secs(20), |ev| {
            // Keep the chat view in step while looking for the delta.
            if let Some((s, seq, ch)) = ChatEv::from_event(ev) {
                if s == xid && seq > xchat.seq {
                    assert_eq!(seq, xchat.seq + 1, "chat seq gap");
                    xchat.apply(seq, &ch);
                }
            }
            match ev {
                Event::ChatDelta { session, thread: Some(t), delta, .. } if *session == xid && *t == kid => Some(delta.clone()),
                _ => None,
            }
        })
        .await
        .unwrap();
    assert_eq!(live, "DONE");
    chat_until(&dev, &mut xchat, &xid, "sub-agent closed with its reply", |st| {
        st.approvals.is_empty()
            && card_of(st).and_then(|c| c.subagent).is_some_and(|s| s.status == SubagentStatus::Closed && s.reply.as_deref() == Some("DONE"))
            && agent_texts(st).iter().any(|t| t.starts_with("sub-agent said DONE"))
    })
    .await;
    assert_eq!(xchat.items().iter().filter(|i| i.kind == ChatItemKind::Subagent).count(), 1);
    let Response::ChatThread { items, .. } =
        dev.request(Request::ChatThread { session: xid.clone(), thread: kid.clone(), before: None, limit: Some(50) }).await.unwrap()
    else {
        panic!()
    };
    assert!(items.iter().any(|i| i.kind == ChatItemKind::Command && i.status == ItemStatus::Completed), "{items:?}");
    assert!(items.iter().any(|i| i.kind == ChatItemKind::Agent && i.text.as_deref() == Some("DONE")), "{items:?}");
    // Unknown thread: empty, not an error.
    let Response::ChatThread { items, .. } =
        dev.request(Request::ChatThread { session: xid.clone(), thread: "nope".into(), before: None, limit: None }).await.unwrap()
    else {
        panic!()
    };
    assert!(items.is_empty());
    dev.request(Request::Kill { session: xid.clone() }).await.unwrap();
    // After the chat ended the thread is read from the log.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let Response::ChatThread { items, .. } =
        dev.request(Request::ChatThread { session: xid.clone(), thread: kid.clone(), before: None, limit: None }).await.unwrap()
    else {
        panic!()
    };
    assert!(items.iter().any(|i| i.kind == ChatItemKind::Agent && i.text.as_deref() == Some("DONE")), "{items:?}");
    drop(legacy);

    // ---- files
    let root = dunce_canon(&env.work);
    let Response::Dir { listing } = dev.request(Request::FsList { path: root.clone(), hidden: false }).await.unwrap() else { panic!() };
    assert!(listing.entries.iter().any(|e| e.name == "hello.txt"));
    let hello = listing.entries.iter().find(|e| e.name == "hello.txt").unwrap().path.clone();
    let Response::FileChunk { data, eof, size, .. } = dev.request(Request::FsRead { path: hello.clone(), offset: 0, len: 1 << 20 }).await.unwrap()
    else {
        panic!()
    };
    assert!(eof);
    assert_eq!(size, 16);
    assert_eq!(B64.decode(data).unwrap(), b"hello from host\n");
    // Chunked upload (3 chunks).
    let big: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
    let target = format!("{root}{}up.bin", std::path::MAIN_SEPARATOR);
    let mut off = 0usize;
    for chunk in big.chunks(256 * 1024) {
        let finish = off + chunk.len() == big.len();
        dev.request(Request::FsWrite { path: target.clone(), offset: off as u64, data: B64.encode(chunk), finish, overwrite: false })
            .await
            .unwrap();
        off += chunk.len();
    }
    assert_eq!(std::fs::read(env.work.join("up.bin")).unwrap(), big);
    // Existing target without overwrite -> exists.
    let e = dev
        .request(Request::FsWrite { path: target.clone(), offset: 0, data: B64.encode(b"x"), finish: true, overwrite: false })
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("exists"), "{e:#}");
    // Download in chunks.
    let mut got = Vec::new();
    loop {
        let Response::FileChunk { data, eof, .. } =
            dev.request(Request::FsRead { path: target.clone(), offset: got.len() as u64, len: 300_000 }).await.unwrap()
        else {
            panic!()
        };
        got.extend(B64.decode(data).unwrap());
        if eof {
            break;
        }
    }
    assert_eq!(got, big);
    let dir2 = format!("{root}{}sub", std::path::MAIN_SEPARATOR);
    dev.request(Request::FsMkdir { path: dir2.clone() }).await.unwrap();
    let moved = format!("{dir2}{}moved.bin", std::path::MAIN_SEPARATOR);
    dev.request(Request::FsRename { from: target.clone(), to: moved.clone(), overwrite: false }).await.unwrap();
    dev.request(Request::FsDelete { path: moved.clone() }).await.unwrap();
    assert!(!env.work.join("sub/moved.bin").exists());
    // Outside the roots.
    let outside = env.config.join("host.key").to_string_lossy().into_owned();
    let e = dev.request(Request::FsRead { path: outside, offset: 0, len: 10 }).await.unwrap_err();
    assert!(e.to_string().starts_with("forbidden"), "{e:#}");
    let audit = std::fs::read_to_string(env.config.join("audit.log")).unwrap();
    assert!(audit.contains("\"op\":\"delete\""), "{audit}");

    // ---- daemon restart: sessions survive
    let term_offset = term.offset;
    drop(dev);
    drop(ctl);
    daemon.child.kill().unwrap();
    let _ = daemon.child.wait();
    let _daemon2 = env.start_daemon();
    let mut ctl = control(&env).await;
    wait_relay_connected(&mut ctl, &env).await;
    let dev = DeviceClient::connect(&relay_url, &device, &host_pub, "test-phone", None).await.unwrap();
    let Response::Sessions { sessions } = dev.request(Request::ListSessions).await.unwrap() else { panic!() };
    let t = sessions.iter().find(|s| s.id == tid).expect("terminal session listed after restart");
    assert_eq!(t.state, SessionState::Running, "{t:?}");
    let c = sessions.iter().find(|s| s.id == cid).expect("chat session listed after restart");
    assert_eq!(c.state, SessionState::Running, "{c:?}");
    // Terminal continues where we left off.
    let Response::Attached { terminal: Some(snap3), .. } =
        dev.request(Request::Attach { session: tid.clone(), since: Some(term_offset) }).await.unwrap()
    else {
        panic!()
    };
    let mut term = Term { text: B64.decode(&snap3.data).unwrap(), offset: snap3.offset };
    let input = if cfg!(windows) { format!("echo after-restart{nl}") } else { format!("echo after-$((1+1))-restart{nl}") };
    dev.send(&ClientMsg::Input { session: tid.clone(), data: B64.encode(input.as_bytes()) }).await.unwrap();
    let want = if cfg!(windows) { "after-restart" } else { "after-2-restart" };
    term_until(&dev, &mut term, &tid, want).await;
    // Chat continues and keeps its history.
    let Response::Attached { chat: Some(csnap), .. } = dev.request(Request::Attach { session: cid.clone(), since: None }).await.unwrap() else {
        panic!()
    };
    let mut chat = ChatState::from_snapshot(&csnap);
    assert!(agent_texts(&chat).iter().any(|t| t == "approved"), "history kept across daemon restart");
    dev.request(Request::ChatSend { session: cid.clone(), text: "still there".into(), attachments: vec![] }).await.unwrap();
    chat_until(&dev, &mut chat, &cid, "reply after restart", |st| agent_texts(st).iter().any(|t| t == "echo: still there")).await;

    // ---- kill, remove
    dev.request(Request::Kill { session: tid.clone() }).await.unwrap();
    dev.wait_event(Duration::from_secs(20), |ev| match ev {
        Event::SessionUpdated { session } if session.id == tid && session.state == SessionState::Exited => Some(()),
        _ => None,
    })
    .await
    .unwrap();
    // Exited sessions can still be attached (replay).
    let Response::Attached { terminal: Some(replay), .. } = dev.request(Request::Attach { session: tid.clone(), since: None }).await.unwrap() else {
        panic!()
    };
    assert!(String::from_utf8_lossy(&B64.decode(replay.data).unwrap()).contains(want));
    dev.request(Request::Remove { session: tid.clone() }).await.unwrap();
    let Response::Sessions { sessions } = dev.request(Request::ListSessions).await.unwrap() else { panic!() };
    assert!(!sessions.iter().any(|s| s.id == tid));
    dev.request(Request::Kill { session: cid.clone() }).await.unwrap();
    dev.wait_event(Duration::from_secs(20), |ev| match ev {
        Event::SessionUpdated { session } if session.id == cid && session.state == SessionState::Exited => Some(()),
        _ => None,
    })
    .await
    .unwrap();

    // ---- devices + revoke
    let Response::Devices { devices } = dev.request(Request::ListDevices).await.unwrap() else { panic!() };
    assert_eq!(devices.len(), 1);
    assert!(devices[0].current);
    ctl.request(Request::RevokeDevice { device: device.public.to_b64() }).await.unwrap();
    // The open link is closed and new connections are refused.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(dev.request_timeout(Request::Ping, Duration::from_secs(3)).await.is_err());
    let err = DeviceClient::connect(&relay_url, &device, &host_pub, "test-phone", None).await.err().unwrap();
    assert!(err.to_string().contains("not_paired"), "{err:#}");

    ctl.request(Request::Shutdown).await.unwrap();
}

fn dunce_canon(p: &Path) -> String {
    let c = std::fs::canonicalize(p).unwrap();
    let s = c.to_string_lossy().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

fn toml_array(v: &[String]) -> String {
    let items: Vec<String> = v.iter().map(|s| format!("'{s}'")).collect();
    format!("[{}]", items.join(", "))
}
