//! Connection sources for the relay: plain TCP, TLS from PEM files (hot reloaded), or ACME.
//!
//! TLS handshakes run in spawned tasks so a slow client cannot stall the accept loop;
//! finished streams are handed to axum through a channel.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::rustls::{self, ServerConfig};
use tracing::{info, warn};

pub enum Io {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl AsyncRead for Io {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_flush(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// An axum `Listener` fed by an accept task.
pub struct Incoming {
    rx: mpsc::Receiver<(Io, SocketAddr)>,
    local: SocketAddr,
}

impl axum::serve::Listener for Incoming {
    type Io = Io;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(x) => x,
            // The accept task ended; park forever (server shutdown is handled elsewhere).
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

fn set_nodelay(s: &TcpStream) {
    let _ = s.set_nodelay(true);
}

/// Plain TCP (use behind a TLS-terminating reverse proxy, or for tests).
pub fn plain(listener: TcpListener) -> Result<Incoming> {
    let local = listener.local_addr()?;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((s, addr)) => {
                    set_nodelay(&s);
                    if tx.send((Io::Plain(s), addr)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    Ok(Incoming { rx, local })
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn load_pem_config(cert: &PathBuf, key: &PathBuf) -> Result<Arc<ServerConfig>> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(cert).with_context(|| format!("open {}", cert.display()))?,
    ))
    .collect::<std::result::Result<_, _>>()
    .context("parse certificate chain")?;
    anyhow::ensure!(!certs.is_empty(), "no certificates in {}", cert.display());
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(key).with_context(|| format!("open {}", key.display()))?,
    ))
    .context("parse private key")?
    .context("no private key found")?;
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate/key mismatch")?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

fn mtime(p: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// TLS with certificate files. The files are re-read when their mtime changes
/// (checked every `reload_every`), so renewals by acme.sh or certbot apply without restart.
pub fn pem_files(listener: TcpListener, cert: PathBuf, key: PathBuf, reload_every: Duration) -> Result<Incoming> {
    let initial = load_pem_config(&cert, &key)?;
    let current = Arc::new(RwLock::new(initial));
    {
        let current = current.clone();
        let (cert, key) = (cert.clone(), key.clone());
        tokio::spawn(async move {
            let mut last = (mtime(&cert), mtime(&key));
            loop {
                tokio::time::sleep(reload_every).await;
                let now = (mtime(&cert), mtime(&key));
                if now != last {
                    match load_pem_config(&cert, &key) {
                        Ok(cfg) => {
                            *current.write().unwrap() = cfg;
                            last = now;
                            info!("reloaded TLS certificate {}", cert.display());
                        }
                        Err(e) => warn!("certificate reload failed (keeping old): {e:#}"),
                    }
                }
            }
        });
    }
    let local = listener.local_addr()?;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let (s, addr) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            set_nodelay(&s);
            let cfg = current.read().unwrap().clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
                match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(s)).await {
                    Ok(Ok(tls)) => {
                        let _ = tx.send((Io::Tls(Box::new(tls)), addr)).await;
                    }
                    Ok(Err(e)) => tracing::debug!(%addr, "tls handshake: {e}"),
                    Err(_) => tracing::debug!(%addr, "tls handshake timeout"),
                }
            });
        }
    });
    Ok(Incoming { rx, local })
}

pub struct AcmeOptions {
    pub domains: Vec<String>,
    pub email: Option<String>,
    pub cache: PathBuf,
    pub staging: bool,
}

/// TLS with certificates from Let's Encrypt (TLS-ALPN-01 on this port; must be reachable
/// as `<domain>:443` from the internet).
pub fn acme(listener: TcpListener, opts: AcmeOptions) -> Result<Incoming> {
    use futures_util::StreamExt;
    use rustls_acme::caches::DirCache;
    use rustls_acme::AcmeConfig;

    let mut cfg = AcmeConfig::new(opts.domains.clone())
        .cache(DirCache::new(opts.cache.clone()))
        .directory_lets_encrypt(!opts.staging);
    if let Some(email) = &opts.email {
        cfg = cfg.contact_push(format!("mailto:{email}"));
    }
    let mut state = cfg.state();
    let resolver = state.resolver();
    tokio::spawn(async move {
        loop {
            match state.next().await {
                Some(Ok(ev)) => info!("acme: {ev:?}"),
                Some(Err(e)) => warn!("acme error: {e:?}"),
                None => break,
            }
        }
    });

    let mut default_cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());
    default_cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    let default_cfg = Arc::new(default_cfg);
    let mut challenge_cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    challenge_cfg.alpn_protocols = vec![rustls_acme::acme::ACME_TLS_ALPN_NAME.to_vec()];
    let challenge_cfg = Arc::new(challenge_cfg);

    let local = listener.local_addr()?;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let (s, addr) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            set_nodelay(&s);
            let (tx, default_cfg, challenge_cfg) = (tx.clone(), default_cfg.clone(), challenge_cfg.clone());
            tokio::spawn(async move {
                let fut = async {
                    let start = tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), s).await?;
                    let is_challenge = start
                        .client_hello()
                        .alpn()
                        .into_iter()
                        .flatten()
                        .eq([rustls_acme::acme::ACME_TLS_ALPN_NAME]);
                    if is_challenge {
                        let mut tls = start.into_stream(challenge_cfg).await?;
                        use tokio::io::AsyncWriteExt;
                        let _ = tls.shutdown().await;
                        Ok::<_, std::io::Error>(None)
                    } else {
                        Ok(Some(start.into_stream(default_cfg).await?))
                    }
                };
                match tokio::time::timeout(Duration::from_secs(10), fut).await {
                    Ok(Ok(Some(tls))) => {
                        let _ = tx.send((Io::Tls(Box::new(tls)), addr)).await;
                    }
                    Ok(Ok(None)) => info!(%addr, "answered TLS-ALPN-01 challenge"),
                    Ok(Err(e)) => tracing::debug!(%addr, "tls: {e}"),
                    Err(_) => tracing::debug!(%addr, "tls handshake timeout"),
                }
            });
        }
    });
    Ok(Incoming { rx, local })
}
