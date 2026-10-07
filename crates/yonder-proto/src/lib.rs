//! yonder wire protocol.
//!
//! Three layers, all shared by relay, host and web client (via wasm):
//! - [`relay`]: JSON control messages + binary data frames between a peer and the relay.
//! - [`noise`]: end-to-end Noise_IK channel between a client device and a host.
//! - [`app`]: application messages carried inside the encrypted channel.
//!
//! [`keys`] and [`pairing`] cover device identity and QR pairing payloads.

pub mod app;
pub mod keys;
pub mod noise;
pub mod pairing;
pub mod relay;

/// Protocol version exchanged in relay hello and app hello.
pub const PROTOCOL_VERSION: u32 = 1;
/// Optional application capabilities exchanged inside the Noise handshake.
pub const FEATURE_SUBAGENTS: &str = "subagents";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("base64: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("rng: {0}")]
    Rng(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;
