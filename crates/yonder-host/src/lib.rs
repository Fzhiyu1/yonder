//! yonder host: the daemon that connects this machine to the relay and runs sessions.
//!
//! - [`daemon`]: shared state and request handling.
//! - [`relay_link`]: relay WebSocket + Noise responder per device link.
//! - [`control`]: local control socket (trusted, used by the `yonder` CLI).
//! - [`sessions`]: session manager on top of detached supervisors.
//! - [`chatsup`]: chat supervisor process (agent adapter + event log).
//! - [`config`]: paths, `config.toml`, host key, authorized devices.

pub mod chatlog;
pub mod chatsup;
pub mod config;
pub mod control;
pub mod daemon;
pub mod device;
mod httpget;
pub mod relay_link;
pub mod sessions;
pub mod util;

use std::sync::Arc;

use anyhow::{Context, Result};

pub use config::{Config, Paths};
pub use daemon::Daemon;

/// Run the daemon until shutdown (Ctrl-C, SIGTERM or a `shutdown` request).
pub async fn run_daemon(paths: Paths, launcher: sessions::Launcher) -> Result<()> {
    let cfg = match Config::load(&paths)? {
        Some(c) => c,
        None => {
            let c = Config::default_for_host();
            paths.ensure()?;
            c.save(&paths)?;
            tracing::info!("created {}", paths.config_file().display());
            c
        }
    };
    let (d, signals) = Daemon::new(paths.clone(), cfg.clone(), launcher)?;
    tracing::info!(
        name = %cfg.name,
        host = %d.key.public,
        fingerprint = %d.key.public.fingerprint(),
        relay = %cfg.relay_url,
        "yonder daemon {} starting",
        env!("CARGO_PKG_VERSION")
    );
    d.sessions.load_existing().await;
    d.spawn_notifier(signals);
    d.prefetch_agents();

    let control = {
        let d = d.clone();
        tokio::spawn(async move {
            if let Err(e) = control::serve(d.clone()).await {
                tracing::error!("control socket: {e:#}");
                d.shutdown.notify_waiters();
            }
        })
    };
    let relay = tokio::spawn(relay_link::run(d.clone()));

    wait_for_shutdown(&d).await;
    tracing::info!("shutting down (sessions keep running)");
    d.shutdown.notify_waiters();
    d.sessions.detach_all();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let _ = relay.await;
        let _ = control.await;
    })
    .await;
    Ok(())
}

async fn wait_for_shutdown(d: &Arc<Daemon>) {
    let requested = d.shutdown.notified();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
        tokio::select! {
            _ = requested => {}
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = requested => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
}

/// Entry point for `yonder __supervise-chat <args>`.
pub fn chat_supervisor_entry(arg: &str) -> Result<()> {
    let args = chatsup::ChatSupArgs::decode(arg).context("decode chat supervisor args")?;
    chatsup::chat_supervisor_main(args)
}
