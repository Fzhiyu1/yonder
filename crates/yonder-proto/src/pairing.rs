//! Pairing payload shown as a QR code by `yonder pair`.
//!
//! The QR encodes a URL `<web_url>#pair=<base64url(json PairPayload)>` so a phone camera
//! opens the web client directly. The fragment never reaches any server.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use serde::{Deserialize, Serialize};

use crate::keys::PublicKey;
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PairPayload {
    /// Payload format version.
    pub v: u32,
    /// Relay WebSocket URL, e.g. `wss://relay.example.com:2097/v1/ws`.
    pub relay: String,
    /// Host static public key (base64url).
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub host: PublicKey,
    pub host_name: String,
    /// One-time token, valid for a few minutes.
    pub token: String,
    /// Expiry, unix milliseconds.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub exp: u64,
}

impl PairPayload {
    pub fn to_fragment(&self) -> Result<String> {
        Ok(B64.encode(serde_json::to_vec(self)?))
    }

    pub fn from_fragment(s: &str) -> Result<Self> {
        let bytes = B64.decode(s.trim())?;
        let p: PairPayload = serde_json::from_slice(&bytes)?;
        if p.v != 1 {
            return Err(Error::Invalid(format!("unsupported pairing version {}", p.v)));
        }
        Ok(p)
    }

    pub fn to_url(&self, web_url: &str) -> Result<String> {
        let base = web_url.trim_end_matches('/');
        Ok(format!("{base}/#pair={}", self.to_fragment()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Keypair;

    #[test]
    fn roundtrip() {
        let k = Keypair::generate().unwrap();
        let p = PairPayload {
            v: 1,
            relay: "wss://r.example:2097/v1/ws".into(),
            host: k.public,
            host_name: "mac".into(),
            token: "abc".into(),
            exp: 1,
        };
        let url = p.to_url("https://app.example/").unwrap();
        let frag = url.split("#pair=").nth(1).unwrap();
        let back = PairPayload::from_fragment(frag).unwrap();
        assert_eq!(back.host, k.public);
        assert_eq!(back.relay, p.relay);
    }
}
