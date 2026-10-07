//! Host-side notifications for yonder: ntfy, Bark, generic webhooks and Web Push.
//!
//! Web Push follows RFC 8291 (message encryption, `aes128gcm`) and RFC 8292 (VAPID).
//! Only the host holds the VAPID key; subscriptions come from paired devices over the
//! encrypted channel. Message text is only sent to third-party channels when
//! `include_content` is enabled; otherwise they get a generic body.

mod webpush;

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

pub use webpush::{encrypt_aes128gcm, vapid_authorization, VapidKey};

const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const DEBOUNCE: Duration = Duration::from_secs(5);
/// VAPID contact when neither `vapid_subject` nor a usable app URL is known.
pub const DEFAULT_VAPID_SUBJECT: &str = "https://github.com/Fzhiyu1/yonder";

fn default_ntfy_server() -> String {
    "https://ntfy.sh".into()
}
fn default_bark_server() -> String {
    "https://api.day.app".into()
}
fn default_true() -> bool {
    true
}

/// One third-party notification channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelConfig {
    Ntfy {
        #[serde(default = "default_ntfy_server")]
        server: String,
        topic: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    Bark {
        #[serde(default = "default_bark_server")]
        server: String,
        key: String,
    },
    Webhook {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl ChannelConfig {
    pub fn label(&self) -> String {
        match self {
            ChannelConfig::Ntfy { topic, .. } => format!("ntfy:{topic}"),
            ChannelConfig::Bark { .. } => "bark".into(),
            ChannelConfig::Webhook { url, .. } => {
                format!("webhook:{}", origin_of(url).unwrap_or_else(|| url.clone()))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyConfig {
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    /// Send message text (agent output, commands) to third-party channels.
    #[serde(default)]
    pub include_content: bool,
    /// Deliver Web Push to subscribed browsers. The payload is end-to-end encrypted to
    /// the browser, so it always carries the full text.
    #[serde(default = "default_true")]
    pub web_push: bool,
    /// HTTP(S) proxy for outgoing notification requests, e.g. `http://127.0.0.1:7897`.
    /// Without it the standard `HTTPS_PROXY` / `ALL_PROXY` environment variables apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Contact sent to push services in the VAPID `sub` claim (`mailto:` or `https:` URI).
    /// Default: the web client URL. Apple's push service rejects placeholders such as
    /// `mailto:x@localhost` or `.local` / `.test` hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vapid_subject: Option<String>,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self { channels: Vec::new(), include_content: false, web_push: true, proxy: None, vapid_subject: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyKind {
    Approval,
    TurnDone,
    Exited,
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub kind: NotifyKind,
    /// Host display name.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    pub title: String,
    pub body: String,
    /// Deep link opened when the notification is tapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Replaces earlier notifications with the same tag (Web Push).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

impl Notification {
    /// Text safe for third-party channels when content is not allowed.
    pub fn generic_body(&self) -> String {
        let what = match self.kind {
            NotifyKind::Approval => "needs your approval",
            NotifyKind::TurnDone => "finished its turn",
            NotifyKind::Exited => "exited",
            NotifyKind::Test => "test notification",
        };
        match &self.session_title {
            Some(t) if self.kind != NotifyKind::Test => format!("{t}: {what}"),
            _ => what.to_string(),
        }
    }
}

/// A Web Push subscription registered by a paired device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSubscription {
    /// Device public key (base64url).
    pub device: String,
    pub endpoint: String,
    /// base64url, uncompressed P-256 point.
    pub p256dh: String,
    /// base64url, 16 bytes.
    pub auth: String,
    /// Device-provided VAPID private key (base64url scalar) the subscription was created
    /// with. None: the host's own key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vapid_private: Option<String>,
    pub created_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct PushStore {
    subscriptions: Vec<PushSubscription>,
}

type Job<'a> = Pin<Box<dyn Future<Output = (String, Result<()>)> + Send + 'a>>;

pub struct Notifier {
    cfg: Mutex<NotifyConfig>,
    vapid: VapidKey,
    push_path: PathBuf,
    subs: Mutex<Vec<PushSubscription>>,
    recent: Mutex<HashMap<(NotifyKind, Option<String>), Instant>>,
    http: Mutex<reqwest::Client>,
    /// Web client URL, the default VAPID contact when it is a public https URL.
    app_url: Mutex<Option<String>>,
}

impl Notifier {
    /// Loads (or creates) `vapid.key` and `push.json` in `state_dir`.
    pub fn new(cfg: NotifyConfig, state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create {}", state_dir.display()))?;
        let vapid = VapidKey::load_or_create(&state_dir.join("vapid.key"))?;
        let push_path = state_dir.join("push.json");
        let subs = match std::fs::read(&push_path) {
            Ok(b) => match serde_json::from_slice::<PushStore>(&b) {
                Ok(s) => s.subscriptions,
                Err(e) => {
                    tracing::warn!("ignoring unreadable {}: {e}", push_path.display());
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", push_path.display())),
        };
        let http = build_client(cfg.proxy.as_deref())?;
        Ok(Self {
            cfg: Mutex::new(cfg),
            vapid,
            push_path,
            subs: Mutex::new(subs),
            recent: Mutex::new(HashMap::new()),
            http: Mutex::new(http),
            app_url: Mutex::new(None),
        })
    }

    pub fn config(&self) -> NotifyConfig {
        self.cfg.lock().unwrap().clone()
    }

    /// Sets the web client URL, used as the VAPID contact unless `vapid_subject` is set.
    pub fn set_app_url(&self, url: &str) {
        *self.app_url.lock().unwrap() = Some(url.trim_end_matches('/').to_string());
    }

    /// The VAPID `sub` claim: `vapid_subject`, else the app URL when push services will
    /// accept it, else the project URL.
    pub fn vapid_subject(&self) -> String {
        if let Some(s) = self.cfg.lock().unwrap().vapid_subject.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return s.to_string();
        }
        match self.app_url.lock().unwrap().as_deref() {
            Some(u) if is_public_https(u) => u.to_string(),
            _ => DEFAULT_VAPID_SUBJECT.to_string(),
        }
    }

    pub fn set_config(&self, cfg: NotifyConfig) -> Result<()> {
        let http = build_client(cfg.proxy.as_deref())?;
        *self.http.lock().unwrap() = http;
        *self.cfg.lock().unwrap() = cfg;
        Ok(())
    }

    fn client(&self) -> reqwest::Client {
        self.http.lock().unwrap().clone()
    }

    /// base64url (no padding) of the 65-byte uncompressed VAPID public key, as used by
    /// `PushManager.subscribe({ applicationServerKey })`.
    pub fn vapid_public(&self) -> String {
        self.vapid.public_b64()
    }

    pub fn subscriptions(&self) -> Vec<PushSubscription> {
        self.subs.lock().unwrap().clone()
    }

    /// Adds or replaces (by endpoint) a Web Push subscription. `vapid_private` is the
    /// device's VAPID key when the subscription was not made with this host's key.
    pub fn subscribe(
        &self,
        device: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        vapid_private: Option<&str>,
    ) -> Result<()> {
        let url = reqwest::Url::parse(endpoint).map_err(|e| anyhow!("invalid endpoint: {e}"))?;
        if url.scheme() != "https" {
            return Err(anyhow!("push endpoint must be https"));
        }
        let ua = webpush::b64d(p256dh).context("p256dh")?;
        p256::PublicKey::from_sec1_bytes(&ua).map_err(|_| anyhow!("p256dh is not a P-256 point"))?;
        if webpush::b64d(auth).context("auth")?.len() != 16 {
            return Err(anyhow!("auth secret must be 16 bytes"));
        }
        if let Some(k) = vapid_private {
            VapidKey::from_private(&webpush::b64d(k).context("vapid_private")?)?;
        }
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| s.endpoint != endpoint);
        subs.push(PushSubscription {
            device: device.into(),
            endpoint: endpoint.into(),
            p256dh: p256dh.into(),
            auth: auth.into(),
            vapid_private: vapid_private.map(str::to_string),
            created_at: now_ms(),
        });
        self.save(&subs)
    }

    /// Drops every subscription of a device (e.g. when it is revoked).
    pub fn remove_device(&self, device: &str) -> Result<()> {
        let mut subs = self.subs.lock().unwrap();
        let before = subs.len();
        subs.retain(|s| s.device != device);
        if subs.len() != before {
            self.save(&subs)?;
        }
        Ok(())
    }

    fn remove_endpoint(&self, endpoint: &str) {
        let mut subs = self.subs.lock().unwrap();
        let before = subs.len();
        subs.retain(|s| s.endpoint != endpoint);
        if subs.len() != before {
            if let Err(e) = self.save(&subs) {
                tracing::warn!("saving push subscriptions: {e:#}");
            }
        }
    }

    fn save(&self, subs: &[PushSubscription]) -> Result<()> {
        let store = PushStore { subscriptions: subs.to_vec() };
        write_private(&self.push_path, &serde_json::to_vec_pretty(&store)?)
    }

    /// True when at least one channel or push subscription would receive notifications.
    pub fn has_targets(&self) -> bool {
        let cfg = self.cfg.lock().unwrap();
        !cfg.channels.is_empty() || (cfg.web_push && !self.subs.lock().unwrap().is_empty())
    }

    /// Returns false when an identical (kind, session) notification was sent recently.
    fn debounce(&self, n: &Notification) -> bool {
        if n.kind == NotifyKind::Test {
            return true;
        }
        let key = (n.kind, n.session.clone());
        let now = Instant::now();
        let mut recent = self.recent.lock().unwrap();
        recent.retain(|_, t| now.duration_since(*t) < DEBOUNCE);
        if recent.contains_key(&key) {
            return false;
        }
        recent.insert(key, now);
        true
    }

    /// Sends `n` to every channel and push subscription concurrently. Returns one result
    /// per target (empty when debounced or when there are no targets).
    pub async fn notify(&self, n: &Notification) -> Vec<(String, Result<()>)> {
        if !self.debounce(n) {
            tracing::info!(kind = ?n.kind, session = ?n.session, "notification debounced");
            return Vec::new();
        }
        let cfg = self.config();
        let subs = if cfg.web_push { self.subscriptions() } else { Vec::new() };

        let mut jobs: Vec<Job<'_>> = Vec::new();
        for ch in cfg.channels {
            let include = cfg.include_content;
            jobs.push(Box::pin(async move {
                let r = tokio::time::timeout(SEND_TIMEOUT, self.send_channel(&ch, n, include))
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("timed out")));
                (ch.label(), r)
            }));
        }
        for sub in subs {
            jobs.push(Box::pin(async move {
                let label = format!("push:{}", origin_of(&sub.endpoint).unwrap_or_default());
                let r = tokio::time::timeout(SEND_TIMEOUT, self.send_push(&sub, n))
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("timed out")));
                (label, r)
            }));
        }
        let results = futures_util::future::join_all(jobs).await;
        for (label, r) in &results {
            match r {
                Ok(()) => tracing::info!(kind = ?n.kind, session = ?n.session, "notification sent via {label}"),
                Err(e) => tracing::warn!("notification via {label} failed: {e:#}"),
            }
        }
        results
    }

    async fn send_channel(&self, ch: &ChannelConfig, n: &Notification, include: bool) -> Result<()> {
        let body = if include || n.kind == NotifyKind::Test { n.body.clone() } else { n.generic_body() };
        let title = format!("{} · {}", n.host, n.title);
        let req = match ch {
            ChannelConfig::Ntfy { server, topic, token } => {
                let mut payload = serde_json::json!({
                    "topic": topic,
                    "title": title,
                    "message": body,
                    "tags": [ntfy_tag(n.kind)],
                    "priority": if n.kind == NotifyKind::Approval { 4 } else { 3 },
                });
                if let Some(url) = &n.url {
                    payload["click"] = serde_json::Value::String(url.clone());
                }
                let mut r = self.client().post(server.trim_end_matches('/')).json(&payload);
                if let Some(t) = token.as_deref().filter(|t| !t.is_empty()) {
                    r = r.bearer_auth(t);
                }
                r
            }
            ChannelConfig::Bark { server, key } => {
                let mut payload = serde_json::json!({
                    "device_key": key,
                    "title": title,
                    "body": body,
                    "group": "yonder",
                });
                if let Some(url) = &n.url {
                    payload["url"] = serde_json::Value::String(url.clone());
                }
                if n.kind == NotifyKind::Approval {
                    payload["level"] = serde_json::Value::String("timeSensitive".into());
                }
                self.client().post(format!("{}/push", server.trim_end_matches('/'))).json(&payload)
            }
            ChannelConfig::Webhook { url, headers } => {
                let mut payload = n.clone();
                payload.body = body;
                let mut r = self.client().post(url).json(&payload);
                for (k, v) in headers {
                    r = r.header(k, v);
                }
                r
            }
        };
        let resp = req.send().await.context("request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("HTTP {status}: {}", truncate(&text, 200)));
        }
        Ok(())
    }

    async fn send_push(&self, sub: &PushSubscription, n: &Notification) -> Result<()> {
        let payload = serde_json::json!({
            "title": format!("{} · {}", n.host, n.title),
            "body": n.body,
            "url": n.url,
            "tag": n.tag,
            "kind": n.kind,
            "session": n.session,
        });
        let plaintext = serde_json::to_vec(&payload)?;
        let ua_pub = webpush::b64d(&sub.p256dh).context("p256dh")?;
        let auth = webpush::b64d(&sub.auth).context("auth")?;
        let body = webpush::encrypt(&plaintext, &ua_pub, &auth)?;
        let aud = origin_of(&sub.endpoint).ok_or_else(|| anyhow!("bad endpoint"))?;
        let exp = now_ms() / 1000 + 12 * 3600;
        let contact = self.vapid_subject();
        let authorization = match &sub.vapid_private {
            Some(k) => {
                let key = VapidKey::from_private(&webpush::b64d(k).context("vapid_private")?)?;
                vapid_authorization(&key, &aud, exp, &contact)?
            }
            None => vapid_authorization(&self.vapid, &aud, exp, &contact)?,
        };
        let resp = self
            .client()
            .post(&sub.endpoint)
            .header("TTL", "86400")
            .header("Urgency", if n.kind == NotifyKind::Approval { "high" } else { "normal" })
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("Authorization", authorization)
            .body(body)
            .send()
            .await
            .context("request")?;
        let status = resp.status();
        if status.as_u16() == 404 || status.as_u16() == 410 {
            self.remove_endpoint(&sub.endpoint);
            return Err(anyhow!("subscription expired (HTTP {status}), removed"));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("HTTP {status}: {}", truncate(&text, 200)));
        }
        Ok(())
    }
}

fn ntfy_tag(kind: NotifyKind) -> &'static str {
    match kind {
        NotifyKind::Approval => "warning",
        NotifyKind::TurnDone => "white_check_mark",
        NotifyKind::Exited => "stop_sign",
        NotifyKind::Test => "bell",
    }
}

fn build_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .timeout(SEND_TIMEOUT)
        .user_agent(concat!("yonder/", env!("CARGO_PKG_VERSION")));
    if let Some(p) = proxy.filter(|p| !p.trim().is_empty()) {
        b = b.proxy(reqwest::Proxy::all(p.trim()).with_context(|| format!("invalid proxy {p}"))?);
    }
    b.build().context("http client")
}

fn origin_of(url: &str) -> Option<String> {
    let u = reqwest::Url::parse(url).ok()?;
    let o = u.origin();
    o.is_tuple().then(|| o.ascii_serialization())
}

/// An https URL whose host a push service can treat as a real contact: a global IP address
/// or a dotted domain outside the reserved / local-only names.
fn is_public_https(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    if u.scheme() != "https" {
        return false;
    }
    match u.host() {
        Some(url::Host::Ipv4(ip)) => {
            !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast() || ip.is_documentation())
        }
        Some(url::Host::Ipv6(ip)) => !(ip.is_loopback() || ip.is_unspecified() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80),
        Some(url::Host::Domain(d)) => {
            const RESERVED: [&str; 12] =
                ["local", "localhost", "test", "invalid", "example", "internal", "lan", "home", "corp", "intranet", "private", "arpa"];
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            match d.rsplit_once('.') {
                Some((_, tld)) => !RESERVED.contains(&tld),
                None => false,
            }
        }
        None => false,
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Writes a file readable only by the current user (0600 on Unix) via a temp + rename.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| anyhow!("no parent dir"))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests;
