//! `yonder`: host daemon, supervisors and the local CLI in one binary.

mod attach;
mod service;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use yonder_host::control::ControlClient;
use yonder_host::sessions::Launcher;
use yonder_host::{Config, Paths};
use yonder_proto::app::{AgentKind, Request, Response, SessionKind, SessionOrigin, SessionSpec, SessionState};

#[derive(Parser)]
#[command(name = "yonder", version, about = "Drive CLI agents and terminals on this machine from your phone")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create or update the host config.
    Init {
        /// Display name shown on your devices.
        #[arg(long)]
        name: Option<String>,
        /// Relay WebSocket URL, e.g. wss://relay.example.com/v1/ws
        #[arg(long)]
        relay: Option<String>,
        /// Web client URL for pairing links (default: the relay origin).
        #[arg(long)]
        web: Option<String>,
        /// Folder the file manager may access (repeatable; default: home).
        #[arg(long = "root")]
        roots: Vec<String>,
        /// HTTP proxy for the relay connection (http://host:port); "none" to clear.
        #[arg(long)]
        relay_proxy: Option<String>,
    },
    /// Run the daemon in the foreground.
    Daemon,
    /// Show a pairing QR code and link for a new device.
    Pair {
        /// Minutes the pairing code stays valid.
        #[arg(long, default_value_t = 10)]
        minutes: u64,
        /// Only allow sessions (no file access).
        #[arg(long)]
        no_files: bool,
        /// Print only the link.
        #[arg(long)]
        link_only: bool,
    },
    /// Daemon status.
    Status,
    /// List sessions.
    Ls,
    /// Run a command in a new session and attach to it (Ctrl-] detaches).
    Run {
        /// Working directory (default: current).
        #[arg(long, short = 'C')]
        cwd: Option<PathBuf>,
        /// Session title.
        #[arg(long, short)]
        title: Option<String>,
        /// Do not attach; print the session id.
        #[arg(long, short)]
        detach: bool,
        /// Command and arguments (default: your shell).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Attach to a terminal session (Ctrl-] detaches).
    Attach { session: String },
    /// Kill a session.
    Kill { session: String },
    /// Remove ended sessions and their logs.
    Rm {
        /// Session ids.
        sessions: Vec<String>,
        /// Remove every ended session.
        #[arg(long)]
        exited: bool,
    },
    /// Paired devices.
    Devices {
        #[command(subcommand)]
        cmd: Option<DevicesCmd>,
    },
    /// Send a test notification.
    NotifyTest,
    /// Install / remove the per-user background service.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Stop the daemon (sessions keep running).
    Stop,
    /// Print config and data paths.
    Paths,
    #[command(name = "__supervise", hide = true)]
    Supervise { args: String },
    #[command(name = "__supervise-chat", hide = true)]
    SuperviseChat { args: String },
}

#[derive(Subcommand)]
enum DevicesCmd {
    /// List paired devices.
    Ls,
    /// Revoke a device (public key or unique prefix, or its name).
    Revoke { device: String },
}

#[derive(Subcommand)]
enum ServiceCmd {
    Install,
    Uninstall,
}

fn init_tracing(to_file: Option<PathBuf>) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("YONDER_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    match to_file {
        Some(path) if std::env::var_os("YONDER_LOG_STDERR").is_none() => {
            let dir = path.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            let name = path.file_name().map(|n| n.to_owned()).unwrap_or_else(|| "daemon.log".into());
            let appender = tracing_appender::rolling::never(dir, name);
            let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(appender).with_ansi(false).try_init();
        }
        _ => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        // Supervisors run their own runtimes and must not share the CLI's.
        Cmd::Supervise { args } => {
            let a = yonder_pty::SupervisorArgs::decode(&args)?;
            return yonder_pty::supervisor_main(a);
        }
        Cmd::SuperviseChat { args } => return yonder_host::chat_supervisor_entry(&args),
        _ => {}
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(run(cli.cmd))
}

async fn run(cmd: Cmd) -> Result<()> {
    let paths = Paths::from_env()?;
    match cmd {
        Cmd::Init { name, relay, web, roots, relay_proxy } => init(&paths, name, relay, web, roots, relay_proxy),
        Cmd::Daemon => {
            paths.ensure()?;
            init_tracing(Some(paths.daemon_log()));
            let launcher = Launcher::current_exe()?;
            yonder_host::run_daemon(paths, launcher).await
        }
        Cmd::Pair { minutes, no_files, link_only } => pair(&paths, minutes, no_files, link_only).await,
        Cmd::Status => status(&paths).await,
        Cmd::Ls => ls(&paths).await,
        Cmd::Run { cwd, title, detach, command } => run_cmd(&paths, cwd, title, detach, command).await,
        Cmd::Attach { session } => {
            let code = attach::attach(&paths.control_socket(), &session).await?;
            if let Some(c) = code {
                std::process::exit(c);
            }
            Ok(())
        }
        Cmd::Kill { session } => {
            let mut c = connect(&paths).await?;
            c.request(Request::Kill { session }).await?;
            Ok(())
        }
        Cmd::Rm { sessions, exited } => rm(&paths, sessions, exited).await,
        Cmd::Devices { cmd } => devices(&paths, cmd.unwrap_or(DevicesCmd::Ls)).await,
        Cmd::NotifyTest => {
            let mut c = connect(&paths).await?;
            c.request(Request::NotifyTest).await?;
            println!("sent");
            Ok(())
        }
        Cmd::Service { cmd } => match cmd {
            ServiceCmd::Install => {
                if Config::load(&paths)?.is_none() {
                    init(&paths, None, None, None, Vec::new(), None)?;
                }
                // A foreground daemon would fight the service for the control socket.
                if let Ok(mut c) = ControlClient::connect(&paths.control_socket()).await {
                    let _ = c.request(Request::Shutdown).await;
                    tokio::time::sleep(Duration::from_millis(800)).await;
                }
                service::install(&paths)
            }
            ServiceCmd::Uninstall => service::uninstall(&paths),
        },
        Cmd::Stop => {
            let mut c = connect(&paths).await?;
            c.request(Request::Shutdown).await?;
            println!("daemon stopping; sessions keep running");
            Ok(())
        }
        Cmd::Paths => {
            println!("config:  {}", paths.config_file().display());
            println!("key:     {}", paths.key_file().display());
            println!("devices: {}", paths.devices_file().display());
            println!("data:    {}", paths.data_dir.display());
            println!("log:     {}", paths.daemon_log().display());
            println!("socket:  {}", paths.control_socket());
            Ok(())
        }
        Cmd::Supervise { .. } | Cmd::SuperviseChat { .. } => unreachable!(),
    }
}

fn init(paths: &Paths, name: Option<String>, relay: Option<String>, web: Option<String>, roots: Vec<String>, relay_proxy: Option<String>) -> Result<()> {
    paths.ensure()?;
    let mut cfg = Config::load(paths)?.unwrap_or_else(Config::default_for_host);
    if let Some(n) = name {
        cfg.name = n;
    }
    if let Some(r) = relay {
        if !(r.starts_with("wss://") || r.starts_with("ws://")) {
            bail!("relay must be a ws:// or wss:// URL");
        }
        cfg.relay_url = r;
    }
    if let Some(w) = web {
        cfg.web_url = Some(w).filter(|w| !w.is_empty());
    }
    if !roots.is_empty() {
        cfg.fs_roots = roots;
    }
    if let Some(p) = relay_proxy {
        cfg.relay_proxy = if p == "none" || p.is_empty() { None } else { Some(p) };
    }
    cfg.save(paths)?;
    let key = yonder_host::config::load_or_create_key(paths)?;
    println!("name:        {}", cfg.name);
    println!("relay:       {}", cfg.relay_url);
    println!("web:         {}", cfg.web_url());
    println!("host key:    {}", key.public);
    println!("fingerprint: {}", key.public.fingerprint());
    println!("config:      {}", paths.config_file().display());
    Ok(())
}

async fn connect(paths: &Paths) -> Result<ControlClient> {
    ControlClient::connect(&paths.control_socket())
        .await
        .map_err(|e| anyhow!("{e:#}\nstart it with `yonder daemon` or `yonder service install`"))
}

/// "3 min ago" style age of a unix-ms timestamp.
fn ago(ms: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    ago_at(ms, now)
}

fn ago_at(ms: u64, now_ms: u64) -> String {
    let s = now_ms.saturating_sub(ms) / 1000;
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

fn print_qr(text: &str) -> Result<()> {
    use qrcode::render::unicode::Dense1x2;
    use qrcode::{EcLevel, QrCode};
    use std::io::IsTerminal;
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L).context("QR code")?;
    // Block characters take the terminal's own colors, so the code only reads correctly on a
    // dark background (light blocks = light modules). Pin white blocks on black with ANSI
    // colors so it scans on light themes too; the plain rendering stays for pipes and files.
    let img = code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).quiet_zone(true).build();
    #[cfg(windows)]
    let ansi = std::io::stdout().is_terminal() && crossterm::ansi_support::supports_ansi();
    #[cfg(not(windows))]
    let ansi = std::io::stdout().is_terminal() && std::env::var("TERM").map_or(true, |t| t != "dumb");
    if ansi {
        for line in img.lines() {
            // 97 = bright white foreground (the blocks), 40 = black background.
            println!("\x1b[97;40m{line}\x1b[0m");
        }
    } else {
        println!("{img}");
    }
    Ok(())
}

async fn pair(paths: &Paths, minutes: u64, no_files: bool, link_only: bool) -> Result<()> {
    let mut c = connect(paths).await?;
    let mut perms = vec!["sessions".to_string()];
    if !no_files {
        perms.push("files".into());
    }
    let resp = c.request(Request::CreatePairing { permissions: perms, ttl_secs: Some(minutes * 60) }).await?;
    let Response::Pairing { payload, url } = resp else { bail!("unexpected response") };
    if link_only {
        println!("{url}");
        return Ok(());
    }
    print_qr(&url)?;
    println!("Scan with your phone camera, or open this link on the device to pair:");
    println!("{url}");
    println!();
    println!("host:        {} ({})", payload.host_name, payload.host.fingerprint());
    println!("valid for:   {minutes} min, one device");
    // Wait for the pairing to complete (the daemon broadcasts device_paired).
    println!("waiting for the device... (Ctrl-C to stop waiting; the code stays valid)");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(minutes * 60);
    loop {
        for ev in std::mem::take(&mut c.events) {
            if let yonder_proto::app::Event::DevicePaired { device } = ev {
                println!("paired: {} ({})", device.name, device.client);
                return Ok(());
            }
        }
        let r = tokio::time::timeout_at(deadline, c.recv()).await;
        match r {
            Err(_) => {
                println!("pairing code expired");
                return Ok(());
            }
            Ok(Ok(Some(yonder_proto::app::HostMsg::Event { event }))) => c.events.push(event),
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => bail!("daemon closed the connection"),
            Ok(Err(e)) => return Err(e),
        }
    }
}

async fn status(paths: &Paths) -> Result<()> {
    let mut c = match ControlClient::connect(&paths.control_socket()).await {
        Ok(c) => c,
        Err(_) => {
            println!("daemon: not running");
            if let Some(cfg) = Config::load(paths)? {
                println!("name:   {}", cfg.name);
                println!("relay:  {}", cfg.relay_url);
            }
            std::process::exit(3);
        }
    };
    let Response::Status { status: s } = c.request(Request::Status).await? else { bail!("unexpected response") };
    println!("daemon:      running ({}), up {}s", s.version, s.uptime_ms / 1000);
    println!("name:        {}", s.name);
    println!("host key:    {}", s.host_pub);
    println!("fingerprint: {}", s.fingerprint);
    println!(
        "relay:       {} ({})",
        s.relay_url.unwrap_or_default(),
        if s.relay_connected { "connected".to_string() } else { format!("disconnected: {}", s.relay_error.unwrap_or_default()) }
    );
    println!("web:         {}", s.web_url.unwrap_or_default());
    println!("clients:     {}", s.clients);
    println!("sessions:    {} running", s.sessions);
    Ok(())
}

async fn ls(paths: &Paths) -> Result<()> {
    let mut c = connect(paths).await?;
    let Response::Sessions { sessions } = c.request(Request::ListSessions).await? else { bail!("unexpected response") };
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    println!("{:<11} {:<8} {:<7} {:<8} {:<30} CWD", "ID", "KIND", "AGENT", "STATE", "TITLE");
    for s in sessions {
        let kind = match s.kind {
            SessionKind::Terminal => "term",
            SessionKind::Chat => "chat",
        };
        let state = match s.state {
            SessionState::Starting => "starting",
            SessionState::Running => "running",
            SessionState::Exited => "exited",
            SessionState::Failed => "failed",
        };
        let title: String = s.title.chars().take(30).collect();
        println!("{:<11} {:<8} {:<7} {:<8} {:<30} {}", s.id, kind, s.agent.as_str(), state, title, s.cwd);
    }
    Ok(())
}

async fn rm(paths: &Paths, mut ids: Vec<String>, exited: bool) -> Result<()> {
    let mut c = connect(paths).await?;
    if exited {
        let Response::Sessions { sessions } = c.request(Request::ListSessions).await? else { bail!("unexpected response") };
        ids.extend(sessions.into_iter().filter(|s| matches!(s.state, SessionState::Exited | SessionState::Failed)).map(|s| s.id));
    }
    // Keep the given order, drop repeats (an id given explicitly may also match --exited).
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    if ids.is_empty() {
        if exited {
            println!("no ended sessions");
            return Ok(());
        }
        bail!("nothing to remove: give session ids or --exited");
    }
    let mut failed = 0;
    for id in ids {
        match c.request(Request::Remove { session: id.clone() }).await {
            Ok(_) => println!("removed {id}"),
            Err(e) => {
                failed += 1;
                eprintln!("{id}: {e:#}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} session(s) not removed");
    }
    Ok(())
}

async fn run_cmd(paths: &Paths, cwd: Option<PathBuf>, title: Option<String>, detach: bool, command: Vec<String>) -> Result<()> {
    let mut c = connect(paths).await?;
    let cwd = match cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let agent = match command.first().map(|s| {
        std::path::Path::new(s).file_stem().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
    }) {
        None => AgentKind::Shell,
        Some(p) if p == "codex" => AgentKind::Codex,
        Some(p) if p == "claude" => AgentKind::Claude,
        Some(p) if p == "pi" => AgentKind::Pi,
        Some(_) => AgentKind::Custom,
    };
    let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
    let spec = SessionSpec {
        kind: Some(SessionKind::Terminal),
        agent: Some(agent),
        command: if command.is_empty() { None } else { Some(command) },
        cwd: Some(cwd.to_string_lossy().into_owned()),
        title,
        cols: Some(cols),
        rows: Some(rows),
        origin: Some(SessionOrigin::Local),
        ..Default::default()
    };
    let Response::Session { session } = c.request(Request::CreateSession { spec }).await? else { bail!("unexpected response") };
    if detach {
        println!("{}", session.id);
        return Ok(());
    }
    drop(c);
    let code = attach::attach(&paths.control_socket(), &session.id).await?;
    if let Some(c) = code {
        std::process::exit(c);
    }
    Ok(())
}

async fn devices(paths: &Paths, cmd: DevicesCmd) -> Result<()> {
    let mut c = connect(paths).await?;
    let Response::Devices { devices } = c.request(Request::ListDevices).await? else { bail!("unexpected response") };
    match cmd {
        DevicesCmd::Ls => {
            if devices.is_empty() {
                println!("no paired devices (run `yonder pair`)");
            }
            for d in devices {
                println!(
                    "{}  {:<16} {:<5} perms={} paired {}  last seen {}",
                    &d.public[..12.min(d.public.len())],
                    d.name,
                    d.client,
                    d.permissions.join(","),
                    ago(d.paired_at),
                    d.last_seen.map(ago).unwrap_or_else(|| "never".into())
                );
            }
            Ok(())
        }
        DevicesCmd::Revoke { device } => {
            let matches: Vec<_> = devices.iter().filter(|d| d.public.starts_with(&device) || d.name == device).collect();
            match matches.as_slice() {
                [d] => {
                    c.request(Request::RevokeDevice { device: d.public.clone() }).await?;
                    println!("revoked {} ({})", d.name, d.public);
                    Ok(())
                }
                [] => bail!("no device matches {device}"),
                _ => bail!("{device} matches several devices; use more of the key"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ago_at;

    #[test]
    fn ages_read_like_a_person_would_say_them() {
        let now = 1_790_000_000_000;
        assert_eq!(ago_at(now - 5_000, now), "just now");
        assert_eq!(ago_at(now + 5_000, now), "just now", "clock skew is not negative");
        assert_eq!(ago_at(now - 3 * 60_000, now), "3 min ago");
        assert_eq!(ago_at(now - 5 * 3_600_000, now), "5 h ago");
        assert_eq!(ago_at(now - 2 * 86_400_000, now), "2 d ago");
    }
}
