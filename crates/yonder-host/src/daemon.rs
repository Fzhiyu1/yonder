//! The host daemon: shared state and request handling.
//!
//! Requests arrive from encrypted relay links ([`crate::relay_link`]) and from the local
//! control socket ([`crate::control`]); both end up in [`Daemon::handle`] with a
//! [`Caller`] that says who is asking and what they may do.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};
use yonder_notify::{Notification, Notifier, NotifyKind};
use yonder_proto::app::{
    AgentAvailability, AgentKind, ApiError, Event, HostInfo, HostMsg, HostStatus, NoticeLevel, Request, Response,
    SessionKind, SessionOrigin, FS_READ_MAX, PERM_FILES, PERM_SESSIONS,
};
use yonder_proto::keys::{Keypair, PublicKey};
use yonder_proto::pairing::PairPayload;

use crate::config::{default_permissions, Config, Device, Devices, Paths};
use crate::sessions::{ClientHandle, Launcher, SessionOptions, SessionSignal, Sessions};
use crate::util::{b64, now_ms, unb64};

/// Default lifetime of a pairing token.
pub const PAIR_TTL: Duration = Duration::from_secs(10 * 60);

/// Who sent a request.
#[derive(Clone)]
pub enum Caller {
    /// Local control socket: the host's own user. Everything allowed.
    Local,
    /// A paired device over the relay.
    Device { key: PublicKey, permissions: Vec<String> },
}

impl Caller {
    fn can(&self, perm: &str) -> bool {
        match self {
            Caller::Local => true,
            Caller::Device { permissions, .. } => permissions.iter().any(|p| p == perm),
        }
    }

    fn label(&self) -> String {
        match self {
            Caller::Local => "local".into(),
            Caller::Device { key, .. } => key.to_b64(),
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PairToken {
    permissions: Vec<String>,
    exp: u64,
}

/// Unused pairing codes: in memory, mirrored to `pairing.json` (0600) so a QR code shown
/// before a daemon restart or upgrade still works afterwards. Expired codes are dropped.
struct PairTokens {
    path: PathBuf,
    map: HashMap<String, PairToken>,
}

impl PairTokens {
    fn load(path: PathBuf) -> Self {
        let now = now_ms();
        let map = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice::<HashMap<String, PairToken>>(&b).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        let mut t = Self { path, map };
        let before = t.map.len();
        t.map.retain(|_, v| v.exp > now);
        if t.map.len() != before {
            t.save();
        }
        t
    }

    fn save(&self) {
        let r = if self.map.is_empty() {
            match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(anyhow::Error::from(e)),
                _ => Ok(()),
            }
        } else {
            serde_json::to_vec(&self.map).map_err(anyhow::Error::from).and_then(|b| crate::util::write_private(&self.path, &b))
        };
        if let Err(e) = r {
            tracing::warn!("saving {}: {e:#}", self.path.display());
        }
    }

    fn insert(&mut self, token: String, t: PairToken) {
        let now = now_ms();
        self.map.retain(|_, v| v.exp > now);
        self.map.insert(token, t);
        self.save();
    }

    /// Consumes a code (single use). Returns it only if it has not expired.
    fn take(&mut self, token: &str) -> Option<PairToken> {
        let t = self.map.remove(token)?;
        self.save();
        (t.exp > now_ms()).then_some(t)
    }
}

/// Relay connection status, updated by the relay task.
#[derive(Debug, Clone, Default)]
pub struct RelayState {
    pub connected: bool,
    pub error: Option<String>,
}

pub struct Daemon {
    pub paths: Paths,
    pub key: Keypair,
    cfg: Mutex<Config>,
    pub devices: Mutex<Devices>,
    pub sessions: Arc<Sessions>,
    pub fs: Arc<yonder_fs::FsService>,
    pub notifier: Arc<Notifier>,
    pair_tokens: Mutex<PairTokens>,
    pub relay_state: Mutex<RelayState>,
    started: Instant,
    next_client: AtomicU64,
    agents: Mutex<Option<(Instant, Vec<AgentAvailability>)>>,
    /// Close signal per device key (revocation).
    revoked_tx: watch::Sender<Vec<PublicKey>>,
    pub shutdown: tokio::sync::Notify,
}

fn fs_service(paths: &Paths, cfg: &Config) -> yonder_fs::FsService {
    let svc = yonder_fs::FsService::new(yonder_fs::FsConfig {
        roots: cfg.fs_roots.iter().map(PathBuf::from).collect(),
        deny: cfg.fs_deny.iter().map(PathBuf::from).collect(),
        upload_dir: paths.uploads_dir(),
        fallback_trash: paths.trash_dir(),
        audit_log: Some(paths.audit_log()),
    });
    // Tests (and users who prefer it) can keep deleted files in the private trash dir
    // instead of the OS trash.
    if std::env::var_os("YONDER_PRIVATE_TRASH").is_some() {
        svc.with_trash(|_| Err("private trash".into()))
    } else {
        svc
    }
}

pub fn session_options(cfg: &Config) -> SessionOptions {
    let mut o = SessionOptions { login_shell: cfg.login_shell, ..Default::default() };
    if let Some(secs) = std::env::var("YONDER_LINGER_SECS").ok().and_then(|s| s.parse().ok()) {
        o.linger_secs = secs;
    }
    for agent in [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi] {
        if let Some(ov) = cfg.agent_override(agent) {
            if let Some(p) = ov.program.clone().filter(|p| !p.is_empty()) {
                o.agent_program.insert(agent, p);
            }
            if !ov.env.is_empty() {
                o.agent_env.insert(agent, ov.env.clone());
            }
        }
    }
    o
}

impl Daemon {
    /// Build the daemon (no network yet). Returns the signal receiver for notifications.
    pub fn new(paths: Paths, cfg: Config, launcher: Launcher) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<SessionSignal>)> {
        paths.ensure()?;
        let key = crate::config::load_or_create_key(&paths)?;
        let devices = Devices::load(&paths.devices_file())?;
        let notifier = Notifier::new(cfg.notify.clone(), &paths.notify_dir()).context("notifier")?;
        notifier.set_app_url(&cfg.web_url());
        let (sig_tx, sig_rx) = mpsc::unbounded_channel();
        let sessions = Sessions::new(paths.sessions_dir(), launcher, session_options(&cfg), sig_tx);
        let fs = Arc::new(fs_service(&paths, &cfg));
        let (revoked_tx, _) = watch::channel(Vec::new());
        let pair_tokens = PairTokens::load(paths.pairing_file());
        let d = Arc::new(Self {
            paths,
            key,
            cfg: Mutex::new(cfg),
            devices: Mutex::new(devices),
            sessions,
            fs,
            notifier: Arc::new(notifier),
            pair_tokens: Mutex::new(pair_tokens),
            relay_state: Mutex::new(RelayState::default()),
            started: Instant::now(),
            next_client: AtomicU64::new(1),
            agents: Mutex::new(None),
            revoked_tx,
            shutdown: tokio::sync::Notify::new(),
        });
        Ok((d, sig_rx))
    }

    pub fn config(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }

    pub fn next_client_id(&self) -> u64 {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    pub fn revoked(&self) -> watch::Receiver<Vec<PublicKey>> {
        self.revoked_tx.subscribe()
    }

    // ------------------------------------------------------------ pairing

    pub fn create_pairing(&self, permissions: Vec<String>, ttl: Duration) -> Result<(PairPayload, String), ApiError> {
        let cfg = self.config();
        let permissions = if permissions.is_empty() { default_permissions() } else { permissions };
        for p in &permissions {
            if p != PERM_SESSIONS && p != PERM_FILES {
                return Err(ApiError::invalid(format!("unknown permission {p}")));
            }
        }
        let token = crate::util::new_token();
        let exp = now_ms() + ttl.as_millis() as u64;
        self.pair_tokens.lock().unwrap().insert(token.clone(), PairToken { permissions, exp });
        let payload = PairPayload {
            v: 1,
            relay: cfg.relay_url.clone(),
            host: self.key.public,
            host_name: cfg.name.clone(),
            token,
            exp,
        };
        let url = payload.to_url(&cfg.web_url()).map_err(|e| ApiError::internal(e.to_string()))?;
        Ok((payload, url))
    }

    /// Authorize a device after a successful Noise handshake.
    /// Returns its permissions, or the `HostHello.error` code.
    pub fn authorize(&self, key: &PublicKey, device_name: &str, client: &str, pair_token: Option<&str>) -> Result<Vec<String>, &'static str> {
        if let Some(d) = self.devices.lock().unwrap().get(key) {
            return Ok(d.permissions.clone());
        }
        let Some(token) = pair_token.filter(|t| !t.is_empty()) else {
            return Err("not_paired");
        };
        let Some(tok) = self.pair_tokens.lock().unwrap().take(token) else {
            return Err("pair_token_invalid");
        };
        let dev = Device {
            public: *key,
            name: sanitize_name(device_name),
            client: sanitize_name(client),
            paired_at: now_ms(),
            last_seen: Some(now_ms()),
            permissions: tok.permissions.clone(),
        };
        if let Err(e) = self.devices.lock().unwrap().add(dev.clone()) {
            tracing::error!("saving devices: {e:#}");
            return Err("internal");
        }
        self.fs.audit(&key.to_b64(), "pair", &dev.name, &Ok(()));
        tracing::info!(device = %key, name = %dev.name, "device paired");
        self.sessions.broadcast(Event::DevicePaired { device: dev.info(false) });
        self.sessions.broadcast(Event::Notice { level: NoticeLevel::Info, message: format!("新设备已配对：{}", dev.name) });
        Ok(tok.permissions)
    }

    pub fn touch_device(&self, key: &PublicKey) {
        self.devices.lock().unwrap().touch(key, now_ms());
    }

    pub fn revoke(&self, key: &PublicKey) -> Result<bool, ApiError> {
        let removed = self.devices.lock().unwrap().remove(key).map_err(|e| ApiError::internal(format!("{e:#}")))?;
        if removed {
            let _ = self.notifier.remove_device(&key.to_b64());
            self.fs.audit("local", "revoke", &key.to_b64(), &Ok(()));
            self.revoked_tx.send_modify(|v| v.push(*key));
        }
        Ok(removed)
    }

    // ------------------------------------------------------------ info

    async fn agents(&self) -> Vec<AgentAvailability> {
        {
            let g = self.agents.lock().unwrap();
            if let Some((t, v)) = g.as_ref() {
                if t.elapsed() < Duration::from_secs(600) {
                    return v.clone();
                }
            }
        }
        let mut v = tokio::task::spawn_blocking(yonder_agents::detect_agents).await.unwrap_or_default();
        // Agents with a configured program override are available when that program is.
        let cfg = self.config();
        for a in v.iter_mut() {
            if let Some(prog) = cfg.agent_override(a.agent).and_then(|o| o.program.clone()).filter(|p| !p.is_empty()) {
                let found = yonder_agents::resolve_program(&prog[0]).or_else(|| {
                    let p = std::path::Path::new(&prog[0]);
                    p.exists().then(|| p.to_path_buf())
                });
                a.available = found.is_some();
                a.path = Some(found.map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| prog.join(" ")));
                if a.version.is_none() {
                    a.version = Some("custom".into());
                }
            }
        }
        // Model lists (best effort, bounded time).
        for a in v.iter_mut().filter(|a| a.available) {
            if cfg.agent_override(a.agent).and_then(|o| o.program.as_ref()).is_some() {
                continue;
            }
            match a.agent {
                AgentKind::Codex => {
                    if let Ok(Ok(info)) = tokio::time::timeout(Duration::from_secs(15), yonder_agents::codex_models()).await {
                        a.models = info.models;
                        a.default_model = info.default_model;
                        a.default_approval = info.approval;
                    }
                }
                AgentKind::Claude => {
                    a.default_approval = tokio::task::spawn_blocking(yonder_agents::claude_default_mode).await.ok().flatten();
                }
                AgentKind::Pi => {
                    if let Ok(Ok(models)) = tokio::time::timeout(Duration::from_secs(15), yonder_agents::pi_models()).await {
                        a.models = models;
                    }
                }
                _ => {}
            }
        }
        *self.agents.lock().unwrap() = Some((Instant::now(), v.clone()));
        v
    }

    /// Warm the agent cache in the background.
    pub fn prefetch_agents(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let v = me.agents().await;
            tracing::info!(
                "agents: {}",
                v.iter().map(|a| format!("{}={}", a.agent.as_str(), a.available)).collect::<Vec<_>>().join(" ")
            );
        });
    }

    async fn host_info(&self, caller: &Caller) -> HostInfo {
        let cfg = self.config();
        let mut agents = self.agents().await;
        agents.push(AgentAvailability {
            agent: AgentKind::Shell,
            available: true,
            version: None,
            path: Some(crate::util::shell_display()),
            chat: false,
            models: vec![],
            default_model: None,
            default_approval: None,
        });
        let permissions = match caller {
            Caller::Local => default_permissions(),
            Caller::Device { permissions, .. } => permissions.clone(),
        };
        let mut recent = self.sessions.recent_dirs();
        let home = self.fs.home();
        if !recent.contains(&home) {
            recent.push(home.clone());
        }
        HostInfo {
            name: cfg.name.clone(),
            hostname: gethostname::gethostname().to_string_lossy().into_owned(),
            os: crate::util::os_name().to_string(),
            arch: std::env::consts::ARCH.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            home: crate::util::home_dir().to_string_lossy().into_owned(),
            shell: crate::util::shell_display(),
            agents,
            recent_dirs: recent,
            permissions,
            fs_roots: self.fs.roots(),
            path_sep: std::path::MAIN_SEPARATOR.to_string(),
            fingerprint: self.key.public.fingerprint(),
            vapid_public: cfg.notify.web_push.then(|| self.notifier.vapid_public()),
        }
    }

    pub fn status(&self) -> HostStatus {
        let cfg = self.config();
        let rs = self.relay_state.lock().unwrap().clone();
        HostStatus {
            name: cfg.name.clone(),
            host_pub: self.key.public.to_b64(),
            fingerprint: self.key.public.fingerprint(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            relay_url: Some(cfg.relay_url.clone()),
            relay_connected: rs.connected,
            relay_error: rs.error,
            clients: self.sessions.client_count() as u32,
            sessions: self.sessions.live_count() as u32,
            uptime_ms: self.started.elapsed().as_millis() as u64,
            web_url: Some(cfg.web_url()),
        }
    }

    // ------------------------------------------------------------ requests

    /// Handle one request. `client` receives events for attach.
    pub async fn handle(self: &Arc<Self>, caller: &Caller, client: &ClientHandle, req: Request) -> Result<Response, ApiError> {
        use Request::*;
        let need = |perm: &str| -> Result<(), ApiError> {
            if caller.can(perm) {
                Ok(())
            } else {
                Err(ApiError::forbidden(format!("this device lacks the '{perm}' permission")))
            }
        };
        match req {
            Ping => Ok(Response::Pong { ts: now_ms() }),
            HostInfo => Ok(Response::HostInfo { info: self.host_info(caller).await }),
            ListSessions => {
                need(PERM_SESSIONS)?;
                Ok(Response::Sessions { sessions: self.sessions.list() })
            }
            CreateSession { mut spec } => {
                need(PERM_SESSIONS)?;
                let origin = match caller {
                    Caller::Local => spec.origin.unwrap_or(SessionOrigin::Local),
                    Caller::Device { .. } => SessionOrigin::Remote,
                };
                spec.origin = Some(origin);
                // A chat without a mode from the client follows the agent's own configuration on
                // this host, so a Codex set to `approval_policy = "never"` does not start asking
                // via yonder. (Terminal agents read their configuration themselves.)
                let chat = spec.kind.map(|k| k == SessionKind::Chat).unwrap_or(spec.command.is_none());
                if spec.approval.is_none() && chat {
                    let agent = spec.agent.unwrap_or(AgentKind::Codex);
                    spec.approval = self.agents().await.iter().find(|a| a.agent == agent).and_then(|a| a.default_approval);
                }
                let s = self.sessions.create(spec, origin).await?;
                Ok(Response::Session { session: s })
            }
            Attach { session, since } => {
                // Normally routed through `on_request` (the response is sent by the
                // session manager); direct callers get the plain list entry.
                need(PERM_SESSIONS)?;
                let _ = since;
                let s = self.sessions.get(&session).ok_or_else(|| ApiError::not_found(format!("session {session}")))?;
                Ok(Response::Session { session: s })
            }
            Detach { session } => {
                self.sessions.detach(&session, client.id);
                Ok(Response::Ok)
            }
            Kill { session } => {
                need(PERM_SESSIONS)?;
                self.sessions.kill(&session).await?;
                Ok(Response::Ok)
            }
            Remove { session } => {
                need(PERM_SESSIONS)?;
                self.sessions.remove(&session).await?;
                Ok(Response::Ok)
            }
            Rename { session, title } => {
                need(PERM_SESSIONS)?;
                Ok(Response::Session { session: self.sessions.rename(&session, &title)? })
            }
            ContinueAsChat { session } => {
                need(PERM_SESSIONS)?;
                Ok(Response::Session { session: self.sessions.continue_as_chat(&session).await? })
            }
            ChatSend { session, text, attachments } => {
                need(PERM_SESSIONS)?;
                // Attachments must be files this device may read (uploads are always allowed).
                for a in &attachments {
                    let fs = self.fs.clone();
                    let a2 = a.clone();
                    tokio::task::spawn_blocking(move || fs.stat(&a2))
                        .await
                        .map_err(|e| ApiError::internal(e.to_string()))?
                        .map_err(|e| ApiError::new(&e.code, format!("attachment {a}: {}", e.message)))?;
                }
                self.sessions.chat_send(&session, text, attachments).await?;
                Ok(Response::Ok)
            }
            ChatInterrupt { session } => {
                need(PERM_SESSIONS)?;
                self.sessions.chat_interrupt(&session).await?;
                Ok(Response::Ok)
            }
            ApprovalRespond { session, approval, option } => {
                need(PERM_SESSIONS)?;
                self.sessions.approval_respond(&session, approval, option).await?;
                Ok(Response::Ok)
            }
            SetApprovalMode { session, mode } => {
                need(PERM_SESSIONS)?;
                let s = self.sessions.set_approval_mode(&session, mode).await?;
                self.sessions.broadcast(yonder_proto::app::Event::SessionUpdated { session: s.clone() });
                self.fs.audit(&caller.label(), "approval_mode", &format!("{session} {}", mode.as_str()), &Ok(()));
                Ok(Response::Session { session: s })
            }
            SetChatModel { session, model } => {
                need(PERM_SESSIONS)?;
                let s = self.sessions.set_chat_model(&session, &model).await?;
                self.sessions.broadcast(yonder_proto::app::Event::SessionUpdated { session: s.clone() });
                self.fs.audit(&caller.label(), "chat_model", &format!("{session} {model}"), &Ok(()));
                Ok(Response::Session { session: s })
            }
            HttpFetch { url } => {
                need(PERM_FILES)?;
                let r = crate::httpget::get(&url).await?;
                Ok(Response::HttpResponse { status: r.status, headers: r.headers, data: b64(&r.body) })
            }
            TailnetUrl { url } => {
                need(PERM_FILES)?;
                let (url, reachable) = crate::httpget::tailnet_url(&url).await?;
                Ok(Response::TailnetUrl { url, reachable })
            }
            AgentHistory { agent, cwd, query, cursor, limit, all } => {
                need(PERM_SESSIONS)?;
                if let Some(a) = agent.filter(|a| !matches!(a, AgentKind::Codex | AgentKind::Claude | AgentKind::Pi)) {
                    return Ok(Response::AgentHistory { sessions: vec![], next_cursor: None, folders: vec![], errors: vec![format!("{} has no history", a.as_str())] });
                }
                let q = yonder_agents::HistoryQuery { agent, cwd, query, cursor, limit: limit.map(|l| l as usize), all };
                let page = tokio::time::timeout(Duration::from_secs(30), yonder_agents::list_history(&q)).await.map_err(|_| ApiError::busy("agent history timed out"))?;
                if agent.is_some() && page.sessions.is_empty() && !page.errors.is_empty() {
                    return Err(ApiError::internal(page.errors.join("; ")));
                }
                Ok(Response::AgentHistory { sessions: page.sessions, next_cursor: page.next_cursor, folders: page.folders, errors: page.errors })
            }
            ChatOlder { session, before, limit } => {
                need(PERM_SESSIONS)?;
                let limit = limit.unwrap_or(crate::chatlog::PAGE_ITEMS as u32).clamp(1, 200) as usize;
                let (items, more) = self.sessions.chat_older(&session, &before, limit)?;
                Ok(Response::ChatOlder { items, more })
            }
            AgentPreview { agent, id } => {
                need(PERM_SESSIONS)?;
                let (items, truncated) = tokio::time::timeout(Duration::from_secs(30), yonder_agents::agent_preview(agent, &id))
                    .await
                    .map_err(|_| ApiError::busy("agent preview timed out"))?
                    .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
                Ok(Response::AgentPreview { items, truncated })
            }
            FsHome => {
                need(PERM_FILES)?;
                Ok(Response::Path { path: self.fs.home() })
            }
            FsList { path, hidden } => {
                need(PERM_FILES)?;
                let listing = self.fs_blocking(move |fs| fs.list(&path, hidden)).await?;
                Ok(Response::Dir { listing })
            }
            FsStat { path } => {
                need(PERM_FILES)?;
                let entry = self.fs_blocking(move |fs| fs.stat(&path)).await?;
                Ok(Response::Stat { entry })
            }
            FsRead { path, offset, len } => {
                need(PERM_FILES)?;
                let p2 = path.clone();
                let who = caller.label();
                let chunk = self
                    .fs_blocking(move |fs| {
                        let r = fs.read(&p2, offset, len.min(FS_READ_MAX));
                        if offset == 0 {
                            fs.audit(&who, "read", &p2, &r.as_ref().map(|_| ()).map_err(|e| e.clone()));
                        }
                        r
                    })
                    .await?;
                Ok(Response::FileChunk { path, offset, data: b64(&chunk.data), eof: chunk.eof, size: chunk.size })
            }
            FsWrite { path, offset, data, finish, overwrite } => {
                need(PERM_FILES)?;
                let bytes = unb64(&data).map_err(|e| ApiError::invalid(e.to_string()))?;
                let who = caller.label();
                self.fs_blocking(move |fs| {
                    let r = fs.write(&path, offset, &bytes, finish, overwrite);
                    if finish || r.is_err() {
                        fs.audit(&who, "write", &path, &r);
                    }
                    r
                })
                .await?;
                Ok(Response::Ok)
            }
            FsMkdir { path } => {
                need(PERM_FILES)?;
                let who = caller.label();
                self.fs_blocking(move |fs| {
                    let r = fs.mkdir(&path);
                    fs.audit(&who, "mkdir", &path, &r);
                    r
                })
                .await?;
                Ok(Response::Ok)
            }
            FsRename { from, to, overwrite } => {
                need(PERM_FILES)?;
                let who = caller.label();
                self.fs_blocking(move |fs| {
                    let r = fs.rename(&from, &to, overwrite);
                    fs.audit(&who, "rename", &format!("{from} -> {to}"), &r);
                    r
                })
                .await?;
                Ok(Response::Ok)
            }
            FsDelete { path } => {
                need(PERM_FILES)?;
                let who = caller.label();
                self.fs_blocking(move |fs| {
                    let r = fs.delete(&path);
                    fs.audit(&who, "delete", &path, &r);
                    r
                })
                .await?;
                Ok(Response::Ok)
            }
            UploadTemp { name, data } => {
                // Chat attachments: allowed with either permission.
                if !caller.can(PERM_SESSIONS) && !caller.can(PERM_FILES) {
                    return Err(ApiError::forbidden("no permission to upload"));
                }
                let bytes = unb64(&data).map_err(|e| ApiError::invalid(e.to_string()))?;
                let who = caller.label();
                let path = self
                    .fs_blocking(move |fs| {
                        let r = fs.upload_temp(&name, &bytes);
                        fs.audit(&who, "upload_temp", &name, &r.as_ref().map(|_| ()).map_err(|e| e.clone()));
                        r
                    })
                    .await?;
                Ok(Response::Path { path })
            }
            ListDevices => {
                let me = match caller {
                    Caller::Device { key, .. } => Some(*key),
                    Caller::Local => None,
                };
                let devices = self.devices.lock().unwrap().list().iter().map(|d| d.info(Some(d.public) == me)).collect();
                Ok(Response::Devices { devices })
            }
            RevokeDevice { device } => {
                let key = PublicKey::from_b64(&device).map_err(|e| ApiError::invalid(e.to_string()))?;
                if !self.revoke(&key)? {
                    return Err(ApiError::not_found("no such device"));
                }
                Ok(Response::Ok)
            }
            CreatePairing { permissions, ttl_secs } => {
                if !matches!(caller, Caller::Local) {
                    return Err(ApiError::forbidden("pairing can only be started on the host"));
                }
                let ttl = ttl_secs.map(Duration::from_secs).unwrap_or(PAIR_TTL).min(Duration::from_secs(24 * 3600));
                let (payload, url) = self.create_pairing(permissions, ttl)?;
                Ok(Response::Pairing { payload, url })
            }
            Status => {
                if !matches!(caller, Caller::Local) {
                    return Err(ApiError::forbidden("local only"));
                }
                Ok(Response::Status { status: self.status() })
            }
            Shutdown => {
                if !matches!(caller, Caller::Local) {
                    return Err(ApiError::forbidden("local only"));
                }
                self.shutdown.notify_waiters();
                Ok(Response::Ok)
            }
            NotifyTest => {
                let cfg = self.config();
                if !self.notifier.has_targets() {
                    return Err(ApiError::invalid("no notification channel is configured and no browser is subscribed"));
                }
                let n = Notification {
                    kind: NotifyKind::Test,
                    host: cfg.name.clone(),
                    session: None,
                    session_title: None,
                    title: "测试通知".into(),
                    body: format!("来自 {} 的测试通知", cfg.name),
                    url: Some(format!("{}/#/settings", cfg.web_url())),
                    tag: Some("yonder-test".into()),
                };
                let results = self.notifier.notify(&n).await;
                let failed: Vec<String> =
                    results.iter().filter_map(|(l, r)| r.as_ref().err().map(|e| format!("{l}: {e:#}"))).collect();
                if !failed.is_empty() && failed.len() == results.len() {
                    return Err(ApiError::internal(failed.join("; ")));
                }
                if !failed.is_empty() {
                    self.sessions.broadcast(Event::Notice { level: NoticeLevel::Warning, message: failed.join("; ") });
                }
                Ok(Response::Ok)
            }
            PushSubscribe { endpoint, p256dh, auth, vapid_private } => {
                let Caller::Device { key, .. } = caller else {
                    return Err(ApiError::invalid("push subscriptions belong to a device"));
                };
                self.notifier
                    .subscribe(&key.to_b64(), &endpoint, &p256dh, &auth, vapid_private.as_deref())
                    .map_err(|e| ApiError::invalid(format!("{e:#}")))?;
                Ok(Response::Ok)
            }
            PushUnsubscribe => {
                if let Caller::Device { key, .. } = caller {
                    self.notifier.remove_device(&key.to_b64()).map_err(|e| ApiError::internal(format!("{e:#}")))?;
                }
                Ok(Response::Ok)
            }
        }
    }

    async fn fs_blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&yonder_fs::FsService) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let fs = self.fs.clone();
        tokio::task::spawn_blocking(move || f(&fs)).await.map_err(|e| ApiError::internal(e.to_string()))?
    }

    /// Dispatch one app message from a client; returns the response to send (if any).
    pub async fn on_client_msg(self: &Arc<Self>, caller: &Caller, client: &ClientHandle, msg: yonder_proto::app::ClientMsg) -> Option<HostMsg> {
        use yonder_proto::app::ClientMsg;
        match msg {
            ClientMsg::Req { id, req } => self.on_request(caller, client, id, req).await,
            ClientMsg::Input { session, data } => {
                if caller.can(PERM_SESSIONS) && self.sessions.clients_attached(&session).contains(&client.id) {
                    if let Ok(bytes) = unb64(&data) {
                        self.sessions.input(&session, bytes).await;
                    }
                }
                None
            }
            ClientMsg::Resize { session, cols, rows } => {
                if caller.can(PERM_SESSIONS) && self.sessions.clients_attached(&session).contains(&client.id) {
                    self.sessions.resize(&session, cols, rows).await;
                }
                None
            }
            ClientMsg::Focus { session } => {
                self.sessions.set_focus(client.id, session);
                None
            }
        }
    }

    /// One request; returns the response, or None when it was already sent (attach).
    pub async fn on_request(self: &Arc<Self>, caller: &Caller, client: &ClientHandle, id: u64, req: Request) -> Option<HostMsg> {
        if let Request::Attach { session, since } = req {
            if !caller.can(PERM_SESSIONS) {
                return Some(HostMsg::err(id, ApiError::forbidden("this device lacks the 'sessions' permission")));
            }
            self.sessions.attach(&session, client, since, id).await;
            return None;
        }
        Some(match self.handle(caller, client, req).await {
            Ok(data) => HostMsg::ok(id, data),
            Err(e) => HostMsg::err(id, e),
        })
    }

    // ------------------------------------------------------------ notifications

    /// Turn session signals into notifications (skipping sessions someone is watching).
    pub fn spawn_notifier(self: &Arc<Self>, mut rx: mpsc::UnboundedReceiver<SessionSignal>) {
        let me = self.clone();
        tokio::spawn(async move {
            while let Some(sig) = rx.recv().await {
                let (kind, session, title, body) = match sig {
                    SessionSignal::Approval { session, title, body } => (NotifyKind::Approval, session, title, body),
                    SessionSignal::TurnDone { session, title, body } => (NotifyKind::TurnDone, session, title, body),
                    SessionSignal::Exited { session, title, body } => (NotifyKind::Exited, session, title, body),
                };
                if me.sessions.is_focused(&session) {
                    tracing::info!(%session, ?kind, "notification skipped: session is on screen");
                    continue;
                }
                if !me.notifier.has_targets() {
                    continue;
                }
                let cfg = me.config();
                let heading = match kind {
                    NotifyKind::Approval => format!("需要审批 · {title}"),
                    NotifyKind::TurnDone => format!("已完成 · {title}"),
                    NotifyKind::Exited => format!("已结束 · {title}"),
                    NotifyKind::Test => title.clone(),
                };
                let n = Notification {
                    kind,
                    host: cfg.name.clone(),
                    session: Some(session.clone()),
                    session_title: Some(title.clone()),
                    title: heading,
                    body: if body.is_empty() { title.clone() } else { body },
                    url: Some(format!("{}/#/h/{}/s/{}", cfg.web_url(), me.key.public.to_b64(), session)),
                    tag: Some(format!("{}-{}", session, kind_tag(kind))),
                };
                let me2 = me.clone();
                tokio::spawn(async move {
                    let _ = me2.notifier.notify(&n).await;
                });
            }
        });
    }
}

fn kind_tag(k: NotifyKind) -> &'static str {
    match k {
        NotifyKind::Approval => "approval",
        NotifyKind::TurnDone => "done",
        NotifyKind::Exited => "exit",
        NotifyKind::Test => "test",
    }
}

fn sanitize_name(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(64).collect();
    let t = t.trim().to_string();
    if t.is_empty() {
        "device".into()
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon(paths: &Paths) -> Arc<Daemon> {
        let launcher = Launcher { program: "yonder".into(), pty_prefix: Vec::new(), chat_prefix: Vec::new() };
        Daemon::new(paths.clone(), Config::default_for_host(), launcher).unwrap().0
    }

    #[tokio::test]
    async fn pairing_code_survives_a_restart_and_works_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config_dir: dir.path().join("c"), data_dir: dir.path().join("d") };
        let phone = Keypair::generate().unwrap();
        let token = {
            let d = daemon(&paths);
            let (payload, _) = d.create_pairing(Vec::new(), Duration::from_secs(600)).unwrap();
            let (expired, _) = d.create_pairing(Vec::new(), Duration::from_millis(1)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            // An expired code is refused, even though it is still on disk.
            assert_eq!(d.authorize(&Keypair::generate().unwrap().public, "x", "web", Some(&expired.token)), Err("pair_token_invalid"));
            payload.token
        };
        assert!(paths.pairing_file().exists());

        // A new daemon process (restart / upgrade) accepts the code shown before...
        let d = daemon(&paths);
        let perms = d.authorize(&phone.public, "iPhone", "web", Some(&token)).unwrap();
        assert_eq!(perms, default_permissions());
        // ...exactly once, and the used code is gone from disk.
        assert_eq!(d.authorize(&Keypair::generate().unwrap().public, "other", "web", Some(&token)), Err("pair_token_invalid"));
        assert!(!paths.pairing_file().exists(), "no codes left, file removed");
        // The paired device itself keeps working without a code.
        assert!(d.authorize(&phone.public, "iPhone", "web", None).is_ok());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let (_, _) = d.create_pairing(Vec::new(), Duration::from_secs(60)).unwrap();
            let mode = std::fs::metadata(paths.pairing_file()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
