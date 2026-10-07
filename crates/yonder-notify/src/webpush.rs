//! RFC 8291 message encryption (`aes128gcm`, RFC 8188) and RFC 8292 VAPID.

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use sha2::Sha256;

/// Record size advertised in the header. Payloads are always a single record.
const RECORD_SIZE: u32 = 4096;
/// Max plaintext in one record: rs - 16 (tag) - 1 (delimiter).
const MAX_PLAINTEXT: usize = RECORD_SIZE as usize - 17;

pub(crate) fn b64d(s: &str) -> Result<Vec<u8>> {
    let t = s.trim().trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(t)
        .or_else(|_| STANDARD.decode(s.trim()))
        .map_err(|e| anyhow!("invalid base64: {e}"))
}

pub(crate) fn b64e(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| anyhow!("getrandom: {e}"))?;
    Ok(b)
}

fn random_secret() -> Result<SecretKey> {
    loop {
        let b: [u8; 32] = random_bytes()?;
        if let Ok(k) = SecretKey::from_slice(&b) {
            return Ok(k);
        }
    }
}

/// Uncompressed SEC1 encoding (65 bytes).
fn uncompressed(pk: &PublicKey) -> Vec<u8> {
    pk.to_encoded_point(false).as_bytes().to_vec()
}

/// Encrypts `plaintext` for one push subscription with a fresh sender key and salt.
pub(crate) fn encrypt(plaintext: &[u8], ua_public: &[u8], auth_secret: &[u8]) -> Result<Vec<u8>> {
    let as_key = random_secret()?;
    let salt: [u8; 16] = random_bytes()?;
    encrypt_aes128gcm(plaintext, ua_public, auth_secret, &as_key.to_bytes(), &salt)
}

/// Deterministic RFC 8291 encryption with a caller-chosen sender private key and salt.
/// Returns `salt || rs || idlen || as_public || ciphertext`.
pub fn encrypt_aes128gcm(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    as_private: &[u8],
    salt: &[u8; 16],
) -> Result<Vec<u8>> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(anyhow!("push payload too large ({} bytes)", plaintext.len()));
    }
    if auth_secret.len() != 16 {
        return Err(anyhow!("auth secret must be 16 bytes"));
    }
    let ua_pk = PublicKey::from_sec1_bytes(ua_public).map_err(|_| anyhow!("invalid user agent key"))?;
    let ua_bytes = uncompressed(&ua_pk);
    let as_sk = SecretKey::from_slice(as_private).map_err(|_| anyhow!("invalid sender key"))?;
    let as_bytes = uncompressed(&as_sk.public_key());

    let shared = p256::ecdh::diffie_hellman(as_sk.to_nonzero_scalar(), ua_pk.as_affine());

    // IKM = HKDF(auth_secret, ecdh_secret, "WebPush: info" || 0x00 || ua_public || as_public, 32)
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(&ua_bytes);
    key_info.extend_from_slice(&as_bytes);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes().as_slice())
        .expand(&key_info, &mut ikm)
        .map_err(|_| anyhow!("hkdf ikm"))?;

    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|_| anyhow!("hkdf cek"))?;
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|_| anyhow!("hkdf nonce"))?;

    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02); // last-record padding delimiter
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| anyhow!("aes key"))?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &record, aad: &[] })
        .map_err(|_| anyhow!("aes-gcm encrypt"))?;

    let mut out = Vec::with_capacity(16 + 4 + 1 + 65 + ct.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_bytes.len() as u8);
    out.extend_from_slice(&as_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// The host's VAPID key pair (P-256).
pub struct VapidKey {
    signing: SigningKey,
    public: Vec<u8>,
}

impl VapidKey {
    pub fn from_private(bytes: &[u8]) -> Result<Self> {
        let sk = SecretKey::from_slice(bytes).map_err(|_| anyhow!("invalid VAPID private key"))?;
        let public = uncompressed(&sk.public_key());
        Ok(Self { signing: SigningKey::from(sk), public })
    }

    pub fn generate() -> Result<Self> {
        Self::from_private(&random_secret()?.to_bytes())
    }

    /// Reads a base64url private key from `path`, or creates one (mode 0600).
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let bytes = b64d(s.trim()).with_context(|| format!("parse {}", path.display()))?;
                Self::from_private(&bytes)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate()?;
                crate::write_private(path, format!("{}\n", b64e(&key.signing.to_bytes())).as_bytes())?;
                Ok(key)
            }
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn public_bytes(&self) -> &[u8] {
        &self.public
    }

    pub fn public_b64(&self) -> String {
        b64e(&self.public)
    }

    /// ES256 JWT for the given audience (push service origin), expiry (unix seconds) and
    /// contact (`sub`: a `mailto:` or `https:` URI).
    pub fn jwt(&self, aud: &str, exp: u64, sub: &str) -> String {
        let header = b64e(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({ "aud": aud, "exp": exp, "sub": sub });
        let claims = b64e(claims.to_string().as_bytes());
        let signing_input = format!("{header}.{claims}");
        let sig: Signature = self.signing.sign(signing_input.as_bytes());
        format!("{signing_input}.{}", b64e(&sig.to_bytes()))
    }
}

/// `Authorization` header value for RFC 8292 (`vapid t=<jwt>, k=<public key>`).
pub fn vapid_authorization(key: &VapidKey, aud: &str, exp: u64, sub: &str) -> Result<String> {
    if aud.is_empty() {
        return Err(anyhow!("empty VAPID audience"));
    }
    if !(sub.starts_with("mailto:") || sub.starts_with("https://")) {
        return Err(anyhow!("VAPID contact must be a mailto: or https: URI, got {sub:?}"));
    }
    Ok(format!("vapid t={}, k={}", key.jwt(aud, exp, sub), key.public_b64()))
}
