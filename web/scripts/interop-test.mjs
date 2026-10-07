// Noise interop: wasm initiator (browser code path) <-> Rust responder example.
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { createInterface } from 'node:readline';
import init, { generateKeypair, Handshake } from '../src/wasm/yonder_wasm.js';

const wasm = readFileSync(new URL('../src/wasm/yonder_wasm_bg.wasm', import.meta.url));
await init({ module_or_path: wasm });
const hex = (b) => Buffer.from(b).toString('hex');
const unhex = (s) => new Uint8Array(Buffer.from(s, 'hex'));

const child = spawn('cargo', ['run', '-q', '-p', 'yonder-proto', '--example', 'noise_responder'], { stdio: ['pipe', 'pipe', 'inherit'] });
const rl = createInterface({ input: child.stdout });
const lines = rl[Symbol.asyncIterator]();
const next = async () => (await lines.next()).value;

const hostPub = await next();
const kp = generateKeypair();
const hs = new Handshake(kp, hostPub);
const m1 = hs.writeHello(JSON.stringify({ protocol: 1, device_name: 'node', client: 'web', pair_token: 'x' }));
child.stdin.write(hex(m1) + '\n');
const ch = hs.readResponse(unhex(await next()));
const hello = JSON.parse(ch.hostHello());
if (!hello.ok || hello.host_name !== 'interop') throw new Error('bad host hello');

const big = 'x'.repeat(200_000);
for (const msg of ['{"t":"req","id":1}', big]) {
  for (const f of ch.encrypt(msg)) child.stdin.write(hex(f) + '\n');
  const frags = (await next()).split(',');
  let got;
  for (const f of frags) got = ch.decrypt(unhex(f)) ?? got;
  if (got !== 'echo:' + msg) throw new Error('mismatch for len ' + msg.length);
}
child.stdin.end();
console.log('INTEROP_OK wasm<->rust noise, fragmented 200KB roundtrip');
process.exit(0);
