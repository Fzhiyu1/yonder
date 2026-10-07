//! Chat supervisor: one detached process per chat session.
//!
//! Runs the agent adapter (`yonder_agents::spawn_adapter`), keeps the authoritative
//! [`ChatState`], appends every change to `events.jsonl` and serves the daemon over the
//! session socket with the same length-prefixed JSON framing as PTY supervisors.
//! The agent keeps running when the daemon restarts; after the agent exits the supervisor
//! lingers a while so late clients can still read the final state, then quits.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use interprocess::local_socket::tokio::{prelude::*, Stream};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use yonder_agents::{AdapterCmd, AdapterEvent, AgentLaunch};
use yonder_proto::app::{AgentKind, ApprovalMode, ChatSnapshot, ChatStatus};
use yonder_pty::ipc::{read_frame, write_frame};

use crate::chatlog::{ChatEv, ChatExit, ChatMeta, ChatState, EventLog, LogLine};
use crate::util::now_ms;

/// Everything a chat supervisor needs; passed as one base64(JSON) argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSupArgs {
    pub id: String,
    pub dir: PathBuf,
    pub agent: AgentKind,
    pub cwd: PathBuf,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub approval: ApprovalMode,
    #[serde(default)]
    pub resume: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub program: Option<Vec<String>>,
    #[serde(default)]
    pub login_shell: bool,
    /// First user message, sent as soon as the adapter runs.
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub attachments: Vec<PathBuf>,
    /// Seconds to keep serving after the agent exited.
    #[serde(default = "default_linger")]
    pub linger_secs: u64,
}

fn default_linger() -> u64 {
    600
}

impl ChatSupArgs {
    pub fn encode(&self) -> Result<String> {
        Ok(base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(self)?))
    }
    pub fn decode(s: &str) -> Result<Self> {
        let b = base64::engine::general_purpose::STANDARD.decode(s.trim()).context("chat args base64")?;
        serde_json::from_slice(&b).context("chat args json")
    }
}

/// Daemon -> chat supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ChatReq {
    Hello,
    /// Snapshot (when `since` is None or too old) or the missed events, then live events.
    Subscribe {
        #[serde(default)]
        since: Option<u64>,
    },
    Send {
        text: String,
        #[serde(default)]
        attachments: Vec<String>,
    },
    Interrupt,
    Approve { approval: String, option: String },
    /// Change the approval mode in place.
    SetApproval { mode: ApprovalMode },
    /// Switch the agent's model.
    SetModel { model: String },
    /// Stop the agent (the supervisor lingers afterwards).
    Kill,
    /// Stop the agent and the supervisor now.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSupInfo {
    pub id: String,
    pub supervisor_pid: u32,
    pub agent_pid: Option<u32>,
    pub started_at: u64,
    pub seq: u64,
    pub status: ChatStatus,
    pub detail: Option<String>,
    pub pending_approvals: u32,
    pub agent_session: Option<String>,
    pub model: Option<String>,
    /// Current approval mode (launch mode unless changed).
    #[serde(default)]
    pub approval: Option<ApprovalMode>,
    /// Understands `set_model` (older supervisors drop the connection on it).
    #[serde(default)]
    pub model_switch: bool,
    pub preview: Option<String>,
    pub exited: Option<ChatExit>,
}

/// Chat supervisor -> daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ChatSupEvent {
    Info { info: ChatSupInfo },
    /// First answer to `subscribe` when a full view is needed.
    Snapshot { snapshot: ChatSnapshot },
    /// First answer to `subscribe{since}` when the missed events are still in memory
    /// (possibly none). Live events follow.
    CatchUp { events: Vec<(u64, ChatEv)> },
    Ev { seq: u64, ev: ChatEv },
    Meta { meta: ChatMeta },
    /// The request could not be carried out (e.g. the agent already exited).
    Error { message: String },
}

/// Events kept in memory for incremental catch-up.
const RING: usize = 4096;

struct Shared {
    args: ChatSupArgs,
    started_at: u64,
    agent_pid: Option<u32>,
    st: Mutex<Inner>,
    live: broadcast::Sender<ChatSupEvent>,
    cmds: mpsc::Sender<AdapterCmd>,
    shutdown: tokio::sync::Notify,
}

struct Inner {
    state: ChatState,
    ring: std::collections::VecDeque<(u64, ChatEv)>,
    log: EventLog,
}

impl Shared {
    fn info(&self) -> ChatSupInfo {
        let g = self.st.lock().unwrap();
        let s = &g.state;
        ChatSupInfo {
            id: self.args.id.clone(),
            supervisor_pid: std::process::id(),
            agent_pid: self.agent_pid,
            started_at: self.started_at,
            seq: s.seq,
            status: s.status,
            detail: s.detail.clone(),
            pending_approvals: s.approvals.len() as u32,
            agent_session: s.agent_session.clone(),
            model: s.model.clone(),
            approval: Some(s.approval.unwrap_or(self.args.approval)),
            model_switch: true,
            preview: s.preview(),
            exited: s.exited.clone(),
        }
    }

    /// Record one sequenced event and broadcast it.
    fn push(&self, ev: ChatEv) {
        let mut g = self.st.lock().unwrap();
        let seq = g.state.seq + 1;
        g.state.apply(seq, &ev);
        g.log.append(&LogLine { seq: Some(seq), ts: now_ms(), ev: Some(ev.clone()), meta: None });
        g.ring.push_back((seq, ev.clone()));
        while g.ring.len() > RING {
            g.ring.pop_front();
        }
        drop(g);
        let _ = self.live.send(ChatSupEvent::Ev { seq, ev });
    }

    fn push_meta(&self, meta: ChatMeta) {
        let mut g = self.st.lock().unwrap();
        g.state.apply_meta(&meta);
        g.log.append(&LogLine { seq: None, ts: now_ms(), ev: None, meta: Some(meta.clone()) });
        g.log.flush();
        drop(g);
        let _ = self.live.send(ChatSupEvent::Meta { meta });
    }

    fn flush(&self) {
        self.st.lock().unwrap().log.flush();
    }
}

/// Entry point of `yonder __supervise-chat <args>`.
pub fn chat_supervisor_main(args: ChatSupArgs) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(run(args))
}

async fn run(args: ChatSupArgs) -> Result<()> {
    std::fs::create_dir_all(&args.dir)?;
    eprintln!("[{}] chat supervisor {} starting: {:?} in {}", now_ms(), std::process::id(), args.agent, args.cwd.display());
    let listener = yonder_pty::local::bind(&args.dir, &args.id)?;

    // Continue an existing log (supervisor restarted for the same session dir).
    let state = ChatState::load(&args.dir);
    let log = EventLog::open(&args.dir).context("open events.jsonl")?;

    let launch = AgentLaunch {
        agent: args.agent,
        cwd: args.cwd.clone(),
        model: args.model.clone(),
        approval: args.approval,
        resume: args.resume.clone(),
        env: args.env.clone(),
        program: args.program.clone(),
        login_shell: args.login_shell,
    };
    let (live, _) = broadcast::channel(2048);
    let adapter = yonder_agents::spawn_adapter(launch);
    let (cmds, mut events, agent_pid) = match adapter {
        Ok(h) => (h.cmds, Some(h.events), h.pid),
        Err(e) => {
            let (tx, _rx) = mpsc::channel(1);
            eprintln!("[{}] adapter failed to start: {e:#}", now_ms());
            let shared = Arc::new(Shared {
                args: args.clone(),
                started_at: now_ms(),
                agent_pid: None,
                st: Mutex::new(Inner { state, ring: Default::default(), log }),
                live: live.clone(),
                cmds: tx,
                shutdown: tokio::sync::Notify::new(),
            });
            shared.push(ChatEv::Status { status: ChatStatus::Error, detail: Some(format!("{e:#}")) });
            shared.push_meta(ChatMeta::Exited { exit: ChatExit { code: None, error: Some(format!("{e:#}")), ended_at: now_ms() } });
            serve(shared, listener, None).await;
            return Ok(());
        }
    };
    let shared = Arc::new(Shared {
        args: args.clone(),
        started_at: now_ms(),
        agent_pid,
        st: Mutex::new(Inner { state, ring: Default::default(), log }),
        live,
        cmds,
        shutdown: tokio::sync::Notify::new(),
    });

    if let Some(p) = args.prompt.clone().filter(|p| !p.trim().is_empty() || !args.attachments.is_empty()) {
        let _ = shared.cmds.send(AdapterCmd::Send { text: p, attachments: args.attachments.clone() }).await;
    }

    // Adapter events -> state + log + broadcast.
    let pump = {
        let shared = shared.clone();
        let mut events = events.take().unwrap();
        tokio::spawn(async move {
            let mut flush_tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    ev = events.recv() => {
                        let Some(ev) = ev else { break };
                        match ev {
                            AdapterEvent::Item(item) => shared.push(ChatEv::Item { item }),
                            AdapterEvent::Delta { item, field, delta } => shared.push(ChatEv::Delta { item, field, delta }),
                            AdapterEvent::Status { status, detail } => shared.push(ChatEv::Status { status, detail }),
                            AdapterEvent::ApprovalRequested(approval) => {
                                shared.push(ChatEv::ApprovalRequested { approval });
                                shared.flush();
                            }
                            AdapterEvent::ApprovalResolved { approval, option } => shared.push(ChatEv::ApprovalResolved { approval, option }),
                            AdapterEvent::AgentSession(id) => shared.push_meta(ChatMeta::AgentSession { id }),
                            AdapterEvent::Model(model) => shared.push_meta(ChatMeta::Model { model }),
                            AdapterEvent::ApprovalMode(mode) => shared.push_meta(ChatMeta::Approval { mode }),
                            AdapterEvent::Exited { code, error } => {
                                eprintln!("[{}] agent exited: code={code:?} error={error:?}", now_ms());
                                shared.push_meta(ChatMeta::Exited { exit: ChatExit { code, error, ended_at: now_ms() } });
                                break;
                            }
                        }
                    }
                    _ = flush_tick.tick() => shared.flush(),
                }
            }
            shared.flush();
            if shared.st.lock().unwrap().state.exited.is_none() {
                shared.push_meta(ChatMeta::Exited {
                    exit: ChatExit { code: None, error: Some("adapter stopped".into()), ended_at: now_ms() },
                });
            }
        })
    };

    serve(shared.clone(), listener, Some(pump)).await;
    Ok(())
}

async fn serve(shared: Arc<Shared>, listener: interprocess::local_socket::tokio::Listener, pump: Option<tokio::task::JoinHandle<()>>) {
    // Linger after the agent exited, then quit.
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Some(p) = pump {
                let _ = p.await;
            }
            tokio::time::sleep(Duration::from_secs(shared.args.linger_secs)).await;
            shared.shutdown.notify_one();
        });
    }
    loop {
        tokio::select! {
            conn = listener.accept() => match conn {
                Ok(conn) => {
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_conn(shared, conn).await {
                            eprintln!("[{}] client error: {e}", now_ms());
                        }
                    });
                }
                Err(e) => {
                    eprintln!("[{}] accept error: {e}", now_ms());
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            },
            _ = shared.shutdown.notified() => break,
        }
    }
    shared.flush();
    yonder_pty::local::cleanup(&shared.args.dir);
    eprintln!("[{}] chat supervisor exiting", now_ms());
}

async fn serve_conn(shared: Arc<Shared>, conn: Stream) -> Result<()> {
    let (mut rx, mut tx) = conn.split();
    let (out_tx, mut out_rx) = mpsc::channel::<ChatSupEvent>(1024);
    let writer = tokio::spawn(async move {
        while let Some(ev) = out_rx.recv().await {
            if write_frame(&mut tx, &ev).await.is_err() {
                break;
            }
        }
    });
    let mut sub: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(req) = read_frame::<_, ChatReq>(&mut rx).await? {
        match req {
            ChatReq::Hello => {
                let _ = out_tx.send(ChatSupEvent::Info { info: shared.info() }).await;
            }
            ChatReq::Subscribe { since } => {
                if let Some(t) = sub.take() {
                    t.abort();
                }
                // Subscribe before reading state so nothing falls in between.
                let mut live = shared.live.subscribe();
                let (initial, mut next) = {
                    let g = shared.st.lock().unwrap();
                    let cur = g.state.seq;
                    let oldest = g.ring.front().map(|(s, _)| *s).unwrap_or(cur + 1);
                    match since {
                        Some(s) if s <= cur && s + 1 >= oldest => {
                            let events: Vec<(u64, ChatEv)> = g
                                .ring
                                .iter()
                                .filter(|(q, _)| *q > s)
                                .map(|(q, e)| (*q, e.clone()))
                                .collect();
                            (ChatSupEvent::CatchUp { events }, cur)
                        }
                        _ => (ChatSupEvent::Snapshot { snapshot: g.state.snapshot() }, cur),
                    }
                };
                let _ = out_tx.send(initial).await;
                let out = out_tx.clone();
                sub = Some(tokio::spawn(async move {
                    loop {
                        match live.recv().await {
                            Ok(ChatSupEvent::Ev { seq, ev }) => {
                                if seq <= next {
                                    continue;
                                }
                                next = seq;
                                if out.send(ChatSupEvent::Ev { seq, ev }).await.is_err() {
                                    break;
                                }
                            }
                            Ok(other) => {
                                if out.send(other).await.is_err() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let _ = out.send(ChatSupEvent::Error { message: "lagged".into() }).await;
                                break;
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }));
            }
            ChatReq::Send { text, attachments } => {
                let exited = shared.st.lock().unwrap().state.exited.is_some();
                if exited {
                    let _ = out_tx.send(ChatSupEvent::Error { message: "session has exited".into() }).await;
                } else {
                    let attachments = attachments.into_iter().map(PathBuf::from).collect();
                    let _ = shared.cmds.send(AdapterCmd::Send { text, attachments }).await;
                }
            }
            ChatReq::Interrupt => {
                let _ = shared.cmds.send(AdapterCmd::Interrupt).await;
            }
            ChatReq::Approve { approval, option } => {
                let _ = shared.cmds.send(AdapterCmd::Approve { approval_id: approval, option_id: option }).await;
            }
            ChatReq::SetApproval { mode } => {
                let exited = shared.st.lock().unwrap().state.exited.is_some();
                if exited {
                    let _ = out_tx.send(ChatSupEvent::Error { message: "session has exited".into() }).await;
                } else {
                    let _ = shared.cmds.send(AdapterCmd::SetApprovalMode(mode)).await;
                }
            }
            ChatReq::SetModel { model } => {
                let exited = shared.st.lock().unwrap().state.exited.is_some();
                if exited {
                    let _ = out_tx.send(ChatSupEvent::Error { message: "session has exited".into() }).await;
                } else {
                    let _ = shared.cmds.send(AdapterCmd::SetModel(model)).await;
                }
            }
            ChatReq::Kill => {
                let _ = shared.cmds.send(AdapterCmd::Shutdown).await;
            }
            ChatReq::Shutdown => {
                let _ = shared.cmds.send(AdapterCmd::Shutdown).await;
                // Give the adapter a moment to stop the agent and record the exit.
                let sh = shared.clone();
                tokio::spawn(async move {
                    for _ in 0..50 {
                        if sh.st.lock().unwrap().state.exited.is_some() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    sh.shutdown.notify_one();
                });
            }
        }
    }
    if let Some(t) = sub {
        t.abort();
    }
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

/// Daemon-side client for a chat supervisor.
pub struct ChatClient {
    tx: tokio::sync::Mutex<interprocess::local_socket::tokio::SendHalf>,
    rx: tokio::sync::Mutex<interprocess::local_socket::tokio::RecvHalf>,
}

impl ChatClient {
    pub async fn connect(dir: &std::path::Path, id: &str) -> Result<Self> {
        let s = yonder_pty::local::connect(dir, id).await.with_context(|| format!("connect chat supervisor {id}"))?;
        let (rx, tx) = s.split();
        Ok(Self { tx: tokio::sync::Mutex::new(tx), rx: tokio::sync::Mutex::new(rx) })
    }

    pub async fn connect_retry(dir: &std::path::Path, id: &str, timeout: Duration) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match Self::connect(dir, id).await {
                Ok(c) => return Ok(c),
                Err(e) if tokio::time::Instant::now() >= deadline => return Err(e),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    pub async fn send(&self, req: &ChatReq) -> Result<()> {
        let mut tx = self.tx.lock().await;
        write_frame(&mut *tx, req).await.context("send to chat supervisor")
    }

    pub async fn recv(&self) -> Result<Option<ChatSupEvent>> {
        let mut rx = self.rx.lock().await;
        Ok(read_frame(&mut *rx).await?)
    }

    pub async fn info(&self) -> Result<ChatSupInfo> {
        self.send(&ChatReq::Hello).await?;
        loop {
            match self.recv().await? {
                Some(ChatSupEvent::Info { info }) => return Ok(info),
                Some(_) => continue,
                None => anyhow::bail!("chat supervisor closed"),
            }
        }
    }
}
