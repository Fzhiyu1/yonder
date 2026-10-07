//! Interop helper: acts as a host responder over stdin/stdout (hex lines).
//! Prints host public key, reads msg1 hex, prints msg2 hex, then echoes decrypted
//! messages back re-encrypted with a "echo:" prefix. Used by web/scripts/interop-test.mjs.
use std::io::{BufRead, Write};
use yonder_proto::keys::Keypair;
use yonder_proto::noise::{HostHello, Responder};

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
fn unhex(s: &str) -> Vec<u8> { (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect() }

fn main() {
    let host = Keypair::generate().unwrap();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    writeln!(out, "{}", host.public.to_b64()).unwrap();
    out.flush().unwrap();
    let mut lines = stdin.lock().lines();
    let m1 = unhex(lines.next().unwrap().unwrap().trim());
    let mut r = Responder::new(&host).unwrap();
    let (dev, hello) = r.read_hello(&m1).unwrap();
    eprintln!("device {} hello {:?}", dev, hello);
    let (mut ch, m2) = r
        .write_response(&HostHello { protocol: 1, ok: true, host_name: "interop".into(), os: "test".into(), ..Default::default() })
        .unwrap();
    writeln!(out, "{}", hex(&m2)).unwrap();
    out.flush().unwrap();
    for line in lines {
        let line = line.unwrap();
        if line.trim().is_empty() { continue; }
        if let Some(msg) = ch.decrypt(&unhex(line.trim())).unwrap() {
            let reply = format!("echo:{}", String::from_utf8(msg).unwrap());
            let frags = ch.encrypt(reply.as_bytes()).unwrap();
            writeln!(out, "{}", frags.iter().map(|f| hex(f)).collect::<Vec<_>>().join(",")).unwrap();
            out.flush().unwrap();
        }
    }
}
