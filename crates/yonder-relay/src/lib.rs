//! yonder relay server.
//!
//! Devices and hosts connect over WebSocket, authenticate their static X25519 keys, and
//! exchange opaque Noise ciphertext over relay-assigned links. See `yonder_proto::relay`.

pub mod hub;
pub mod tls;
mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use yonder_proto::keys::Keypair;
use yonder_proto::relay::{HEALTH_PATH, WS_PATH};

pub use hub::{Hub, Limits};

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub key: Keypair,
    pub behind_proxy: bool,
    pub web_dir: Option<PathBuf>,
    pub auth_timeout: Duration,
    pub ping_interval: Duration,
    pub idle_timeout: Duration,
    /// Per-connection outbound queue capacity (messages).
    pub queue: usize,
}

impl RelayConfig {
    pub fn new(key: Keypair) -> Self {
        Self {
            key,
            behind_proxy: false,
            web_dir: None,
            auth_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(90),
            queue: 512,
        }
    }
}

/// Remote address of a connection (implements axum `Connected` for our listener).
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tls::Incoming>> for PeerAddr {
    fn connect_info(stream: axum::serve::IncomingStream<'_, tls::Incoming>) -> Self {
        PeerAddr(*stream.remote_addr())
    }
}

pub struct AppState {
    pub cfg: RelayConfig,
    pub hub: Hub,
}

pub fn router(state: Arc<AppState>) -> Router {
    let mut app = Router::new()
        .route(WS_PATH, get(ws_handler))
        .route(HEALTH_PATH, get(|| async { "ok" }))
        .route("/v1/relay", get(relay_info));
    if let Some(dir) = state.cfg.web_dir.clone() {
        use tower_http::services::{ServeDir, ServeFile};
        use tower_http::set_header::SetResponseHeaderLayer;
        let index = dir.join("index.html");
        let serve = ServeDir::new(&dir).append_index_html_on_directories(true).fallback(ServeFile::new(index));
        app = app.fallback_service(serve).layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ));
    }
    app.with_state(state)
}

async fn relay_info(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let (peers, hosts, links) = st.hub.stats();
    axum::Json(serde_json::json!({
        "relay_pub": st.cfg.key.public.to_b64(),
        "protocol": yonder_proto::PROTOCOL_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "peers": peers, "hosts": hosts, "links": links,
    }))
}

async fn ws_handler(
    State(st): State<Arc<AppState>>,
    ConnectInfo(PeerAddr(addr)): ConnectInfo<PeerAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    let ip = if st.cfg.behind_proxy {
        headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(addr.ip())
    } else {
        addr.ip()
    };
    if !st.hub.acquire_ip(ip) {
        return (axum::http::StatusCode::TOO_MANY_REQUESTS, "too many connections").into_response();
    }
    ws.max_message_size(yonder_proto::relay::MAX_FRAME_LEN + 1024)
        .max_frame_size(yonder_proto::relay::MAX_FRAME_LEN + 1024)
        .on_upgrade(move |socket| async move {
            ws::handle(st.clone(), socket, ip).await;
            st.hub.release_ip(ip);
        })
}

/// Serve the relay on an accepted-connection source (plain TCP or TLS).
pub async fn serve(listener: tls::Incoming, cfg: RelayConfig) -> Result<()> {
    let state = Arc::new(AppState { hub: Hub::new(Limits::default()), cfg });
    serve_with_state(listener, state).await
}

pub async fn serve_with_state(listener: tls::Incoming, state: Arc<AppState>) -> Result<()> {
    let app = router(state);
    axum::serve(listener, app.into_make_service_with_connect_info::<PeerAddr>())
        .await
        .context("serve")
}

/// Load the relay key from `path`, creating it (mode 0600) if missing.
pub fn load_or_create_key(path: &std::path::Path) -> Result<Keypair> {
    if path.exists() {
        let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        return serde_json::from_str(&s).context("parse relay key");
    }
    let kp = Keypair::generate()?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let data = serde_json::to_string_pretty(&kp)?;
    write_private(path, data.as_bytes())?;
    Ok(kp)
}

fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        f.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)?;
        Ok(())
    }
}
