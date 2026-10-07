//! Live tests against the real agent CLIs. Run with:
//! `cargo test -p yonder-agents --test live -- --ignored --nocapture --test-threads 1`

use std::time::Duration;

use yonder_agents::{spawn_adapter, AdapterCmd, AdapterEvent, AgentLaunch};
use yonder_proto::app::{AgentKind, ApprovalMode, ChatItemKind, ChatStatus, ItemStatus};

fn launch(agent: AgentKind, dir: &std::path::Path, approval: ApprovalMode) -> AgentLaunch {
    AgentLaunch {
        agent,
        cwd: dir.to_path_buf(),
        // Keep live Claude runs cheap.
        model: (agent == AgentKind::Claude).then(|| "haiku".to_string()),
        approval,
        resume: None,
        env: Default::default(),
        program: None,
        login_shell: false,
    }
}

/// Wait for the first Idle status after work started (turn done); approve every approval.
async fn run_turn(h: &mut yonder_agents::AdapterHandle, text: &str, approve: Option<&str>) -> (Vec<AdapterEvent>, Option<String>) {
    h.cmds.send(AdapterCmd::Send { text: text.into(), attachments: vec![] }).await.unwrap();
    let mut evs = Vec::new();
    let mut worked = false;
    let mut session = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let ev = tokio::time::timeout_at(deadline, h.events.recv()).await.expect("turn timed out").expect("adapter ended");
        match &ev {
            AdapterEvent::AgentSession(s) => session = Some(s.clone()),
            AdapterEvent::ApprovalRequested(a) => {
                if let Some(opt) = approve {
                    h.cmds.send(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: opt.into() }).await.unwrap();
                }
            }
            AdapterEvent::Status { status: ChatStatus::Working, .. } => worked = true,
            AdapterEvent::Status { status: ChatStatus::Idle, .. } if worked => {
                evs.push(ev);
                break;
            }
            AdapterEvent::Exited { error, .. } => panic!("agent exited: {error:?}"),
            _ => {}
        }
        evs.push(ev);
    }
    (evs, session)
}

fn agent_text(evs: &[AdapterEvent]) -> String {
    let mut texts: std::collections::HashMap<String, String> = Default::default();
    for e in evs {
        match e {
            AdapterEvent::Item(i) if i.kind == ChatItemKind::Agent => {
                texts.insert(i.id.clone(), i.text.clone().unwrap_or_default());
            }
            AdapterEvent::Delta { item, delta, .. } => {
                if let Some(t) = texts.get_mut(item) {
                    t.push_str(delta);
                }
            }
            _ => {}
        }
    }
    texts.into_values().collect::<Vec<_>>().join("\n")
}

async fn pong(agent: AgentKind) {
    let dir = tempfile::tempdir().unwrap();
    let mut h = spawn_adapter(launch(agent, dir.path(), ApprovalMode::Ask)).unwrap();
    let (evs, _) = run_turn(&mut h, "Reply with exactly: PONG", None).await;
    let text = agent_text(&evs);
    println!("{agent:?}: {text}");
    assert!(text.contains("PONG"), "{agent:?} said {text:?}");
    h.cmds.send(AdapterCmd::Shutdown).await.unwrap();
    let mut exited = false;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(10), h.events.recv()).await {
        if matches!(ev, AdapterEvent::Exited { .. }) {
            exited = true;
            break;
        }
    }
    assert!(exited, "{agent:?} did not exit on shutdown");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_codex_pong() {
    pong(AgentKind::Codex).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_claude_pong() {
    pong(AgentKind::Claude).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_pi_pong() {
    pong(AgentKind::Pi).await;
}

/// Claude in ask mode must request approval for a shell command; approving runs it.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_claude_approval() {
    let dir = tempfile::tempdir().unwrap();
    // The user's settings may allow Bash(*); force an ask rule for this run.
    let mut l = launch(AgentKind::Claude, dir.path(), ApprovalMode::Ask);
    l.program = Some(vec!["claude".into(), "--settings".into(), r#"{"permissions":{"ask":["Bash"]}}"#.into()]);
    let mut h = spawn_adapter(l).unwrap();
    let (evs, _) = run_turn(&mut h, "Run this shell command: touch yonder_ok.txt", Some("allow")).await;
    assert!(evs.iter().any(|e| matches!(e, AdapterEvent::ApprovalRequested(_))), "no approval requested");
    assert!(dir.path().join("yonder_ok.txt").exists(), "file not created");
    let done = evs.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.kind == ChatItemKind::Command && i.status == ItemStatus::Completed));
    assert!(done);
    h.cmds.send(AdapterCmd::Shutdown).await.unwrap();
}

/// Codex in ask mode: a write outside the sandbox needs approval; approving runs it.
/// The target lives under home: the workspace-write sandbox allows the cwd and the temp dirs.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_codex_approval() {
    let dir = tempfile::tempdir().unwrap();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).expect("home");
    let outside = tempfile::Builder::new().prefix(".yonder-live-").tempdir_in(home).unwrap();
    let target = outside.path().join("yonder_codex_ok.txt");
    let mut h = spawn_adapter(launch(AgentKind::Codex, dir.path(), ApprovalMode::Ask)).unwrap();
    let prompt = format!("Run exactly this shell command and nothing else: touch {}", target.display());
    let (evs, session) = run_turn(&mut h, &prompt, Some("allow")).await;
    println!("codex session {session:?}");
    let asked = evs.iter().any(|e| matches!(e, AdapterEvent::ApprovalRequested(_)));
    assert!(asked, "no approval requested for a write outside the workspace");
    assert!(target.exists(), "file not created");
    h.cmds.send(AdapterCmd::Shutdown).await.unwrap();
}

/// One turn; `on_approval` decides what to do with each approval (None: leave it pending).
async fn run_turn_with(
    h: &mut yonder_agents::AdapterHandle,
    text: &str,
    mut on_approval: impl FnMut(&yonder_proto::app::Approval) -> Option<AdapterCmd>,
) -> Vec<AdapterEvent> {
    h.cmds.send(AdapterCmd::Send { text: text.into(), attachments: vec![] }).await.unwrap();
    let mut evs = Vec::new();
    let mut worked = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let ev = tokio::time::timeout_at(deadline, h.events.recv()).await.expect("turn timed out").expect("adapter ended");
        match &ev {
            AdapterEvent::ApprovalRequested(a) => {
                println!("  approval requested: {} {:?} {:?}", a.id, a.kind, a.command);
                if let Some(cmd) = on_approval(a) {
                    h.cmds.send(cmd).await.unwrap();
                }
            }
            AdapterEvent::ApprovalResolved { approval, option } => println!("  approval resolved: {approval} -> {option}"),
            AdapterEvent::ApprovalMode(m) => println!("  mode now {m:?}"),
            AdapterEvent::Status { status: ChatStatus::Working, .. } => worked = true,
            AdapterEvent::Status { status: ChatStatus::Idle, .. } if worked => {
                evs.push(ev);
                break;
            }
            AdapterEvent::Exited { error, .. } => panic!("agent exited: {error:?}"),
            _ => {}
        }
        evs.push(ev);
    }
    evs
}

fn asked(evs: &[AdapterEvent]) -> usize {
    evs.iter().filter(|e| matches!(e, AdapterEvent::ApprovalRequested(_))).count()
}

/// Codex started in ask mode, switched to full access while an approval waits (no answer from
/// the user): the waiting command runs; the next turn runs outside the workspace without
/// asking; back in ask mode it asks again.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_codex_switch_mode() {
    let dir = tempfile::tempdir().unwrap();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).expect("home");
    let outside = tempfile::Builder::new().prefix(".yonder-live-").tempdir_in(home).unwrap();
    let t = |n: &str| outside.path().join(n);
    let mut h = spawn_adapter(launch(AgentKind::Codex, dir.path(), ApprovalMode::Ask)).unwrap();
    let prompt = |p: &std::path::Path| format!("Run exactly this shell command and nothing else: touch {}", p.display());

    println!("turn 1 (ask; switch to yolo while the approval waits)");
    let mut switched = false;
    let evs = run_turn_with(&mut h, &prompt(&t("a.txt")), |_| {
        (!std::mem::replace(&mut switched, true)).then_some(AdapterCmd::SetApprovalMode(ApprovalMode::Yolo))
    })
    .await;
    assert!(asked(&evs) >= 1, "turn 1 did not ask");
    assert!(evs.iter().any(|e| matches!(e, AdapterEvent::ApprovalResolved { option, .. } if option == "allow")), "not resolved by the switch");
    assert!(t("a.txt").exists(), "a.txt not created");

    println!("turn 2 (yolo)");
    let evs = run_turn_with(&mut h, &prompt(&t("b.txt")), |a| Some(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "deny".into() })).await;
    assert_eq!(asked(&evs), 0, "full access still asked");
    assert!(t("b.txt").exists(), "b.txt not created");

    println!("turn 3 (back to ask; deny)");
    h.cmds.send(AdapterCmd::SetApprovalMode(ApprovalMode::Ask)).await.unwrap();
    let evs = run_turn_with(&mut h, &prompt(&t("c.txt")), |a| Some(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "deny".into() })).await;
    assert!(asked(&evs) >= 1, "ask mode did not ask again");
    assert!(!t("c.txt").exists(), "c.txt created despite the denial");
    h.cmds.send(AdapterCmd::Shutdown).await.unwrap();
}

/// Claude started in ask mode (user settings off, so writes prompt), switched to full access
/// while a Bash prompt waits: it runs; the next Bash runs without a prompt; back to ask, it
/// prompts again. The switch must be confirmed by Claude (no error item, no rollback).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_claude_switch_mode() {
    let dir = tempfile::tempdir().unwrap();
    let mut l = launch(AgentKind::Claude, dir.path(), ApprovalMode::Ask);
    // Without the user's settings (allow rules, default mode); auth comes from the environment.
    l.program = Some(vec!["claude".into(), "--setting-sources".into(), "project,local".into(), "--no-session-persistence".into()]);
    if let Some(env) = claude_env() {
        l.env = env;
    }
    let mut h = spawn_adapter(l).unwrap();
    let t = |n: &str| dir.path().join(n);
    let prompt = |n: &str| format!("Use the Bash tool to run exactly this command and nothing else: touch {n}");
    let rollback = |evs: &[AdapterEvent]| {
        evs.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.kind == ChatItemKind::Error && i.text.as_deref().unwrap_or("").contains("审批模式")))
    };

    println!("turn 1 (ask; switch to yolo while the prompt waits)");
    let mut switched = false;
    let evs = run_turn_with(&mut h, &prompt("a.txt"), |_| {
        (!std::mem::replace(&mut switched, true)).then_some(AdapterCmd::SetApprovalMode(ApprovalMode::Yolo))
    })
    .await;
    assert!(asked(&evs) >= 1, "turn 1 did not prompt");
    assert!(evs.iter().any(|e| matches!(e, AdapterEvent::ApprovalResolved { option, .. } if option == "allow")));
    assert!(t("a.txt").exists(), "a.txt not created");
    assert!(!rollback(&evs), "set_permission_mode refused");

    println!("turn 2 (yolo)");
    let evs = run_turn_with(&mut h, &prompt("b.txt"), |a| Some(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "deny".into() })).await;
    assert_eq!(asked(&evs), 0, "bypassPermissions still prompted");
    assert!(t("b.txt").exists(), "b.txt not created");
    assert!(!rollback(&evs));

    println!("turn 3 (back to ask; deny)");
    h.cmds.send(AdapterCmd::SetApprovalMode(ApprovalMode::Ask)).await.unwrap();
    let evs = run_turn_with(&mut h, &prompt("c.txt"), |a| Some(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "deny".into() })).await;
    assert!(asked(&evs) >= 1, "ask mode did not prompt again");
    assert!(!t("c.txt").exists(), "c.txt created despite the denial");
    assert!(!rollback(&evs));
    h.cmds.send(AdapterCmd::Shutdown).await.unwrap();
}

/// Claude's endpoint settings (`env` in the user's settings.json), which `--setting-sources`
/// without `user` would otherwise drop.
fn claude_env() -> Option<std::collections::BTreeMap<String, String>> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(std::path::Path::new(&home).join(".claude/settings.json")).ok()?).ok()?;
    Some(v.get("env")?.as_object()?.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_history() {
    for agent in [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi] {
        let h = yonder_agents::list_agent_history(agent, None).await.unwrap();
        println!("{agent:?}: {} sessions; first: {:?}", h.len(), h.first().map(|s| (&s.id, &s.title)));
        assert!(!h.is_empty(), "{agent:?} history empty");
    }
    let info = yonder_agents::codex_models().await.unwrap();
    println!("codex models {:?} default {:?} approval {:?}", info.models, info.default_model, info.approval);
    assert!(!info.models.is_empty());
}

/// Real history on this machine: merged listing, search, paging and a preview per agent.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_history_view() {
    use yonder_agents::{agent_preview, list_history, HistoryQuery};
    let t = std::time::Instant::now();
    let p = list_history(&HistoryQuery::default()).await;
    println!("merged first page {} sessions in {:?}, next {:?}, {} folders, errors {:?}", p.sessions.len(), t.elapsed(), p.next_cursor, p.folders.len(), p.errors);
    assert!(!p.sessions.is_empty());
    let all = list_history(&HistoryQuery { limit: Some(200), ..Default::default() }).await;
    let t = std::time::Instant::now();
    let cached = list_history(&HistoryQuery { query: Some("yonder".into()), ..Default::default() }).await;
    println!("search 'yonder' (cached) {} in {:?}", cached.sessions.len(), t.elapsed());
    for agent in [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi] {
        let a = list_history(&HistoryQuery { agent: Some(agent), limit: Some(200), ..Default::default() }).await;
        let every = list_history(&HistoryQuery { agent: Some(agent), limit: Some(200), all: true, ..Default::default() }).await;
        println!("{agent:?}: shown {} (more: {}), with hidden {} (more: {})", a.sessions.len(), a.next_cursor.is_some(), every.sessions.len(), every.next_cursor.is_some());
        if let Some(s) = a.sessions.first() {
            let t = std::time::Instant::now();
            let (items, truncated) = agent_preview(agent, &s.id).await.unwrap();
            println!("  preview {} items (truncated {truncated}) in {:?}; kinds {:?}", items.len(), t.elapsed(), items.iter().map(|i| i.kind).collect::<Vec<_>>());
        }
    }
    let _ = all;
}

/// An idle Codex chat stops its app-server so the thread can be continued elsewhere (the
/// desktop app holds the same writer lock), and the next message resumes the same thread.
#[tokio::test]
#[ignore]
async fn codex_idle_release_frees_the_thread() {
    std::env::set_var("YONDER_CODEX_IDLE_RELEASE_SECS", "3");
    let dir = tempfile::tempdir().unwrap();
    let mut h = spawn_adapter(launch(AgentKind::Codex, dir.path(), ApprovalMode::Yolo)).unwrap();
    let (evs, session) = run_turn(&mut h, "Reply with exactly: ONE", None).await;
    let thread = session.expect("thread id");
    assert!(agent_text(&evs).contains("ONE"));
    let lock = dirs_home().join(".codex/thread-writer-locks").join(format!("{thread}.lock"));
    let held = |p: &std::path::Path| std::process::Command::new("lsof").arg(p).output().map(|o| !o.stdout.is_empty()).unwrap_or(false);
    assert!(held(&lock), "app-server holds the thread while active");
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(!held(&lock), "idle app-server released the thread");
    let (evs, session2) = run_turn(&mut h, "Reply with exactly: TWO", None).await;
    assert!(agent_text(&evs).contains("TWO"), "{:?}", agent_text(&evs));
    assert!(session2.is_none() || session2.as_deref() == Some(thread.as_str()), "same thread, not a fork: {session2:?}");
    assert!(held(&lock), "resumed the same thread");
    h.cmds.send(AdapterCmd::Shutdown).await.unwrap();
}

fn dirs_home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap())
}

/// Sub-agent threads never show in history, not even with `all` (set `YONDER_LIVE_CHILDREN` to
/// child thread / session ids recorded on this machine).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_history_hides_subagents() {
    use yonder_agents::{list_history, HistoryQuery};
    let children: Vec<String> = std::env::var("YONDER_LIVE_CHILDREN").unwrap_or_default().split(',').filter(|s| !s.is_empty()).map(str::to_string).collect();
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let mut cursor = None;
        let mut n = 0;
        loop {
            let p = list_history(&HistoryQuery { agent: Some(agent), limit: Some(200), all: true, cursor: cursor.clone(), ..Default::default() }).await;
            for s in &p.sessions {
                assert!(!children.contains(&s.id), "{agent:?} lists sub-agent {}", s.id);
                assert!(!s.title.starts_with("Run the shell command: touch"), "{agent:?} lists a sub-agent prompt: {} {}", s.id, s.title);
            }
            n += p.sessions.len();
            cursor = p.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        println!("{agent:?}: {n} sessions, none of them sub-agents");
    }
}
