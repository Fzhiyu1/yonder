use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use yonder_relay::{load_or_create_key, tls, RelayConfig};

/// yonder relay: forwards end-to-end encrypted traffic between devices and hosts.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "YONDER_RELAY_LISTEN", default_value = "0.0.0.0:8443")]
    listen: String,
    /// Relay identity key file (created if missing).
    #[arg(long, env = "YONDER_RELAY_KEY_FILE", default_value = "relay.key")]
    key_file: PathBuf,
    /// Plain HTTP behind a TLS-terminating reverse proxy; trusts X-Forwarded-For.
    #[arg(long, env = "YONDER_RELAY_BEHIND_PROXY")]
    behind_proxy: bool,
    /// PEM certificate chain (enables direct TLS; hot reloaded on change).
    #[arg(long, env = "YONDER_RELAY_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// PEM private key.
    #[arg(long, env = "YONDER_RELAY_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Obtain certificates from Let's Encrypt for this domain (TLS-ALPN-01, port must be 443).
    #[arg(long, env = "YONDER_RELAY_ACME_DOMAIN", conflicts_with_all = ["tls_cert", "behind_proxy"])]
    acme_domain: Vec<String>,
    #[arg(long, env = "YONDER_RELAY_ACME_EMAIL")]
    acme_email: Option<String>,
    #[arg(long, env = "YONDER_RELAY_ACME_CACHE", default_value = "acme-cache")]
    acme_cache: PathBuf,
    #[arg(long, env = "YONDER_RELAY_ACME_STAGING")]
    acme_staging: bool,
    /// Serve the web client from this directory at `/`.
    #[arg(long, env = "YONDER_RELAY_WEB_DIR")]
    web_dir: Option<PathBuf>,
    /// Print the relay public key and exit.
    #[arg(long)]
    print_key: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let args = Args::parse();
    let key = load_or_create_key(&args.key_file)?;
    if args.print_key {
        println!("{}", key.public.to_b64());
        return Ok(());
    }
    tracing::info!("relay public key {} (fingerprint {})", key.public.to_b64(), key.public.fingerprint());

    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!("listening on {}", listener.local_addr()?);
    let incoming = if let (Some(cert), Some(k)) = (args.tls_cert.clone(), args.tls_key.clone()) {
        tracing::info!("TLS from {}", cert.display());
        tls::pem_files(listener, cert, k, Duration::from_secs(60))?
    } else if !args.acme_domain.is_empty() {
        tracing::info!("TLS via ACME for {:?}", args.acme_domain);
        tls::acme(
            listener,
            tls::AcmeOptions {
                domains: args.acme_domain.clone(),
                email: args.acme_email.clone(),
                cache: args.acme_cache.clone(),
                staging: args.acme_staging,
            },
        )?
    } else {
        if !args.behind_proxy {
            tracing::warn!("serving plain HTTP; use --behind-proxy with a TLS proxy, --tls-cert, or --acme-domain");
        }
        tls::plain(listener)?
    };

    let mut cfg = RelayConfig::new(key);
    cfg.behind_proxy = args.behind_proxy;
    cfg.web_dir = args.web_dir.clone();
    tokio::select! {
        r = yonder_relay::serve(incoming, cfg) => r,
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
