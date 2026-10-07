//! Browser bindings: device identity, relay auth proof, Noise IK initiator and channel.
//! All crypto stays in Rust; JS only moves opaque bytes and JSON strings.

use wasm_bindgen::prelude::*;
use yonder_proto::keys::{Keypair, PublicKey};
use yonder_proto::noise::{self, DeviceHello};
use yonder_proto::relay::{self, Role};

fn js_err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

/// Returns a new device keypair as JSON `{"private": "...", "public": "..."}`.
#[wasm_bindgen(js_name = generateKeypair)]
pub fn generate_keypair() -> Result<String, JsError> {
    let kp = Keypair::generate().map_err(js_err)?;
    serde_json::to_string(&kp).map_err(js_err)
}

/// Public key (base64url) for a keypair JSON.
#[wasm_bindgen(js_name = publicKeyOf)]
pub fn public_key_of(keypair_json: &str) -> Result<String, JsError> {
    let kp: Keypair = serde_json::from_str(keypair_json).map_err(js_err)?;
    Ok(kp.public.to_b64())
}

#[wasm_bindgen]
pub fn fingerprint(public_b64: &str) -> Result<String, JsError> {
    Ok(PublicKey::from_b64(public_b64).map_err(js_err)?.fingerprint())
}

/// Relay auth proof for a device. `nonce_b64` is the standard-base64 nonce from `challenge`.
#[wasm_bindgen(js_name = relayAuthProof)]
pub fn relay_auth_proof(keypair_json: &str, relay_pub_b64: &str, nonce_b64: &str) -> Result<String, JsError> {
    let kp: Keypair = serde_json::from_str(keypair_json).map_err(js_err)?;
    let relay_pub = PublicKey::from_b64(relay_pub_b64).map_err(js_err)?;
    let nonce = relay::decode_nonce(nonce_b64).map_err(js_err)?;
    relay::auth_proof(&kp, &relay_pub, &nonce, Role::Device).map_err(js_err)
}

/// Handshake in progress (device side).
#[wasm_bindgen]
pub struct Handshake {
    inner: Option<noise::Initiator>,
}

#[wasm_bindgen]
impl Handshake {
    /// Start a handshake with a host. `hello_json` is a `DeviceHello`.
    #[wasm_bindgen(constructor)]
    pub fn new(keypair_json: &str, host_pub_b64: &str) -> Result<Handshake, JsError> {
        let kp: Keypair = serde_json::from_str(keypair_json).map_err(js_err)?;
        let host = PublicKey::from_b64(host_pub_b64).map_err(js_err)?;
        let ini = noise::Initiator::new(&kp, &host).map_err(js_err)?;
        Ok(Handshake { inner: Some(ini) })
    }

    /// Handshake message 1 bytes.
    #[wasm_bindgen(js_name = writeHello)]
    pub fn write_hello(&mut self, hello_json: &str) -> Result<Vec<u8>, JsError> {
        let hello: DeviceHello = serde_json::from_str(hello_json).map_err(js_err)?;
        self.inner
            .as_mut()
            .ok_or_else(|| JsError::new("handshake already finished"))?
            .write_hello(&hello)
            .map_err(js_err)
    }

    /// Consume message 2. Returns the channel; `hostHello()` on it gives the host JSON.
    #[wasm_bindgen(js_name = readResponse)]
    pub fn read_response(&mut self, msg: &[u8]) -> Result<Channel, JsError> {
        let ini = self.inner.take().ok_or_else(|| JsError::new("handshake already finished"))?;
        let (ch, hello) = ini.read_response(msg).map_err(js_err)?;
        let host_hello = serde_json::to_string(&hello).map_err(js_err)?;
        Ok(Channel { inner: ch, host_hello })
    }
}

/// Established encrypted channel.
#[wasm_bindgen]
pub struct Channel {
    inner: noise::Channel,
    host_hello: String,
}

#[wasm_bindgen]
impl Channel {
    #[wasm_bindgen(js_name = hostHello)]
    pub fn host_hello(&self) -> String {
        self.host_hello.clone()
    }

    /// Encrypt one JSON app message. Returns an array of Noise messages (Uint8Array[]).
    pub fn encrypt(&mut self, json: &str) -> Result<js_sys_array::Array, JsError> {
        let frags = self.inner.encrypt(json.as_bytes()).map_err(js_err)?;
        Ok(js_sys_array::Array::from_frags(frags))
    }

    /// Decrypt one Noise message. Returns the complete JSON message, or undefined while
    /// more fragments are pending. Throws on authentication failure (drop the link).
    pub fn decrypt(&mut self, msg: &[u8]) -> Result<Option<String>, JsError> {
        match self.inner.decrypt(msg).map_err(js_err)? {
            Some(bytes) => Ok(Some(String::from_utf8(bytes).map_err(js_err)?)),
            None => Ok(None),
        }
    }
}

mod js_sys_array {
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_name = Array)]
        pub type Array;
        #[wasm_bindgen(constructor, js_class = "Array")]
        fn new() -> Array;
        #[wasm_bindgen(method, js_class = "Array")]
        fn push(this: &Array, v: &JsValue) -> u32;
        #[wasm_bindgen(js_namespace = Uint8Array, js_name = from)]
        fn u8_from(v: &JsValue) -> JsValue;
    }

    impl Array {
        pub fn from_frags(frags: Vec<Vec<u8>>) -> Array {
            let arr = Array::new();
            for f in frags {
                let v: JsValue = f.into();
                arr.push(&v);
            }
            arr
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_json_roundtrip() {
        let j = generate_keypair().ok().unwrap();
        let p = public_key_of(&j).ok().unwrap();
        assert_eq!(p.len(), 43);
    }
}
