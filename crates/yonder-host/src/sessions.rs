//! Session manager: creates supervisors, watches them, fans events out to clients.
//!
//! Each session is a directory `sessions/<id>/` with `meta.json` plus the supervisor's
//! files. Terminal sessions are served by `yonder __supervise` (yonder-pty), chat sessions
//! by `yonder __supervise-chat` ([`crate::chatsup`]). Supervisors are detached, so sessions
//! survive daemon restarts; on start the daemon re-adopts every live supervisor.
//!
//! Per session the daemon keeps one "watch" connection (subscribed to all output) that
//! updates list metadata, fires notifications and forwards stream events to attached
//! clients, plus one command connection for input/resize/kill/chat commands.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use yonder_proto::app::{
    AgentKind, ApiError, ApprovalMode, ChatSnapshot, ChatStatus, Event, HostMsg, SessionInfo, SessionKind,
    SessionOrigin, SessionSpec, SessionState, TerminalSnapshot,
};
use yonder_pty::{SupEvent, SupRequest, SupervisorArgs, SupervisorClient};

use crate::chatlog::{ChatEv, ChatMeta, ChatState};
use crate::chatsup::{ChatClient, ChatReq, ChatSupArgs, ChatSupEvent};
use crate::util::{b64, new_id, now_ms, one_line, write_atomic};

pub const META_FILE: &str = "meta.json";
/// Terminal replay sent on attach when the client has nothing (bytes).
const ATTACH_REPLAY: usize = 512 * 1024;
const CLIENT_QUEUE: usize = 2048;

/// Persistent session metadata (`meta.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub kind: SessionKind,
    pub agent: AgentKind,
    pub title: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub origin: SessionOrigin,
    pub created_at: u64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub approval: ApprovalMode,
    #[serde(default)]
    pub agent_session: Option<String>,
    /// Title set explicitly by a user (not overwritten by terminal titles).
    #[serde(default)]
    pub custom_title: bool,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    /// Recorded when the daemon observed the end.
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub ended_at: Option<u64>,
    #[serde(default)]
    pub failed: bool,
}

impl SessionMeta {
    fn save(&self, dir: &Path) -> Result<()> {
        write_atomic(&dir.join(META_FILE), &serde_json::to_vec_pretty(self)?)
    }

    fn load(dir: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(dir.join(META_FILE)).ok()?).ok()
    }
}

/// Something a session wants to tell the notifier.
#[derive(Debug, Clone)]
pub enum SessionSignal {
    Approval { session: String, title: String, body: String },
    TurnDone { session: String, title: String, body: String },
    Exited { session: String, title: String, body: String },
}

/// A connected client (encrypted link or local control connection).
#[derive(Clone)]
pub struct ClientHandle {
    pub id: u64,
    pub tx: mpsc::Sender<HostMsg>,
    /// Set when the client falls behind; its link is then resynced or closed.
    pub lagged: Arc<std::sync::atomic::AtomicBool>,
}

impl ClientHandle {
    pub fn new(id: u64) -> (Self, mpsc::Receiver<HostMsg>) {
        let (tx, rx) = mpsc::channel(CLIENT_QUEUE);
        (Self { id, tx, lagged: Arc::new(std::sync::atomic::AtomicBool::new(false)) }, rx)
    }

    /// Non-blocking send; marks the client lagged when its queue is full.
    pub fn send(&self, m: HostMsg) -> bool {
        match self.tx.try_send(m) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.lagged.store(true, std::sync::atomic::Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

enum Ctl {
    Pty(SupervisorClient),
    Chat(ChatClient),
}

struct Session {
    meta: SessionMeta,
    dir: PathBuf,
    state: SessionState,
    pid: Option<u32>,
    cols: u16,
    rows: u16,
    /// Terminal: bytes of output so far. Chat: last seq.
    offset: u64,
    chat_status: Option<ChatStatus>,
    pending_approvals: u32,
    preview: Option<String>,
    updated_at: u64,
    /// Attached clients.
    attached: HashMap<u64, ClientHandle>,
    ctl: Option<Arc<tokio::sync::Mutex<Ctl>>>,
    /// Chat: authoritative-enough copy for attach snapshots when the supervisor is gone.
    chat: Option<ChatState>,
    /// Chat: the last completed turn produced an agent message since the last notification.
    turn_text: Option<String>,
    /// Chat: the supervisor can change the approval mode in place (older ones cannot).
    approval_switch: bool,
    /// Chat: the supervisor can switch the model (older ones cannot).
    model_switch: bool,
    watching: bool,
    /// Serializes live event fan-out with attach (snapshot + response), so a client never
    /// sees live events of a session before its `attached` response.
    fanout: Arc<tokio::sync::Mutex<()>>,
}

impl Session {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.meta.id.clone(),
            kind: self.meta.kind,
            agent: self.meta.agent,
            title: self.meta.title.clone(),
            command: self.meta.command.clone(),
            cwd: self.meta.cwd.clone(),
            origin: self.meta.origin,
            state: self.state,
            exit_code: self.meta.exit_code,
            pid: self.pid,
            created_at: self.meta.created_at,
            updated_at: self.updated_at,
            cols: self.cols,
            rows: self.rows,
            clients: self.attached.len() as u32,
            agent_session: self.meta.agent_session.clone(),
            chat_status: self.chat_status,
            pending_approvals: self.pending_approvals,
            model: self.meta.model.clone(),
            approval: (self.meta.kind == SessionKind::Chat && self.meta.agent.has_approvals()).then_some(self.meta.approval),
            approval_live: self.approval_switch && self.meta.agent.has_approvals() && self.live() && self.ctl.is_some(),
            preview: self.preview.clone(),
        }
    }

    fn live(&self) -> bool {
        matches!(self.state, SessionState::Starting | SessionState::Running)
    }

    fn new(meta: SessionMeta, dir: PathBuf, state: SessionState) -> Self {
        Session {
            dir,
            state,
            pid: None,
            cols: meta.cols.max(20),
            rows: meta.rows.max(5),
            offset: 0,
            chat_status: (meta.kind == SessionKind::Chat).then_some(ChatStatus::Starting),
            pending_approvals: 0,
            preview: None,
            updated_at: meta.ended_at.unwrap_or(meta.created_at),
            attached: HashMap::new(),
            ctl: None,
            chat: None,
            turn_text: None,
            approval_switch: false,
            model_switch: false,
            watching: false,
            fanout: Arc::new(tokio::sync::Mutex::new(())),
            meta,
        }
    }
}

/// How to start supervisors: the current executable with a hidden subcommand.
#[derive(Clone)]
pub struct Launcher {
    pub program: PathBuf,
    pub pty_prefix: Vec<OsString>,
    pub chat_prefix: Vec<OsString>,
}

impl Launcher {
    pub fn current_exe() -> Result<Self> {
        let program = std::env::current_exe().context("current exe")?;
        Ok(Self {
            program,
            pty_prefix: vec!["__supervise".into()],
            chat_prefix: vec!["__supervise-chat".into()],
        })
    }
}

/// Options that come from the host config.
#[derive(Clone)]
pub struct SessionOptions {
    pub login_shell: bool,
    /// Per-agent program override and env.
    pub agent_program: HashMap<AgentKind, Vec<String>>,
    pub agent_env: HashMap<AgentKind, std::collections::BTreeMap<String, String>>,
    /// Seconds supervisors keep serving after the child exited.
    pub linger_secs: u64,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self { login_shell: true, agent_program: HashMap::new(), agent_env: HashMap::new(), linger_secs: 600 }
    }
}

pub struct Sessions {
    root: PathBuf,
    launcher: Launcher,
    opts: Mutex<SessionOptions>,
    inner: Mutex<HashMap<String, Session>>,
    /// Every connected client, for `session_updated` broadcasts.
    clients: Mutex<HashMap<u64, ClientHandle>>,
    signals: mpsc::UnboundedSender<SessionSignal>,
    /// Session a client is looking at (focus) and when it last said so, per client id.
    focus: Mutex<HashMap<u64, (String, std::time::Instant)>>,
    recent_dirs: Mutex<Vec<String>>,
}

/// How long a client's focus report stays valid (clients repeat it every 10 s while visible).
pub const FOCUS_TTL: Duration = Duration::from_secs(25);

fn focus_current(focused: &str, at: std::time::Instant, session: &str, now: std::time::Instant) -> bool {
    focused == session && now.saturating_duration_since(at) < FOCUS_TTL
}

fn api_internal(e: anyhow::Error) -> ApiError {
    ApiError::internal(format!("{e:#}"))
}

impl Sessions {
    pub fn new(
        root: PathBuf,
        launcher: Launcher,
        opts: SessionOptions,
        signals: mpsc::UnboundedSender<SessionSignal>,
    ) -> Arc<Self> {
        Arc::new(Self {
            root,
            launcher,
            opts: Mutex::new(opts),
            inner: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            signals,
            focus: Mutex::new(HashMap::new()),
            recent_dirs: Mutex::new(Vec::new()),
        })
    }

    pub fn set_options(&self, opts: SessionOptions) {
        *self.opts.lock().unwrap() = opts;
    }

    // ---------------------------------------------------------------- clients

    pub fn add_client(&self, c: ClientHandle) {
        self.clients.lock().unwrap().insert(c.id, c);
    }

    pub fn remove_client(self: &Arc<Self>, id: u64) {
        self.clients.lock().unwrap().remove(&id);
        self.focus.lock().unwrap().remove(&id);
        let mut changed = Vec::new();
        {
            let mut g = self.inner.lock().unwrap();
            for s in g.values_mut() {
                if s.attached.remove(&id).is_some() {
                    changed.push(s.info());
                }
            }
        }
        for info in changed {
            self.broadcast(Event::SessionUpdated { session: info });
        }
    }

    pub fn set_focus(&self, client: u64, session: Option<String>) {
        let mut f = self.focus.lock().unwrap();
        match session {
            Some(s) => {
                f.insert(client, (s, std::time::Instant::now()));
            }
            None => {
                f.remove(&client);
            }
        }
    }

    /// True when some client currently shows this session. Clients repeat their focus while
    /// visible; a phone suspended in the background (iOS) cannot say it left, so a focus that
    /// was not repeated within [`FOCUS_TTL`] no longer counts.
    pub fn is_focused(&self, session: &str) -> bool {
        let now = std::time::Instant::now();
        self.focus.lock().unwrap().values().any(|(s, at)| focus_current(s, *at, session, now))
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    pub fn broadcast(&self, ev: Event) {
        let clients: Vec<ClientHandle> = self.clients.lock().unwrap().values().cloned().collect();
        for c in clients {
            c.send(HostMsg::event(ev.clone()));
        }
    }

    fn fanout_lock(&self, id: &str) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.inner.lock().unwrap().get(id).map(|s| s.fanout.clone())
    }

    /// Send a stream event to every client attached to `id`. Serialized with attach.
    async fn forward(&self, id: &str, ev: Event) {
        let Some(lock) = self.fanout_lock(id) else { return };
        let _g = lock.lock().await;
        let targets: Vec<ClientHandle> = {
            let g = self.inner.lock().unwrap();
            match g.get(id) {
                Some(s) => s.attached.values().cloned().collect(),
                None => return,
            }
        };
        for c in targets {
            c.send(HostMsg::event(ev.clone()));
        }
    }

    fn updated(&self, id: &str) {
        let info = {
            let mut g = self.inner.lock().unwrap();
            match g.get_mut(id) {
                Some(s) => {
                    s.updated_at = now_ms();
                    s.info()
                }
                None => return,
            }
        };
        self.broadcast(Event::SessionUpdated { session: info });
    }

    // ---------------------------------------------------------------- queries

    pub fn list(&self) -> Vec<SessionInfo> {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<SessionInfo> = g.values().map(|s| s.info()).collect();
        v.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        v
    }

    pub fn get(&self, id: &str) -> Option<SessionInfo> {
        self.inner.lock().unwrap().get(id).map(|s| s.info())
    }

    pub fn live_count(&self) -> usize {
        self.inner.lock().unwrap().values().filter(|s| s.live()).count()
    }

    pub fn recent_dirs(&self) -> Vec<String> {
        self.recent_dirs.lock().unwrap().clone()
    }

    fn remember_dir(&self, dir: &str) {
        let mut r = self.recent_dirs.lock().unwrap();
        r.retain(|d| d != dir);
        r.insert(0, dir.to_string());
        r.truncate(12);
    }

    // ---------------------------------------------------------------- startup

    /// Load every session dir; re-adopt live supervisors.
    pub async fn load_existing(self: &Arc<Self>) {
        let _ = std::fs::create_dir_all(&self.root);
        let mut dirs: Vec<(u64, PathBuf, SessionMeta)> = yonder_pty::list_session_dirs(&self.root)
            .into_iter()
            .filter_map(|d| SessionMeta::load(&d).map(|m| (m.created_at, d, m)))
            .collect();
        dirs.sort_by_key(|(t, _, _)| *t);
        for (_, dir, meta) in dirs {
            self.remember_dir(&meta.cwd);
            let alive = match meta.kind {
                SessionKind::Terminal => yonder_pty::supervisor_alive(&dir, &meta.id).await,
                SessionKind::Chat => chat_alive(&dir, &meta.id).await,
            };
            let state = if alive { SessionState::Running } else { SessionState::Exited };
            let mut s = Session::new(meta, dir.clone(), state);
            if !alive {
                self.fill_exited(&mut s);
            }
            let id = s.meta.id.clone();
            self.inner.lock().unwrap().insert(id.clone(), s);
            if alive {
                self.start_watch(&id);
            }
        }
    }

    /// Metadata for a session whose supervisor is gone.
    fn fill_exited(&self, s: &mut Session) {
        s.state = if s.meta.failed { SessionState::Failed } else { SessionState::Exited };
        match s.meta.kind {
            SessionKind::Terminal => {
                if let Some(exit) = yonder_pty::read_exit(&s.dir) {
                    s.meta.exit_code = exit.code;
                    s.meta.ended_at = Some(exit.ended_at);
                    s.updated_at = exit.ended_at;
                }
                s.offset = terminal_log_end(&s.dir);
                s.preview = terminal_preview(&s.dir);
            }
            SessionKind::Chat => {
                let st = ChatState::load(&s.dir);
                s.offset = st.seq;
                s.chat_status = Some(ChatStatus::Exited);
                s.preview = st.preview();
                if let Some(e) = &st.exited {
                    s.meta.exit_code = e.code;
                    s.meta.ended_at = Some(e.ended_at);
                    s.updated_at = e.ended_at;
                }
                if s.meta.agent_session.is_none() {
                    s.meta.agent_session = st.agent_session.clone();
                }
                s.chat = Some(st);
            }
        }
    }

    // ---------------------------------------------------------------- create

    pub async fn create(self: &Arc<Self>, spec: SessionSpec, origin: SessionOrigin) -> Result<SessionInfo, ApiError> {
        let kind = spec.kind.unwrap_or(if spec.command.is_some() { SessionKind::Terminal } else { SessionKind::Chat });
        let agent = spec.agent.unwrap_or(match kind {
            SessionKind::Chat => AgentKind::Codex,
            SessionKind::Terminal => {
                if spec.command.is_some() {
                    AgentKind::Custom
                } else {
                    AgentKind::Shell
                }
            }
        });
        let home = crate::util::home_dir();
        let cwd_raw = spec.cwd.clone().filter(|c| !c.trim().is_empty()).unwrap_or_else(|| home.to_string_lossy().into_owned());
        let cwd = yonder_fs::expand_home(Path::new(cwd_raw.trim()));
        let cwd = dunce::canonicalize(&cwd).map_err(|e| ApiError::invalid(format!("working directory {}: {e}", cwd.display())))?;
        if !cwd.is_dir() {
            return Err(ApiError::invalid(format!("{} is not a folder", cwd.display())));
        }
        let cwd_s = cwd.to_string_lossy().into_owned();
        let opts = self.opts.lock().unwrap().clone();
        let id = new_id(10);
        let dir = self.root.join(&id);
        std::fs::create_dir_all(&dir).map_err(|e| ApiError::internal(format!("create session dir: {e}")))?;
        let cols = spec.cols.unwrap_or(100).clamp(20, 1000);
        let rows = spec.rows.unwrap_or(30).clamp(5, 500);
        let model = spec.model.clone().filter(|m| !m.trim().is_empty());
        let resume = spec.resume.clone().filter(|m| !m.trim().is_empty());
        let approval = spec.approval.unwrap_or_default();
        let mut env = spec.env.clone().unwrap_or_default();
        if let Some(extra) = opts.agent_env.get(&agent) {
            for (k, v) in extra {
                env.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        let (command, title) = match kind {
            SessionKind::Terminal => {
                let argv = match (&spec.command, agent) {
                    (Some(c), _) if !c.is_empty() => c.clone(),
                    (_, AgentKind::Shell) | (_, AgentKind::Custom) => crate::util::default_shell_argv(),
                    (_, a) => {
                        let mut v = yonder_agents::terminal_argv(a, resume.as_deref(), model.as_deref());
                        if let Some(p) = opts.agent_program.get(&a) {
                            v.splice(0..1, p.iter().cloned());
                        }
                        terminal_approval_flags(a, approval, &mut v);
                        v
                    }
                };
                let title = spec.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| default_title(agent, &argv, &cwd));
                (argv, title)
            }
            SessionKind::Chat => {
                if !matches!(agent, AgentKind::Codex | AgentKind::Claude | AgentKind::Pi) {
                    return Err(ApiError::invalid(format!("{} has no chat mode", agent.as_str())));
                }
                let title = spec
                    .title
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| spec.prompt.as_deref().filter(|p| !p.trim().is_empty()).map(|p| one_line(p, 60)))
                    .unwrap_or_else(|| default_title(agent, &[], &cwd));
                (vec![agent.as_str().to_string()], title)
            }
        };

        let meta = SessionMeta {
            id: id.clone(),
            kind,
            agent,
            title,
            command: command.clone(),
            cwd: cwd_s.clone(),
            origin,
            created_at: now_ms(),
            model: model.clone(),
            approval,
            agent_session: resume.clone(),
            custom_title: spec.title.as_deref().is_some_and(|t| !t.trim().is_empty()),
            cols,
            rows,
            exit_code: None,
            ended_at: None,
            failed: false,
        };
        meta.save(&dir).map_err(api_internal)?;

        let spawned = match kind {
            SessionKind::Terminal => {
                let mut env = env.clone();
                if !env.contains_key("PATH") {
                    if let Some(p) = yonder_agents::user_path() {
                        env.insert("PATH".into(), p);
                    }
                }
                let args = SupervisorArgs {
                    id: id.clone(),
                    dir: dir.clone(),
                    argv: command.clone(),
                    cwd: cwd.clone(),
                    env,
                    cols,
                    rows,
                    // Shells are already login shells; agents benefit from the profile PATH.
                    login_shell: opts.login_shell && !matches!(agent, AgentKind::Shell),
                    linger_secs: opts.linger_secs,
                };
                yonder_pty::spawn_supervisor(&self.launcher.program, &self.launcher.pty_prefix, &args)
            }
            SessionKind::Chat => {
                let attachments: Vec<PathBuf> = Vec::new();
                let args = ChatSupArgs {
                    id: id.clone(),
                    dir: dir.clone(),
                    agent,
                    cwd: cwd.clone(),
                    model: model.clone(),
                    approval,
                    resume: resume.clone(),
                    env,
                    program: opts.agent_program.get(&agent).cloned(),
                    login_shell: opts.login_shell,
                    prompt: spec.prompt.clone(),
                    attachments,
                    linger_secs: opts.linger_secs,
                };
                let mut argv = self.launcher.chat_prefix.clone();
                argv.push(args.encode().map_err(api_internal)?.into());
                yonder_pty::launch::spawn_detached(&self.launcher.program, &argv, &dir, &dir.join(yonder_pty::SUP_LOG_FILE))
            }
        };
        let pid = match spawned {
            Ok(pid) => pid,
            Err(e) => {
                let mut m = meta.clone();
                m.failed = true;
                m.ended_at = Some(now_ms());
                let _ = m.save(&dir);
                return Err(ApiError::internal(format!("start session: {e:#}")));
            }
        };
        tracing::info!(session = %id, ?kind, agent = agent.as_str(), pid, "session created");
        self.remember_dir(&cwd_s);
        // The history view should show the new (or resumed) agent session right away.
        yonder_agents::invalidate_history();

        let mut s = Session::new(meta, dir, SessionState::Starting);
        s.updated_at = now_ms();
        let info = s.info();
        self.inner.lock().unwrap().insert(id.clone(), s);
        self.broadcast(Event::SessionUpdated { session: info.clone() });
        self.start_watch(&id);
        // Wait briefly for the supervisor so the first attach sees a live session.
        for _ in 0..100 {
            if self.ctl(&id).is_some() || !self.get(&id).map(|s| s.state == SessionState::Starting).unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(self.get(&id).unwrap_or(info))
    }

    fn ctl(&self, id: &str) -> Option<Arc<tokio::sync::Mutex<Ctl>>> {
        self.inner.lock().unwrap().get(id).and_then(|s| s.ctl.clone())
    }

    // ---------------------------------------------------------------- watch

    fn start_watch(self: &Arc<Self>, id: &str) {
        let (kind, dir) = {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.get_mut(id) else { return };
            if s.watching {
                return;
            }
            s.watching = true;
            (s.meta.kind, s.dir.clone())
        };
        let me = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let r = match kind {
                SessionKind::Terminal => me.watch_pty(&id, &dir).await,
                SessionKind::Chat => me.watch_chat(&id, &dir).await,
            };
            if let Err(e) = r {
                tracing::warn!(session = %id, "watch ended: {e:#}");
            }
            me.on_supervisor_gone(&id);
        });
    }

    async fn watch_pty(self: &Arc<Self>, id: &str, dir: &Path) -> Result<()> {
        let watch = SupervisorClient::connect_retry(dir, id, Duration::from_secs(10)).await?;
        let ctl = SupervisorClient::connect_retry(dir, id, Duration::from_secs(5)).await?;
        let info = watch.info().await?;
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(s) = g.get_mut(id) {
                s.state = if info.exited.is_some() { SessionState::Exited } else { SessionState::Running };
                s.pid = info.child_pid;
                s.cols = info.cols;
                s.rows = info.rows;
                s.offset = info.offset;
                s.preview = info.preview.clone();
                if let Some(t) = info.title.clone().filter(|t| !t.trim().is_empty()) {
                    if !s.meta.custom_title {
                        s.meta.title = one_line(&t, 80);
                    }
                }
                if let Some(e) = &info.exited {
                    s.meta.exit_code = e.code;
                    s.meta.ended_at = Some(e.ended_at);
                }
                s.ctl = Some(Arc::new(tokio::sync::Mutex::new(Ctl::Pty(ctl))));
            }
        }
        self.updated(id);
        watch.subscribe(Some(info.offset)).await?;
        let mut last_preview = std::time::Instant::now();
        loop {
            let Some(ev) = watch.recv().await? else { break };
            match ev {
                SupEvent::Output { offset, data } => {
                    let len = crate::util::unb64(&data).map(|b| b.len() as u64).unwrap_or(0);
                    let first_update = {
                        let mut g = self.inner.lock().unwrap();
                        match g.get_mut(id) {
                            Some(s) => {
                                s.offset = s.offset.max(offset + len);
                                s.updated_at = now_ms();
                                false
                            }
                            None => false,
                        }
                    };
                    let _ = first_update;
                    if len > 0 {
                        self.forward(id, Event::PtyOutput { session: id.to_string(), offset, data }).await;
                    }
                    if last_preview.elapsed() > Duration::from_secs(3) {
                        last_preview = std::time::Instant::now();
                        let me = self.clone();
                        let id2 = id.to_string();
                        let dir2 = dir.to_path_buf();
                        tokio::spawn(async move { me.refresh_pty_preview(&id2, &dir2).await });
                    }
                }
                SupEvent::Snapshot { offset, .. } => {
                    if let Some(s) = self.inner.lock().unwrap().get_mut(id) {
                        s.offset = s.offset.max(offset);
                    }
                }
                SupEvent::Resized { cols, rows } => {
                    if let Some(s) = self.inner.lock().unwrap().get_mut(id) {
                        s.cols = cols;
                        s.rows = rows;
                        s.meta.cols = cols;
                        s.meta.rows = rows;
                        let _ = s.meta.save(&s.dir);
                    }
                    self.forward(id, Event::PtyResized { session: id.to_string(), cols, rows }).await;
                    self.updated(id);
                }
                SupEvent::Title { title } => {
                    let changed = {
                        let mut g = self.inner.lock().unwrap();
                        match g.get_mut(id) {
                            Some(s) if !s.meta.custom_title && !title.trim().is_empty() => {
                                s.meta.title = one_line(&title, 80);
                                let _ = s.meta.save(&s.dir);
                                true
                            }
                            _ => false,
                        }
                    };
                    if changed {
                        self.updated(id);
                    }
                }
                SupEvent::Exited { exit } => {
                    let (title, preview) = {
                        let mut g = self.inner.lock().unwrap();
                        match g.get_mut(id) {
                            Some(s) => {
                                s.state = SessionState::Exited;
                                s.meta.exit_code = exit.code;
                                s.meta.ended_at = Some(exit.ended_at);
                                let _ = s.meta.save(&s.dir);
                                (s.meta.title.clone(), s.preview.clone())
                            }
                            None => (String::new(), None),
                        }
                    };
                    self.refresh_pty_preview(id, dir).await;
                    self.updated(id);
                    let body = match exit.code {
                        Some(c) => format!("exit code {c}"),
                        None => exit.signal.clone().map(|s| format!("signal {s}")).unwrap_or_else(|| "ended".into()),
                    };
                    let body = preview.map(|p| format!("{body} · {p}")).unwrap_or(body);
                    let _ = self.signals.send(SessionSignal::Exited { session: id.to_string(), title, body });
                }
                SupEvent::Error { message } => {
                    tracing::debug!(session = %id, "supervisor: {message}");
                    if message == "lagged" {
                        // Our own watch fell behind: resubscribe from the current end.
                        let off = self.inner.lock().unwrap().get(id).map(|s| s.offset);
                        watch.subscribe(off).await?;
                        self.resync_attached_pty(id).await;
                    }
                }
                SupEvent::Info { .. } => {}
            }
        }
        Ok(())
    }

    async fn refresh_pty_preview(&self, id: &str, _dir: &Path) {
        let Some(ctl) = self.ctl(id) else { return };
        let info = {
            let g = ctl.lock().await;
            match &*g {
                Ctl::Pty(c) => tokio::time::timeout(Duration::from_secs(2), c.info()).await.ok().and_then(|r| r.ok()),
                Ctl::Chat(_) => None,
            }
        };
        if let Some(info) = info {
            let changed = {
                let mut g = self.inner.lock().unwrap();
                match g.get_mut(id) {
                    Some(s) if s.preview != info.preview => {
                        s.preview = info.preview.clone();
                        true
                    }
                    _ => false,
                }
            };
            if changed {
                self.updated(id);
            }
        }
    }

    async fn watch_chat(self: &Arc<Self>, id: &str, dir: &Path) -> Result<()> {
        let watch = ChatClient::connect_retry(dir, id, Duration::from_secs(15)).await?;
        let ctl = ChatClient::connect_retry(dir, id, Duration::from_secs(5)).await?;
        let info = watch.info().await?;
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(s) = g.get_mut(id) {
                s.state = if info.exited.is_some() { SessionState::Exited } else { SessionState::Running };
                s.pid = info.agent_pid;
                s.chat_status = Some(info.status);
                s.pending_approvals = info.pending_approvals;
                s.preview = info.preview.clone();
                s.offset = info.seq;
                if info.agent_session.is_some() {
                    s.meta.agent_session = info.agent_session.clone();
                }
                if info.model.is_some() {
                    s.meta.model = info.model.clone();
                }
                if let Some(mode) = info.approval {
                    if mode != s.meta.approval {
                        s.meta.approval = mode;
                        let _ = s.meta.save(&s.dir);
                    }
                }
                // Supervisors of older releases do not report the mode and would drop the
                // connection on a `set_approval` request.
                s.approval_switch = info.approval.is_some();
                s.model_switch = info.model_switch;
                s.ctl = Some(Arc::new(tokio::sync::Mutex::new(Ctl::Chat(ctl))));
            }
        }
        self.updated(id);
        // Full state first so attach snapshots can be served locally.
        watch.send(&ChatReq::Subscribe { since: None }).await?;
        loop {
            let Some(ev) = watch.recv().await? else { break };
            match ev {
                ChatSupEvent::Snapshot { snapshot } => {
                    let mut st = ChatState::from_snapshot(&snapshot);
                    if let Some(s) = self.inner.lock().unwrap().get_mut(id) {
                        s.offset = snapshot.seq;
                        st.agent_session = s.meta.agent_session.clone();
                        s.chat = Some(st);
                    }
                    // Attached clients may be behind: give them the fresh view.
                    self.forward(id, Event::ChatSnapshot { session: id.to_string(), snapshot }).await;
                }
                ChatSupEvent::CatchUp { events } => {
                    for (seq, ev) in events {
                        self.on_chat_ev(id, seq, ev).await;
                    }
                }
                ChatSupEvent::Ev { seq, ev } => self.on_chat_ev(id, seq, ev).await,
                ChatSupEvent::Meta { meta } => self.on_chat_meta(id, meta),
                ChatSupEvent::Error { message } => {
                    if message == "lagged" {
                        watch.send(&ChatReq::Subscribe { since: None }).await?;
                    } else {
                        tracing::debug!(session = %id, "chat supervisor: {message}");
                    }
                }
                ChatSupEvent::Info { .. } => {}
            }
        }
        Ok(())
    }

    async fn on_chat_ev(self: &Arc<Self>, id: &str, seq: u64, ev: ChatEv) {
        let Some(lock) = self.fanout_lock(id) else { return };
        let fan = lock.lock().await;
        let mut signal = None;
        let mut list_changed = false;
        let targets: Vec<ClientHandle>;
        {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.get_mut(id) else { return };
            if seq <= s.offset && s.chat.as_ref().map(|c| c.seq >= seq).unwrap_or(false) {
                return;
            }
            s.offset = seq;
            s.updated_at = now_ms();
            let st = s.chat.get_or_insert_with(ChatState::default);
            let before_status = st.status;
            st.apply(seq, &ev);
            let preview = st.preview();
            if preview != s.preview {
                s.preview = preview;
                list_changed = true;
            }
            let pending = st.approvals.len() as u32;
            if pending != s.pending_approvals {
                s.pending_approvals = pending;
                list_changed = true;
            }
            match &ev {
                ChatEv::Status { status, .. } => {
                    if s.chat_status != Some(*status) {
                        s.chat_status = Some(*status);
                        list_changed = true;
                    }
                    if *status == ChatStatus::Idle && before_status == ChatStatus::Working {
                        let body = st.last_agent_text.clone().unwrap_or_default();
                        signal = Some(SessionSignal::TurnDone {
                            session: id.to_string(),
                            title: s.meta.title.clone(),
                            body: one_line(&body, 300),
                        });
                    }
                }
                ChatEv::ApprovalRequested { approval } => {
                    let body = approval
                        .command
                        .clone()
                        .or_else(|| approval.reason.clone())
                        .unwrap_or_else(|| approval.title.clone());
                    signal = Some(SessionSignal::Approval {
                        session: id.to_string(),
                        title: s.meta.title.clone(),
                        body: format!("{} · {}", approval.title, one_line(&body, 300)),
                    });
                }
                _ => {}
            }
            let _ = s.turn_text.take();
            targets = s.attached.values().cloned().collect();
        }
        let event = ev.to_event(id, seq);
        for c in targets {
            c.send(HostMsg::event(event.clone()));
        }
        drop(fan);
        if list_changed {
            self.updated(id);
        }
        if let Some(sig) = signal {
            let _ = self.signals.send(sig);
        }
    }

    fn on_chat_meta(self: &Arc<Self>, id: &str, meta: ChatMeta) {
        let mut exit_signal = None;
        {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.get_mut(id) else { return };
            if let Some(st) = s.chat.as_mut() {
                st.apply_meta(&meta);
            }
            match &meta {
                ChatMeta::AgentSession { id: aid } => {
                    s.meta.agent_session = Some(aid.clone());
                }
                ChatMeta::Model { model } => {
                    s.meta.model = Some(model.clone());
                }
                ChatMeta::Approval { mode } => {
                    s.meta.approval = *mode;
                }
                ChatMeta::Exited { exit } => {
                    s.state = SessionState::Exited;
                    s.chat_status = Some(ChatStatus::Exited);
                    s.pending_approvals = 0;
                    s.meta.exit_code = exit.code;
                    s.meta.ended_at = Some(exit.ended_at);
                    let body = exit
                        .error
                        .clone()
                        .or_else(|| exit.code.map(|c| format!("exit code {c}")))
                        .unwrap_or_else(|| "ended".into());
                    exit_signal = Some(SessionSignal::Exited { session: id.to_string(), title: s.meta.title.clone(), body });
                }
            }
            let _ = s.meta.save(&s.dir);
        }
        self.updated(id);
        if let Some(sig) = exit_signal {
            let _ = self.signals.send(sig);
        }
    }

    fn on_supervisor_gone(self: &Arc<Self>, id: &str) {
        {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.get_mut(id) else { return };
            s.watching = false;
            s.ctl = None;
            s.approval_switch = false;
            s.model_switch = false;
            s.pid = None;
            s.pending_approvals = 0;
            if s.live() {
                // Supervisor vanished without reporting an exit (crash or never started).
                if s.state == SessionState::Starting {
                    s.meta.failed = true;
                }
                s.meta.ended_at.get_or_insert_with(now_ms);
                let _ = s.meta.save(&s.dir);
            }
            s.chat = None;
            self.fill_exited(s);
        }
        self.updated(id);
    }

    async fn resync_attached_pty(&self, id: &str) {
        let clients: Vec<ClientHandle> = {
            let g = self.inner.lock().unwrap();
            g.get(id).map(|s| s.attached.values().cloned().collect()).unwrap_or_default()
        };
        for c in clients {
            if let Ok(snap) = self.terminal_snapshot(id, None).await {
                c.send(HostMsg::event(Event::PtySnapshot { session: id.to_string(), snapshot: snap }));
            }
        }
    }

    /// A client dropped events (its queue overflowed): send fresh snapshots for every
    /// session it is attached to.
    pub async fn resync_client(&self, client: &ClientHandle) {
        let sessions: Vec<(String, SessionKind)> = {
            let g = self.inner.lock().unwrap();
            g.iter().filter(|(_, s)| s.attached.contains_key(&client.id)).map(|(id, s)| (id.clone(), s.meta.kind)).collect()
        };
        for (id, kind) in sessions {
            let Some(lock) = self.fanout_lock(&id) else { continue };
            let _g = lock.lock().await;
            match kind {
                SessionKind::Terminal => {
                    if let Ok(snap) = self.terminal_snapshot(&id, None).await {
                        client.send(HostMsg::event(Event::PtySnapshot { session: id.clone(), snapshot: snap }));
                    }
                }
                SessionKind::Chat => {
                    let snap = self.chat_snapshot(&id);
                    client.send(HostMsg::event(Event::ChatSnapshot { session: id.clone(), snapshot: snap }));
                }
            }
        }
        // Session list may be stale too.
        for info in self.list() {
            client.send(HostMsg::event(Event::SessionUpdated { session: info }));
        }
    }

    // ---------------------------------------------------------------- attach

    /// Attach `client` to a session and send the `attached` response for request `req_id`
    /// itself: the response and the start of the live stream are ordered under the
    /// session's fan-out lock, so the client never sees live events before the snapshot
    /// and never misses any in between (duplicates are dropped by offset / seq).
    pub async fn attach(self: &Arc<Self>, id: &str, client: &ClientHandle, since: Option<u64>, req_id: u64) {
        let r = self.attach_inner(id, client, since, req_id).await;
        if let Err(e) = r {
            client.send(HostMsg::err(req_id, e));
        }
    }

    async fn attach_inner(self: &Arc<Self>, id: &str, client: &ClientHandle, since: Option<u64>, req_id: u64) -> Result<(), ApiError> {
        let (kind, lock) = {
            let g = self.inner.lock().unwrap();
            let s = g.get(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            (s.meta.kind, s.fanout.clone())
        };
        let fan = lock.lock().await;
        let (terminal, chat) = match kind {
            SessionKind::Terminal => (Some(self.terminal_snapshot(id, since).await?), None),
            SessionKind::Chat => (None, Some(self.chat_snapshot(id))),
        };
        let info = {
            let mut g = self.inner.lock().unwrap();
            let s = g.get_mut(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            s.attached.insert(client.id, client.clone());
            s.info()
        };
        client.send(HostMsg::ok(req_id, yonder_proto::app::Response::Attached { session: info, terminal, chat }));
        drop(fan);
        self.updated(id);
        Ok(())
    }

    pub fn detach(self: &Arc<Self>, id: &str, client: u64) {
        let removed = {
            let mut g = self.inner.lock().unwrap();
            g.get_mut(id).map(|s| s.attached.remove(&client).is_some()).unwrap_or(false)
        };
        if removed {
            self.updated(id);
        }
    }

    async fn terminal_snapshot(&self, id: &str, since: Option<u64>) -> Result<TerminalSnapshot, ApiError> {
        let (dir, cols, rows, live) = {
            let g = self.inner.lock().unwrap();
            let s = g.get(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            (s.dir.clone(), s.cols, s.rows, s.ctl.is_some())
        };
        let end = terminal_log_end(&dir);
        // Incremental: the client already has [0, since).
        if let Some(since) = since {
            if since <= end && end - since <= ATTACH_REPLAY as u64 {
                if let Ok(Some(bytes)) = yonder_pty::log::read_range(&dir, since, ATTACH_REPLAY) {
                    let offset = since + bytes.len() as u64;
                    return Ok(TerminalSnapshot { reset: false, data: b64(&bytes), offset, cols, rows });
                }
            }
        }
        // Live session: a rendered screen from the supervisor (exact, compact).
        if live {
            if let Some(ctl) = self.ctl(id) {
                let g = ctl.lock().await;
                if let Ctl::Pty(c) = &*g {
                    // The command connection is not subscribed, so the next frame answers us.
                    if c.send(&SupRequest::Snapshot).await.is_ok() {
                        let r = tokio::time::timeout(Duration::from_secs(3), async {
                            loop {
                                match c.recv().await {
                                    Ok(Some(SupEvent::Snapshot { data, offset, cols, rows })) => {
                                        return Some(TerminalSnapshot { reset: true, data, offset, cols, rows })
                                    }
                                    Ok(Some(_)) => continue,
                                    _ => return None,
                                }
                            }
                        })
                        .await;
                        if let Ok(Some(snap)) = r {
                            return Ok(snap);
                        }
                    }
                }
            }
        }
        // Exited (or snapshot failed): replay the tail of the log.
        let from = end.saturating_sub(ATTACH_REPLAY as u64);
        let base = yonder_pty_log_base(&dir);
        let from = from.max(base);
        let bytes = yonder_pty::log::read_range(&dir, from, ATTACH_REPLAY).ok().flatten().unwrap_or_default();
        let mut data = Vec::with_capacity(bytes.len() + 16);
        data.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
        data.extend_from_slice(&bytes);
        Ok(TerminalSnapshot { reset: true, data: b64(&data), offset: from + bytes.len() as u64, cols, rows })
    }

    fn chat_snapshot(&self, id: &str) -> ChatSnapshot {
        let mut g = self.inner.lock().unwrap();
        let Some(s) = g.get_mut(id) else {
            return ChatSnapshot { items: vec![], approvals: vec![], status: ChatStatus::Exited, seq: 0, truncated: false };
        };
        if s.chat.is_none() {
            s.chat = Some(ChatState::load(&s.dir));
        }
        let st = s.chat.as_ref().unwrap();
        // Only the newest page: long chats used to send megabytes before showing anything.
        let mut snap = st.page();
        if let Some(cs) = s.chat_status {
            snap.status = cs;
        }
        if !s.live() {
            snap.status = ChatStatus::Exited;
            snap.approvals.clear();
        }
        snap
    }

    /// Items before `before` for a client that scrolled up.
    pub fn chat_older(&self, id: &str, before: &str, limit: usize) -> Result<(Vec<yonder_proto::app::ChatItem>, bool), ApiError> {
        let dir = {
            let mut g = self.inner.lock().unwrap();
            let s = g.get_mut(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            if s.meta.kind != SessionKind::Chat {
                return Err(ApiError::invalid("not a chat session"));
            }
            if s.chat.is_none() {
                s.chat = Some(ChatState::load(&s.dir));
            }
            if let Some(r) = s.chat.as_ref().unwrap().older(before, limit) {
                return Ok(r);
            }
            s.dir.clone()
        };
        // The daemon's copy starts at the supervisor's snapshot; the log has the rest.
        match ChatState::load(&dir).older(before, limit) {
            Some(r) => Ok(r),
            // Older than what the log keeps in memory.
            None => Ok((Vec::new(), false)),
        }
    }

    // ---------------------------------------------------------------- commands

    pub async fn input(&self, id: &str, bytes: Vec<u8>) {
        if let Some(ctl) = self.ctl(id) {
            if let Ctl::Pty(c) = &*ctl.lock().await {
                let _ = c.input(&bytes).await;
            }
        }
    }

    pub async fn resize(&self, id: &str, cols: u16, rows: u16) {
        if cols < 2 || rows < 1 {
            return;
        }
        if let Some(ctl) = self.ctl(id) {
            if let Ctl::Pty(c) = &*ctl.lock().await {
                let _ = c.resize(cols.min(1000), rows.min(500)).await;
            }
        }
    }

    pub async fn kill(self: &Arc<Self>, id: &str) -> Result<(), ApiError> {
        let (live, kind) = {
            let g = self.inner.lock().unwrap();
            let s = g.get(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            (s.live(), s.meta.kind)
        };
        if !live {
            return Ok(());
        }
        let ctl = self.ctl(id).ok_or_else(|| ApiError::busy("session is still starting"))?;
        let g = ctl.lock().await;
        match (&*g, kind) {
            (Ctl::Pty(c), _) => c.kill().await.map_err(api_internal)?,
            (Ctl::Chat(c), _) => c.send(&ChatReq::Kill).await.map_err(api_internal)?,
        }
        Ok(())
    }

    pub async fn remove(self: &Arc<Self>, id: &str) -> Result<(), ApiError> {
        let (live, dir, kind) = {
            let g = self.inner.lock().unwrap();
            let s = g.get(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            (s.live(), s.dir.clone(), s.meta.kind)
        };
        if live {
            return Err(ApiError::busy("session is still running; kill it first"));
        }
        // Stop a lingering supervisor so its files can go.
        if let Some(ctl) = self.ctl(id) {
            let g = ctl.lock().await;
            match &*g {
                Ctl::Pty(c) => {
                    let _ = c.send(&SupRequest::Shutdown).await;
                }
                Ctl::Chat(c) => {
                    let _ = c.send(&ChatReq::Shutdown).await;
                }
            }
        }
        let _ = kind;
        self.inner.lock().unwrap().remove(id);
        self.broadcast(Event::SessionRemoved { session: id.to_string() });
        // The supervisor may hold its files for a moment after shutdown (Windows). A leftover
        // directory would bring the session back at the next daemon start, so report it.
        let mut last = None;
        for delay in [150u64, 300, 600, 1200, 2400] {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        let e = last.map(|e| e.to_string()).unwrap_or_default();
        tracing::warn!(session = %id, "remove {}: {e}", dir.display());
        Err(ApiError::internal(format!("session removed, but its files could not be deleted ({}): {e}", dir.display())))
    }

    pub fn rename(self: &Arc<Self>, id: &str, title: &str) -> Result<SessionInfo, ApiError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(ApiError::invalid("empty title"));
        }
        {
            let mut g = self.inner.lock().unwrap();
            let s = g.get_mut(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            s.meta.title = one_line(title, 120);
            s.meta.custom_title = true;
            s.meta.save(&s.dir).map_err(api_internal)?;
        }
        self.updated(id);
        self.get(id).ok_or_else(|| ApiError::not_found(id.to_string()))
    }

    pub async fn chat_send(&self, id: &str, text: String, attachments: Vec<String>) -> Result<(), ApiError> {
        let (kind, live) = self.kind_live(id)?;
        if kind != SessionKind::Chat {
            return Err(ApiError::invalid("not a chat session"));
        }
        if !live {
            return Err(ApiError::invalid("session has ended"));
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Err(ApiError::invalid("empty message"));
        }
        let ctl = self.ctl(id).ok_or_else(|| ApiError::busy("session is still starting"))?;
        let g = ctl.lock().await;
        if let Ctl::Chat(c) = &*g {
            c.send(&ChatReq::Send { text, attachments }).await.map_err(api_internal)?;
        }
        Ok(())
    }

    pub async fn chat_interrupt(&self, id: &str) -> Result<(), ApiError> {
        let (kind, _) = self.kind_live(id)?;
        if kind != SessionKind::Chat {
            return Err(ApiError::invalid("not a chat session"));
        }
        let ctl = self.ctl(id).ok_or_else(|| ApiError::invalid("session is not running"))?;
        let g = ctl.lock().await;
        if let Ctl::Chat(c) = &*g {
            c.send(&ChatReq::Interrupt).await.map_err(api_internal)?;
        }
        Ok(())
    }

    pub async fn approval_respond(&self, id: &str, approval: String, option: String) -> Result<(), ApiError> {
        let (kind, live) = self.kind_live(id)?;
        if kind != SessionKind::Chat || !live {
            return Err(ApiError::invalid("no pending approvals in this session"));
        }
        let known = {
            let g = self.inner.lock().unwrap();
            g.get(id)
                .and_then(|s| s.chat.as_ref())
                .map(|c| c.approvals.iter().any(|a| a.id == approval && a.options.iter().any(|o| o.id == option)))
                .unwrap_or(false)
        };
        if !known {
            return Err(ApiError::not_found("approval is no longer pending"));
        }
        let ctl = self.ctl(id).ok_or_else(|| ApiError::invalid("session is not running"))?;
        let g = ctl.lock().await;
        if let Ctl::Chat(c) = &*g {
            c.send(&ChatReq::Approve { approval, option }).await.map_err(api_internal)?;
        }
        Ok(())
    }

    fn kind_live(&self, id: &str) -> Result<(SessionKind, bool), ApiError> {
        let g = self.inner.lock().unwrap();
        let s = g.get(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
        Ok((s.meta.kind, s.live()))
    }

    /// Change a running chat's approval mode (Codex, Claude). The supervisor applies it in place
    /// and reports it back (`ChatMeta::Approval`). A chat run by a supervisor of an older release
    /// is restarted instead: it resumes the agent's own session in the new mode, and the answer
    /// is the new session.
    pub async fn set_approval_mode(self: &Arc<Self>, id: &str, mode: ApprovalMode) -> Result<SessionInfo, ApiError> {
        let (kind, live, agent, in_place, meta) = {
            let g = self.inner.lock().unwrap();
            let s = g.get(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            (s.meta.kind, s.live(), s.meta.agent, s.approval_switch, s.meta.clone())
        };
        if kind != SessionKind::Chat {
            return Err(ApiError::invalid("not a chat session"));
        }
        if !agent.has_approvals() {
            return Err(ApiError::unsupported(format!("{} does not ask for approvals", agent.as_str())));
        }
        if !live {
            return Err(ApiError::invalid("session has ended"));
        }
        let ctl = self.ctl(id).ok_or_else(|| ApiError::busy("session is still starting"))?;
        if !in_place {
            return self.restart_with_mode(id, meta, mode).await;
        }
        {
            let g = ctl.lock().await;
            if let Ctl::Chat(c) = &*g {
                c.send(&ChatReq::SetApproval { mode }).await.map_err(api_internal)?;
            }
        }
        // Answer with the new mode right away; the supervisor's confirmation follows as an update.
        let info = {
            let mut g = self.inner.lock().unwrap();
            let s = g.get_mut(id).ok_or_else(|| ApiError::not_found(id.to_string()))?;
            s.meta.approval = mode;
            let _ = s.meta.save(&s.dir);
            s.info()
        };
        Ok(info)
    }

    /// Switch a running chat's model in place. The answer carries the requested model right
    /// away; the supervisor reports the model actually in effect (`ChatMeta::Model`) after.
    pub async fn set_chat_model(&self, id: &str, model: &str) -> Result<SessionInfo, ApiError> {
        let model = model.trim().to_string();
        if model.is_empty() {
            return Err(ApiError::invalid("empty model"));
        }
        let (kind, live, in_place) = {
            let g = self.inner.lock().unwrap();
            let s = g.get(id).ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            (s.meta.kind, s.live(), s.approval_switch && s.model_switch)
        };
        if kind != SessionKind::Chat {
            return Err(ApiError::invalid("not a chat session"));
        }
        if !live {
            return Err(ApiError::invalid("session has ended"));
        }
        let ctl = self.ctl(id).ok_or_else(|| ApiError::busy("session is still starting"))?;
        // Supervisors of older releases drop the connection on requests they do not know.
        if !in_place {
            return Err(ApiError::unsupported("restart this chat to switch models"));
        }
        {
            let g = ctl.lock().await;
            if let Ctl::Chat(c) = &*g {
                c.send(&ChatReq::SetModel { model: model.clone() }).await.map_err(api_internal)?;
            }
        }
        let info = {
            let mut g = self.inner.lock().unwrap();
            let s = g.get_mut(id).ok_or_else(|| ApiError::not_found(id.to_string()))?;
            s.meta.model = Some(model);
            let _ = s.meta.save(&s.dir);
            s.info()
        };
        Ok(info)
    }

    async fn restart_with_mode(self: &Arc<Self>, id: &str, meta: SessionMeta, mode: ApprovalMode) -> Result<SessionInfo, ApiError> {
        let Some(resume) = meta.agent_session.clone() else {
            return Err(ApiError::unsupported("this chat cannot switch modes: the agent has not reported its session yet"));
        };
        let _ = self.kill(id).await;
        // Let the agent flush its session before the new chat resumes it.
        for _ in 0..50 {
            if !self.get(id).map(|s| matches!(s.state, SessionState::Running | SessionState::Starting)).unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tracing::info!(session = %id, mode = mode.as_str(), "restarting chat to change its approval mode");
        let spec = SessionSpec {
            kind: Some(SessionKind::Chat),
            agent: Some(meta.agent),
            cwd: Some(meta.cwd.clone()),
            title: meta.custom_title.then(|| meta.title.clone()),
            model: meta.model.clone(),
            approval: Some(mode),
            resume: Some(resume),
            ..Default::default()
        };
        self.create(spec, meta.origin).await
    }

    /// Terminal agent session -> chat session resuming the same agent session.
    pub async fn continue_as_chat(self: &Arc<Self>, id: &str) -> Result<SessionInfo, ApiError> {
        let meta = {
            let g = self.inner.lock().unwrap();
            g.get(id).map(|s| s.meta.clone()).ok_or_else(|| ApiError::not_found(format!("session {id}")))?
        };
        if !matches!(meta.agent, AgentKind::Codex | AgentKind::Claude | AgentKind::Pi) {
            return Err(ApiError::invalid("only agent sessions can continue as chat"));
        }
        let resume = match meta.agent_session.clone() {
            Some(r) => Some(r),
            None => {
                // The TUI did not tell us its session id: take the agent's most recent
                // session in this folder that started after this terminal session.
                let hist = yonder_agents::list_agent_history(meta.agent, Some(&meta.cwd)).await.unwrap_or_default();
                hist.into_iter()
                    .filter(|h| h.updated_at.map(|t| t + 5_000 >= meta.created_at).unwrap_or(false))
                    .max_by_key(|h| h.updated_at.unwrap_or(0))
                    .map(|h| h.id)
            }
        };
        let Some(resume) = resume else {
            return Err(ApiError::not_found("could not find the agent's session id to resume"));
        };
        if meta.kind == SessionKind::Terminal {
            let _ = self.kill(id).await;
            // Let the TUI flush its session file before the chat adapter resumes it.
            for _ in 0..40 {
                if !self.get(id).map(|s| matches!(s.state, SessionState::Running | SessionState::Starting)).unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        let spec = SessionSpec {
            kind: Some(SessionKind::Chat),
            agent: Some(meta.agent),
            cwd: Some(meta.cwd.clone()),
            title: Some(meta.title.clone()),
            model: meta.model.clone(),
            approval: Some(meta.approval),
            resume: Some(resume),
            ..Default::default()
        };
        self.create(spec, SessionOrigin::Remote).await
    }

    /// Stop watching (daemon shutdown); supervisors keep running.
    pub fn detach_all(&self) {
        let mut g = self.inner.lock().unwrap();
        for s in g.values_mut() {
            s.attached.clear();
        }
        self.clients.lock().unwrap().clear();
    }

    pub fn clients_attached(&self, id: &str) -> Vec<u64> {
        self.inner.lock().unwrap().get(id).map(|s| s.attached.keys().copied().collect()).unwrap_or_default()
    }

    pub fn exists(&self, id: &str) -> bool {
        self.inner.lock().unwrap().contains_key(id)
    }

    pub fn attached_sessions(&self, client: u64) -> HashSet<String> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.attached.contains_key(&client))
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn session_dir(&self, id: &str) -> Option<PathBuf> {
        self.inner.lock().unwrap().get(id).map(|s| s.dir.clone())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

async fn chat_alive(dir: &Path, id: &str) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), async {
            let c = ChatClient::connect(dir, id).await.ok()?;
            c.info().await.ok()
        })
        .await,
        Ok(Some(_))
    )
}

fn terminal_log_end(dir: &Path) -> u64 {
    let base = yonder_pty_log_base(dir);
    let len = std::fs::metadata(dir.join(yonder_pty::LOG_FILE)).map(|m| m.len()).unwrap_or(0);
    base + len
}

fn yonder_pty_log_base(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join(yonder_pty::LOG_BASE_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Last non-empty line of an exited terminal's output (rough: strips escapes).
fn terminal_preview(dir: &Path) -> Option<String> {
    let end = terminal_log_end(dir);
    let from = end.saturating_sub(4096).max(yonder_pty_log_base(dir));
    let bytes = yonder_pty::log::read_range(dir, from, 4096).ok().flatten()?;
    let text = yonder_agents::common::strip_ansi(&String::from_utf8_lossy(&bytes));
    text.lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c.is_control()))
        .rfind(|l| !l.is_empty())
        .map(|l| one_line(l, 140))
}

fn default_title(agent: AgentKind, argv: &[String], cwd: &Path) -> String {
    let folder = cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| cwd.to_string_lossy().into_owned());
    match agent {
        AgentKind::Shell => format!("shell · {folder}"),
        AgentKind::Custom => {
            let prog = argv.first().map(|p| {
                Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.clone())
            });
            format!("{} · {folder}", prog.unwrap_or_else(|| "command".into()))
        }
        a => format!("{} · {folder}", a.as_str()),
    }
}

/// Map the approval mode onto the agent's own CLI flags for terminal (TUI) sessions.
fn terminal_approval_flags(agent: AgentKind, approval: ApprovalMode, argv: &mut Vec<String>) {
    let flags: Vec<&str> = match (agent, approval) {
        (_, ApprovalMode::Ask) => vec![],
        (AgentKind::Codex, ApprovalMode::Auto) => vec!["--sandbox", "workspace-write", "--ask-for-approval", "on-request"],
        (AgentKind::Codex, ApprovalMode::Yolo) => vec!["--dangerously-bypass-approvals-and-sandbox"],
        (AgentKind::Claude, ApprovalMode::Auto) => vec!["--permission-mode", "acceptEdits"],
        (AgentKind::Claude, ApprovalMode::Yolo) => vec!["--dangerously-skip-permissions"],
        _ => vec![],
    };
    // Insert right after the program (before a `resume` subcommand for codex).
    let at = 1.min(argv.len());
    for (i, f) in flags.iter().enumerate() {
        argv.insert(at + i, f.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_expires_unless_repeated() {
        let at = std::time::Instant::now();
        assert!(focus_current("s1", at, "s1", at + Duration::from_secs(10)));
        assert!(!focus_current("s1", at, "s2", at));
        assert!(!focus_current("s1", at, "s1", at + FOCUS_TTL));
    }

    #[test]
    fn approval_flags() {
        let mut v = vec!["codex".to_string(), "resume".into(), "t1".into()];
        terminal_approval_flags(AgentKind::Codex, ApprovalMode::Yolo, &mut v);
        assert_eq!(v, vec!["codex", "--dangerously-bypass-approvals-and-sandbox", "resume", "t1"]);
        let mut v = vec!["claude".to_string()];
        terminal_approval_flags(AgentKind::Claude, ApprovalMode::Auto, &mut v);
        assert_eq!(v, vec!["claude", "--permission-mode", "acceptEdits"]);
        let mut v = vec!["pi".to_string()];
        terminal_approval_flags(AgentKind::Pi, ApprovalMode::Yolo, &mut v);
        assert_eq!(v, vec!["pi"]);
    }

    #[test]
    fn titles() {
        assert_eq!(default_title(AgentKind::Codex, &[], Path::new("/x/proj")), "codex · proj");
        assert_eq!(default_title(AgentKind::Custom, &["/usr/bin/htop".into()], Path::new("/x/y")), "htop · y");
    }
}
