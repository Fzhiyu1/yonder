//! End-to-end channel between a client device (initiator) and a host (responder).
//!
//! Pattern `Noise_IK_25519_ChaChaPoly_BLAKE2s`: the device already knows the host
//! static key from pairing, so the first message is encrypted to the host and
//! authenticates the device static key. The relay only ever sees these bytes.
//!
//! Handshake payloads are JSON ([`DeviceHello`], [`HostHello`]).
//! Transport messages carry one fragment each: `[flags u8][chunk]`, where
//! `flags & 1` means "more fragments follow". [`Channel`] splits and reassembles
//! so application messages can be larger than one Noise message.

use serde::{Deserialize, Serialize};
use snow::{HandshakeState, TransportState};

use crate::keys::{Keypair, PublicKey};
use crate::{Error, Result};

pub const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
pub const PROLOGUE: &[u8] = b"yonder-e2e-v1";
/// Max Noise message length (spec limit).
pub const MAX_NOISE_MSG: usize = 65_535;
const TAG_LEN: usize = 16;
/// Plaintext bytes per fragment (leaves room for flag byte and AEAD tag).
pub const FRAGMENT_PLAINTEXT: usize = 60_000;
/// Reassembled application message limit.
pub const MAX_APP_MESSAGE: usize = 32 * 1024 * 1024;

const FLAG_MORE: u8 = 1;

/// Sent by the device inside handshake message 1 (encrypted).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DeviceHello {
    pub protocol: u32,
    /// Human readable device name, e.g. "iPhone".
    pub device_name: String,
    /// Client kind: "web", "ios", "cli".
    pub client: String,
    /// One-time pairing token from the QR code. Present only on first contact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub pair_token: Option<String>,
    /// Optional capabilities understood by this client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub features: Option<Vec<String>>,
}

/// Sent by the host inside handshake message 2 (encrypted).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HostHello {
    pub protocol: u32,
    pub ok: bool,
    /// Machine-readable reason when `ok == false`: `not_paired`, `pair_token_invalid`,
    /// `revoked`, `protocol_mismatch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub error: Option<String>,
    pub host_name: String,
    /// `macos`, `linux`, `windows`.
    pub os: String,
    pub version: String,
    /// Permissions granted to this device: `sessions`, `files`.
    pub permissions: Vec<String>,
    /// Optional capabilities supported by this host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub features: Option<Vec<String>>,
}

fn builder<'a>() -> Result<snow::Builder<'a>> {
    Ok(snow::Builder::new(NOISE_PARAMS.parse()?).prologue(PROLOGUE)?)
}

/// Device side of the handshake.
pub struct Initiator {
    hs: HandshakeState,
}

impl Initiator {
    pub fn new(local: &Keypair, host: &PublicKey) -> Result<Self> {
        let hs = builder()?
            .local_private_key(&local.private)?
            .remote_public_key(&host.0)?
            .build_initiator()?;
        Ok(Self { hs })
    }

    /// Produce handshake message 1.
    pub fn write_hello(&mut self, hello: &DeviceHello) -> Result<Vec<u8>> {
        let payload = serde_json::to_vec(hello)?;
        let mut buf = vec![0u8; MAX_NOISE_MSG];
        let n = self.hs.write_message(&payload, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume handshake message 2 and switch to transport mode.
    pub fn read_response(mut self, msg: &[u8]) -> Result<(Channel, HostHello)> {
        let mut buf = vec![0u8; MAX_NOISE_MSG];
        let n = self.hs.read_message(msg, &mut buf)?;
        let hello: HostHello = serde_json::from_slice(&buf[..n])?;
        let remote = PublicKey::from_slice(
            self.hs
                .get_remote_static()
                .ok_or_else(|| Error::Invalid("missing remote static".into()))?,
        )?;
        let ts = self.hs.into_transport_mode()?;
        Ok((Channel::new(ts, remote), hello))
    }
}

/// Host side of the handshake.
pub struct Responder {
    hs: HandshakeState,
    remote: Option<PublicKey>,
}

impl Responder {
    pub fn new(local: &Keypair) -> Result<Self> {
        let hs = builder()?.local_private_key(&local.private)?.build_responder()?;
        Ok(Self { hs, remote: None })
    }

    /// Consume handshake message 1. Returns the authenticated device key and its hello.
    pub fn read_hello(&mut self, msg: &[u8]) -> Result<(PublicKey, DeviceHello)> {
        let mut buf = vec![0u8; MAX_NOISE_MSG];
        let n = self.hs.read_message(msg, &mut buf)?;
        let hello: DeviceHello = serde_json::from_slice(&buf[..n])?;
        let remote = PublicKey::from_slice(
            self.hs
                .get_remote_static()
                .ok_or_else(|| Error::Invalid("missing remote static".into()))?,
        )?;
        self.remote = Some(remote);
        Ok((remote, hello))
    }

    /// Produce handshake message 2 and switch to transport mode.
    pub fn write_response(mut self, hello: &HostHello) -> Result<(Channel, Vec<u8>)> {
        let remote = self
            .remote
            .ok_or_else(|| Error::Invalid("read_hello must be called first".into()))?;
        let payload = serde_json::to_vec(hello)?;
        let mut buf = vec![0u8; MAX_NOISE_MSG];
        let n = self.hs.write_message(&payload, &mut buf)?;
        buf.truncate(n);
        let ts = self.hs.into_transport_mode()?;
        Ok((Channel::new(ts, remote), buf))
    }
}

/// Established transport. Not thread-safe; wrap in a mutex or own it in one task.
pub struct Channel {
    ts: TransportState,
    remote: PublicKey,
    rx: Vec<u8>,
}

impl Channel {
    fn new(ts: TransportState, remote: PublicKey) -> Self {
        Self { ts, remote, rx: Vec::new() }
    }

    pub fn remote(&self) -> PublicKey {
        self.remote
    }

    /// Encrypt one application message into one or more Noise messages.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<Vec<u8>>> {
        if plaintext.len() > MAX_APP_MESSAGE {
            return Err(Error::Invalid("application message too large".into()));
        }
        let mut out = Vec::with_capacity(plaintext.len() / FRAGMENT_PLAINTEXT + 1);
        let mut chunks = plaintext.chunks(FRAGMENT_PLAINTEXT).peekable();
        if chunks.peek().is_none() {
            out.push(self.seal(0, &[])?);
            return Ok(out);
        }
        while let Some(chunk) = chunks.next() {
            let flags = if chunks.peek().is_some() { FLAG_MORE } else { 0 };
            out.push(self.seal(flags, chunk)?);
        }
        Ok(out)
    }

    fn seal(&mut self, flags: u8, chunk: &[u8]) -> Result<Vec<u8>> {
        let mut pt = Vec::with_capacity(chunk.len() + 1);
        pt.push(flags);
        pt.extend_from_slice(chunk);
        let mut buf = vec![0u8; pt.len() + TAG_LEN];
        let n = self.ts.write_message(&pt, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Decrypt one Noise message. Returns a complete application message once the
    /// final fragment arrives. Any error means the channel must be dropped.
    pub fn decrypt(&mut self, msg: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; msg.len()];
        let n = self.ts.read_message(msg, &mut buf)?;
        if n == 0 {
            return Err(Error::Invalid("empty fragment".into()));
        }
        let flags = buf[0];
        if self.rx.len() + n - 1 > MAX_APP_MESSAGE {
            return Err(Error::Invalid("application message too large".into()));
        }
        self.rx.extend_from_slice(&buf[1..n]);
        if flags & FLAG_MORE != 0 {
            Ok(None)
        } else {
            Ok(Some(std::mem::take(&mut self.rx)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Channel, Channel, DeviceHello) {
        let host = Keypair::generate().unwrap();
        let dev = Keypair::generate().unwrap();
        let mut ini = Initiator::new(&dev, &host.public).unwrap();
        let m1 = ini
            .write_hello(&DeviceHello {
                protocol: 1,
                device_name: "test".into(),
                client: "cli".into(),
                pair_token: Some("tok".into()),
                features: None,
            })
            .unwrap();
        let mut resp = Responder::new(&host).unwrap();
        let (who, hello) = resp.read_hello(&m1).unwrap();
        assert_eq!(who, dev.public);
        let (hc, m2) = resp
            .write_response(&HostHello { protocol: 1, ok: true, host_name: "h".into(), ..Default::default() })
            .unwrap();
        let (dc, hh) = ini.read_response(&m2).unwrap();
        assert!(hh.ok);
        assert_eq!(dc.remote(), host.public);
        (dc, hc, hello)
    }

    #[test]
    fn handshake_and_fragmentation() {
        let (mut dc, mut hc, hello) = pair();
        assert_eq!(hello.pair_token.as_deref(), Some("tok"));
        for size in [0usize, 1, FRAGMENT_PLAINTEXT, FRAGMENT_PLAINTEXT + 1, 3 * FRAGMENT_PLAINTEXT + 17] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let frags = dc.encrypt(&data).unwrap();
            assert!(frags.iter().all(|f| f.len() <= MAX_NOISE_MSG));
            let mut got = None;
            for (i, f) in frags.iter().enumerate() {
                let r = hc.decrypt(f).unwrap();
                if i + 1 < frags.len() {
                    assert!(r.is_none());
                } else {
                    got = r;
                }
            }
            assert_eq!(got.unwrap(), data);
        }
        let back = hc.encrypt(b"pong").unwrap();
        assert_eq!(dc.decrypt(&back[0]).unwrap().unwrap(), b"pong");
    }

    #[test]
    fn wrong_host_key_fails() {
        let host = Keypair::generate().unwrap();
        let other = Keypair::generate().unwrap();
        let dev = Keypair::generate().unwrap();
        let mut ini = Initiator::new(&dev, &other.public).unwrap();
        let m1 = ini.write_hello(&DeviceHello::default()).unwrap();
        let mut resp = Responder::new(&host).unwrap();
        assert!(resp.read_hello(&m1).is_err());
    }

    #[test]
    fn tampering_fails() {
        let (mut dc, mut hc, _) = pair();
        let mut f = dc.encrypt(b"hello").unwrap().remove(0);
        let last = f.len() - 1;
        f[last] ^= 1;
        assert!(hc.decrypt(&f).is_err());
    }

    #[test]
    fn old_hello_defaults_to_no_features() {
        let old_device: DeviceHello = serde_json::from_str(
            r#"{"protocol":1,"device_name":"old","client":"web"}"#,
        )
        .unwrap();
        assert!(old_device.features.is_none());
        let old_host: HostHello = serde_json::from_str(
            r#"{"protocol":1,"ok":true,"host_name":"old","os":"linux","version":"0.1.0","permissions":[]}"#,
        )
        .unwrap();
        assert!(old_host.features.is_none());
    }
}
