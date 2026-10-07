//! Peer <-> relay protocol, carried over one WebSocket per peer.
//!
//! - Text frames: JSON control messages ([`ClientMsg`] up, [`ServerMsg`] down).
//! - Binary frames: link data, `[u32 link id, big-endian][opaque payload]`.
//!   The payload is always Noise ciphertext; the relay never sees plaintext.
//!
//! Flow:
//! 1. Relay sends `challenge` with its static public key and a random nonce.
//! 2. Peer answers `auth` with role, public key and proof
//!    `BLAKE2s-MAC(key = X25519(peer, relay), AUTH_CONTEXT || nonce || role || peer_pub)`.
//! 3. Relay replies `welcome` (or `error` + close).
//! 4. A device sends `open {req, to: host_pub}`; the relay assigns a link id, tells the
//!    host `incoming {link, from: device_pub}` and the device `opened {req, link}`.
//!    Either side may `close {link}`; the relay notifies the other side with `closed`.
//! 5. Devices may `watch` host keys to receive `presence` updates.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use blake2::digest::{KeyInit, Mac};
use serde::{Deserialize, Serialize};

use crate::keys::{Keypair, PublicKey};
use crate::{Error, Result};

/// WebSocket path served by the relay.
pub const WS_PATH: &str = "/v1/ws";
/// Health endpoint served by the relay (plain text "ok").
pub const HEALTH_PATH: &str = "/v1/health";
pub const AUTH_CONTEXT: &[u8] = b"yonder-relay-auth-v1";
/// Upper bound for one binary frame (header + Noise message).
pub const MAX_FRAME_LEN: usize = 4 + 65_535;
pub const NONCE_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Device,
}

impl Role {
    fn byte(self) -> u8 {
        match self {
            Role::Host => b'h',
            Role::Device => b'd',
        }
    }
}

/// Messages from a peer (host or device) to the relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, rename = "RelayClientMsg"))]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Auth {
        role: Role,
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        public: PublicKey,
        /// Base64 (standard) MAC, see module docs.
        proof: String,
        protocol: u32,
    },
    /// Device only: open a link to a host.
    Open {
        req: u32,
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        to: PublicKey,
    },
    Close { link: u32 },
    /// Device only: subscribe to presence of these hosts (replaces previous list).
    Watch {
        #[cfg_attr(feature = "ts", ts(type = "Array<string>"))]
        hosts: Vec<PublicKey>,
    },
    Ping { ts: u64 },
}

/// Messages from the relay to a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, rename = "RelayServerMsg"))]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Challenge {
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        relay_pub: PublicKey,
        /// Base64 (standard) random nonce.
        nonce: String,
        protocol: u32,
    },
    Welcome {
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        you: PublicKey,
        role: Role,
    },
    /// To host: a device opened a link.
    Incoming {
        link: u32,
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        from: PublicKey,
    },
    /// To device: result of `open`.
    Opened {
        req: u32,
        link: u32,
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        to: PublicKey,
    },
    OpenFailed {
        req: u32,
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        to: PublicKey,
        /// `host_offline`, `rate_limited`, `invalid`, ...
        reason: String,
    },
    /// The other end closed the link or went away.
    Closed { link: u32, reason: String },
    Presence {
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        host: PublicKey,
        online: bool,
    },
    Error { code: String, message: String },
    Pong { ts: u64 },
}

/// Compute the relay auth proof for `role` as the owner of `local`.
pub fn auth_proof(local: &Keypair, relay_pub: &PublicKey, nonce: &[u8], role: Role) -> Result<String> {
    let key = local.dh(relay_pub)?;
    let mac = auth_mac(&key, nonce, role, &local.public)?;
    Ok(B64.encode(mac.finalize().into_bytes()))
}

/// Relay side: verify a proof produced by [`auth_proof`].
pub fn verify_auth_proof(
    relay: &Keypair,
    peer: &PublicKey,
    nonce: &[u8],
    role: Role,
    proof_b64: &str,
) -> bool {
    let Ok(proof) = B64.decode(proof_b64) else { return false };
    let Ok(key) = relay.dh(peer) else { return false };
    let Ok(mac) = auth_mac(&key, nonce, role, peer) else { return false };
    mac.verify_slice(&proof).is_ok()
}

fn auth_mac(key: &[u8; 32], nonce: &[u8], role: Role, peer: &PublicKey) -> Result<blake2::Blake2sMac256> {
    let mut mac = <blake2::Blake2sMac256 as KeyInit>::new_from_slice(key)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    mac.update(AUTH_CONTEXT);
    mac.update(nonce);
    mac.update(&[role.byte()]);
    mac.update(&peer.0);
    Ok(mac)
}

pub fn encode_frame(link: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + payload.len());
    v.extend_from_slice(&link.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn decode_frame(frame: &[u8]) -> Option<(u32, &[u8])> {
    if frame.len() < 4 || frame.len() > MAX_FRAME_LEN {
        return None;
    }
    let link = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
    Some((link, &frame[4..]))
}

pub fn encode_nonce(nonce: &[u8]) -> String {
    B64.encode(nonce)
}

pub fn decode_nonce(s: &str) -> Result<Vec<u8>> {
    Ok(B64.decode(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_roundtrip() {
        let relay = Keypair::generate().unwrap();
        let peer = Keypair::generate().unwrap();
        let nonce = crate::keys::random_bytes::<NONCE_LEN>().unwrap();
        let proof = auth_proof(&peer, &relay.public, &nonce, Role::Host).unwrap();
        assert!(verify_auth_proof(&relay, &peer.public, &nonce, Role::Host, &proof));
        assert!(!verify_auth_proof(&relay, &peer.public, &nonce, Role::Device, &proof));
        let other = Keypair::generate().unwrap();
        assert!(!verify_auth_proof(&relay, &other.public, &nonce, Role::Host, &proof));
    }

    #[test]
    fn frames_and_json() {
        let f = encode_frame(7, b"abc");
        assert_eq!(decode_frame(&f), Some((7, &b"abc"[..])));
        assert_eq!(decode_frame(&[0, 1]), None);
        let k = Keypair::generate().unwrap();
        let m = ClientMsg::Open { req: 1, to: k.public };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.starts_with("{\"t\":\"open\""));
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ClientMsg::Open { req: 1, .. }));
    }
}
