//! Host configuration, identity and authorized devices.
//!
//! Config dir (`dirs::config_dir()/yonder`, override with `YONDER_CONFIG_DIR`):
//! `config.toml`, `host.key` (0600), `devices.json`, `audit.log`.
//! Data dir (`dirs::data_local_dir()/yonder`, override with `YONDER_DATA_DIR`):
//! `sessions/<id>/…`, `uploads/`, `trash/`, `notify/` (VAPID key, push subscriptions),
//! `daemon.log`, and the Unix control socket `yonder.sock`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use yonder_notify::NotifyConfig;
use yonder_proto::app::{DeviceInfo, PERM_FILES, PERM_SESSIONS};
use yonder_proto::keys::{Keypair, PublicKey};

use crate::util::{ensure_private_dir, write_private};

pub const DEFAULT_RELAY: &str = "wss://relay.example.com/v1/ws";

/// Where everything lives. Tests point both dirs into a temp dir.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let config_dir = match std::env::var_os("YONDER_CONFIG_DIR") {
            Some(d) => PathBuf::from(d),
            None => dirs::config_dir().ok_or_else(|| anyhow!("no config dir"))?.join("yonder"),
        };
        let data_dir = match std::env::var_os("YONDER_DATA_DIR") {
            Some(d) => PathBuf::from(d),
            None => dirs::data_local_dir().ok_or_else(|| anyhow!("no data dir"))?.join("yonder"),
        };
        Ok(Self { config_dir, data_dir })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn key_file(&self) -> PathBuf {
        self.config_dir.join("host.key")
    }
    pub fn devices_file(&self) -> PathBuf {
        self.config_dir.join("devices.json")
    }
    /// Pairing codes handed out and not used yet (so they survive a daemon restart).
    pub fn pairing_file(&self) -> PathBuf {
        self.config_dir.join("pairing.json")
    }
    pub fn audit_log(&self) -> PathBuf {
        self.config_dir.join("audit.log")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
    pub fn uploads_dir(&self) -> PathBuf {
        self.data_dir.join("uploads")
    }
    pub fn trash_dir(&self) -> PathBuf {
        self.data_dir.join("trash")
    }
    pub fn notify_dir(&self) -> PathBuf {
        self.data_dir.join("notify")
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.data_dir.join("daemon.log")
    }
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    /// Local control socket: a Unix socket path, or a per-user named pipe name on Windows.
    pub fn control_socket(&self) -> String {
        #[cfg(unix)]
        {
            self.data_dir.join("yonder.sock").to_string_lossy().into_owned()
        }
        #[cfg(windows)]
        {
            let who = std::env::var("USERNAME").unwrap_or_default();
            let key = format!("{}|{}", who, self.data_dir.display());
            format!("yonder-ctl-{}", crate::util::short_hash(&key))
        }
    }

    pub fn ensure(&self) -> Result<()> {
        ensure_private_dir(&self.config_dir)?;
        ensure_private_dir(&self.data_dir)?;
        ensure_private_dir(&self.sessions_dir())?;
        ensure_private_dir(&self.uploads_dir())?;
        ensure_private_dir(&self.notify_dir())?;
        Ok(())
    }
}

/// Per-agent overrides in `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOverride {
    /// Program (and leading args) to run instead of the default executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<Vec<String>>,
    /// Extra environment variables.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<AgentOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<AgentOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi: Option<AgentOverride>,
}

fn default_true() -> bool {
    true
}

/// `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Display name shown to devices.
    pub name: String,
    /// Relay WebSocket URL.
    pub relay_url: String,
    /// Web client URL used in pairing links. Default: derived from `relay_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    /// Folders the file manager may access (default: home). `~` is expanded.
    #[serde(default)]
    pub fs_roots: Vec<String>,
    /// Paths the file manager may never access.
    #[serde(default)]
    pub fs_deny: Vec<String>,
    /// Run agents through the login shell on Unix so the PATH matches a terminal.
    #[serde(default = "default_true")]
    pub login_shell: bool,
    /// HTTP(S) proxy for the relay connection (`http://host:port`). Default: none (direct).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_proxy: Option<String>,
    #[serde(default)]
    pub notify: NotifyConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
}

impl Config {
    pub fn default_for_host() -> Self {
        let host = gethostname::gethostname().to_string_lossy().into_owned();
        let name = host.split('.').next().unwrap_or(&host).to_string();
        Self {
            name: if name.is_empty() { "host".into() } else { name },
            relay_url: DEFAULT_RELAY.into(),
            web_url: None,
            fs_roots: Vec::new(),
            fs_deny: Vec::new(),
            login_shell: true,
            relay_proxy: None,
            notify: NotifyConfig::default(),
            agents: AgentsConfig::default(),
        }
    }

    pub fn load(paths: &Paths) -> Result<Option<Self>> {
        let path = paths.config_file();
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let cfg: Config = toml::from_str(&s).with_context(|| format!("parse {}", path.display()))?;
                Ok(Some(cfg))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let s = toml::to_string_pretty(self).context("serialize config")?;
        write_private(&paths.config_file(), s.as_bytes())
    }

    /// Web client URL: explicit `web_url`, else the relay origin (the relay serves the app).
    pub fn web_url(&self) -> String {
        if let Some(w) = self.web_url.as_deref().filter(|w| !w.is_empty()) {
            return w.trim_end_matches('/').to_string();
        }
        web_url_from_relay(&self.relay_url)
    }

    pub fn agent_override(&self, agent: yonder_proto::app::AgentKind) -> Option<&AgentOverride> {
        use yonder_proto::app::AgentKind;
        match agent {
            AgentKind::Codex => self.agents.codex.as_ref(),
            AgentKind::Claude => self.agents.claude.as_ref(),
            AgentKind::Pi => self.agents.pi.as_ref(),
            _ => None,
        }
    }
}

/// `wss://h:p/v1/ws` -> `https://h:p`.
pub fn web_url_from_relay(relay: &str) -> String {
    let (scheme, rest) = match relay.split_once("://") {
        Some(("wss", r)) => ("https", r),
        Some(("ws", r)) => ("http", r),
        Some((s, r)) => (s, r),
        None => ("https", relay),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}://{authority}")
}

/// Load the host keypair, creating it (0600) on first use.
pub fn load_or_create_key(paths: &Paths) -> Result<Keypair> {
    let path = paths.key_file();
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let kp = Keypair::generate().map_err(|e| anyhow!("generate key: {e}"))?;
            write_private(&path, serde_json::to_string_pretty(&kp)?.as_bytes())?;
            Ok(kp)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// One authorized device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub public: PublicKey,
    pub name: String,
    pub client: String,
    pub paired_at: u64,
    #[serde(default)]
    pub last_seen: Option<u64>,
    pub permissions: Vec<String>,
}

impl Device {
    pub fn info(&self, current: bool) -> DeviceInfo {
        DeviceInfo {
            public: self.public.to_b64(),
            name: self.name.clone(),
            client: self.client.clone(),
            paired_at: self.paired_at,
            last_seen: self.last_seen,
            permissions: self.permissions.clone(),
            current,
        }
    }

    pub fn can(&self, perm: &str) -> bool {
        self.permissions.iter().any(|p| p == perm)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    devices: Vec<Device>,
}

/// `devices.json`, cached in memory.
pub struct Devices {
    path: PathBuf,
    list: Vec<Device>,
}

impl Devices {
    pub fn load(path: &Path) -> Result<Self> {
        let list = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice::<DevicesFile>(&b)
                .with_context(|| format!("parse {}", path.display()))?
                .devices,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Ok(Self { path: path.to_path_buf(), list })
    }

    fn save(&self) -> Result<()> {
        let f = DevicesFile { devices: self.list.clone() };
        write_private(&self.path, &serde_json::to_vec_pretty(&f)?)
    }

    pub fn get(&self, key: &PublicKey) -> Option<&Device> {
        self.list.iter().find(|d| &d.public == key)
    }

    pub fn list(&self) -> &[Device] {
        &self.list
    }

    pub fn add(&mut self, d: Device) -> Result<()> {
        self.list.retain(|x| x.public != d.public);
        self.list.push(d);
        self.save()
    }

    pub fn remove(&mut self, key: &PublicKey) -> Result<bool> {
        let before = self.list.len();
        self.list.retain(|d| &d.public != key);
        if self.list.len() != before {
            self.save()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Update `last_seen` (saved at most once a minute per device).
    pub fn touch(&mut self, key: &PublicKey, now: u64) {
        if let Some(d) = self.list.iter_mut().find(|d| &d.public == key) {
            let stale = d.last_seen.map(|t| now.saturating_sub(t) > 60_000).unwrap_or(true);
            d.last_seen = Some(now);
            if stale {
                let _ = self.save();
            }
        }
    }
}

pub fn default_permissions() -> Vec<String> {
    vec![PERM_SESSIONS.to_string(), PERM_FILES.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config_dir: dir.path().join("c"), data_dir: dir.path().join("d") };
        paths.ensure().unwrap();
        assert!(Config::load(&paths).unwrap().is_none());
        let mut cfg = Config::default_for_host();
        cfg.fs_roots = vec!["~".into()];
        cfg.save(&paths).unwrap();
        let back = Config::load(&paths).unwrap().unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.web_url(), "https://relay.example.com");
        assert_eq!(web_url_from_relay("ws://127.0.0.1:9/v1/ws"), "http://127.0.0.1:9");

        let minimal: Config = toml::from_str("name = \"m\"\nrelay_url = \"wss://r/v1/ws\"\n").unwrap();
        assert!(minimal.login_shell);
        assert!(minimal.notify.web_push);

        let k1 = load_or_create_key(&paths).unwrap();
        let k2 = load_or_create_key(&paths).unwrap();
        assert_eq!(k1.public, k2.public);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.key_file()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn devices_add_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devices.json");
        let mut devs = Devices::load(&path).unwrap();
        let k = Keypair::generate().unwrap().public;
        devs.add(Device {
            public: k,
            name: "iPhone".into(),
            client: "web".into(),
            paired_at: 1,
            last_seen: None,
            permissions: default_permissions(),
        })
        .unwrap();
        let again = Devices::load(&path).unwrap();
        assert!(again.get(&k).unwrap().can(PERM_FILES));
        let mut again = again;
        assert!(again.remove(&k).unwrap());
        assert!(Devices::load(&path).unwrap().get(&k).is_none());
    }
}
