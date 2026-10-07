//! Static X25519 identities. Every host and every client device owns one keypair.
//! The public key (32 bytes) is the identity used for pairing, routing and authorization.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const KEY_LEN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey(pub [u8; KEY_LEN]);

impl PublicKey {
    pub fn to_b64(&self) -> String {
        B64.encode(self.0)
    }

    pub fn from_b64(s: &str) -> Result<Self> {
        let v = B64.decode(s.trim())?;
        Self::from_slice(&v)
    }

    pub fn from_slice(v: &[u8]) -> Result<Self> {
        let arr: [u8; KEY_LEN] = v
            .try_into()
            .map_err(|_| Error::Invalid(format!("public key must be {KEY_LEN} bytes")))?;
        Ok(Self(arr))
    }

    /// Short human-comparable fingerprint (first 8 bytes of BLAKE2s, hex, grouped).
    pub fn fingerprint(&self) -> String {
        use blake2::{Blake2s256, Digest};
        let h = Blake2s256::digest(self.0);
        h[..8]
            .chunks(2)
            .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
            .collect::<Vec<_>>()
            .join("-")
    }
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", self.to_b64())
    }
}

impl std::fmt::Display for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_b64())
    }
}

impl Serialize for PublicKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_b64())
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        PublicKey::from_b64(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Keypair {
    #[serde(with = "b64_secret")]
    pub private: [u8; KEY_LEN],
    pub public: PublicKey,
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keypair {{ public: {} }}", self.public)
    }
}

impl Keypair {
    pub fn generate() -> Result<Self> {
        let builder = snow::Builder::new(crate::noise::NOISE_PARAMS.parse()?);
        let kp = builder.generate_keypair()?;
        Self::from_private(&kp.private)
    }

    pub fn from_private(private: &[u8]) -> Result<Self> {
        let private: [u8; KEY_LEN] = private
            .try_into()
            .map_err(|_| Error::Invalid("private key must be 32 bytes".into()))?;
        let public = x25519_public(&private)?;
        Ok(Self { private, public })
    }

    /// X25519 shared secret with a remote static key. Used for the relay auth proof.
    pub fn dh(&self, remote: &PublicKey) -> Result<[u8; KEY_LEN]> {
        use snow::resolvers::{CryptoResolver, DefaultResolver};
        use snow::params::DHChoice;
        let mut dh = DefaultResolver
            .resolve_dh(&DHChoice::Curve25519)
            .ok_or_else(|| Error::Invalid("no curve25519 resolver".into()))?;
        dh.set(&self.private);
        let mut out = [0u8; KEY_LEN];
        dh.dh(&remote.0, &mut out)?;
        if out == [0u8; KEY_LEN] {
            return Err(Error::Invalid("low-order remote key".into()));
        }
        Ok(out)
    }
}

fn x25519_public(private: &[u8; KEY_LEN]) -> Result<PublicKey> {
    use snow::params::DHChoice;
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .ok_or_else(|| Error::Invalid("no curve25519 resolver".into()))?;
    dh.set(private);
    PublicKey::from_slice(dh.pubkey())
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| Error::Rng(e.to_string()))?;
    Ok(b)
}

mod b64_secret {
    use super::{B64, KEY_LEN};
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; KEY_LEN], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&B64.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; KEY_LEN], D::Error> {
        let s = String::deserialize(d)?;
        let v = B64.decode(s.trim()).map_err(serde::de::Error::custom)?;
        v.try_into()
            .map_err(|_| serde::de::Error::custom("private key must be 32 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dh_is_symmetric() {
        let a = Keypair::generate().unwrap();
        let b = Keypair::generate().unwrap();
        assert_eq!(a.dh(&b.public).unwrap(), b.dh(&a.public).unwrap());
        let a2 = Keypair::from_private(&a.private).unwrap();
        assert_eq!(a2.public, a.public);
        let s = serde_json::to_string(&a).unwrap();
        let a3: Keypair = serde_json::from_str(&s).unwrap();
        assert_eq!(a3.public, a.public);
        assert_eq!(a.public.fingerprint().len(), 19);
    }
}
