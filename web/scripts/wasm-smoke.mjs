// Node smoke test for the wasm crypto bindings (no Rust host needed).
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import init, { fingerprint, generateKeypair, Handshake, publicKeyOf, relayAuthProof } from '../src/wasm/yonder_wasm.js';

const wasm = readFileSync(new URL('../src/wasm/yonder_wasm_bg.wasm', import.meta.url));
await init({ module_or_path: wasm });

const kp = generateKeypair();
const parsed = JSON.parse(kp);
assert.ok(parsed.private && parsed.public, 'keypair has private + public');

const pub = publicKeyOf(kp);
assert.equal(pub, parsed.public, 'publicKeyOf matches keypair.public');
assert.equal(Buffer.from(pub, 'base64url').length, 32, 'public key is 32 bytes');

const fp = fingerprint(pub);
assert.ok(typeof fp === 'string' && fp.length >= 8, 'fingerprint is a string');
assert.equal(fingerprint(pub), fp, 'fingerprint is deterministic');

const host = JSON.parse(generateKeypair());
assert.notEqual(host.public, pub, 'keypairs are random');

const nonce = Buffer.alloc(32, 7).toString('base64');
const proof = relayAuthProof(kp, host.public, nonce);
assert.ok(proof.length > 0, 'relay auth proof produced');
assert.equal(relayAuthProof(kp, host.public, nonce), proof, 'proof is deterministic for same nonce');

const hs = new Handshake(kp, host.public);
const m1 = hs.writeHello(JSON.stringify({ protocol: 1, device_name: 'smoke', client: 'web', pair_token: 'tok' }));
assert.ok(m1 instanceof Uint8Array, 'writeHello returns bytes');
// IK message 1: e (32) + encrypted s (32 + 16) + encrypted payload (+16 tag).
assert.ok(m1.length > 96, `handshake message 1 has expected size (${m1.length})`);

assert.throws(() => hs.readResponse(new Uint8Array(10)), 'garbage response is rejected');
assert.throws(() => new Handshake(kp, 'not-a-key'), 'invalid host key is rejected');

console.log(`WASM_SMOKE_OK pub=${pub.slice(0, 8)}… fp=${fp} m1=${m1.length}B`);
