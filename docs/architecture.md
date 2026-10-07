# yonder architecture (v1)

Source of truth for the wire format is `crates/yonder-proto` (Rust). TypeScript types are
generated into `web/src/proto/generated/` with `cargo test -p yonder-proto --features ts`.
Do not edit generated files; change the Rust types instead.

## Processes

```
phone / browser (web client, wasm Noise)
        |  wss  (relay protocol: JSON control + binary link frames)
   yonder-relay  (the relay host, ciphertext only, stateless)
        |  wss
   yonder host daemon  (`yonder daemon`, one per machine, user-level service)
        |-- local control socket (unix socket / named pipe): `yonder` CLI, `yonder run`
        |-- per-session supervisor processes (`yonder __supervise <id>`), survive daemon restarts
        |     owns PTY (portable-pty; ConPTY on Windows) or agent RPC pipes, writes event log
        `-- embedded web client assets (optional local dev server on 127.0.0.1)
```

Binaries:
- `yonder-relay`: relay server.
- `yonder`: everything else (daemon, supervisor, CLI). Crate `yonder-cli` builds binary
  `yonder`; the logic lives in library crate `yonder-host`.

## Relay protocol (crates/yonder-proto/src/relay.rs)

WebSocket at `/v1/ws`. Text = JSON control (`t` tag), binary = `[u32 BE link][noise msg]`.
Relay sends `challenge{relay_pub, nonce}`; peer answers `auth{role, public, proof, protocol}`
where proof = BLAKE2s-MAC(X25519(peer, relay), ctx||nonce||role||pub). One host connection per
host key (newest wins). Devices `open{req,to}` -> relay allocates link id, sends host
`incoming{link, from}` and device `opened{req, link, to}`. `close{link}` / `closed{link}`.
Devices can `watch{hosts}` to get `presence{host, online}`. Relay limits: frame <= 64KiB+4,
per-connection rate and buffered-bytes caps, max links per device, idle ping.

## E2E channel (crates/yonder-proto/src/noise.rs)

Per link: Noise_IK_25519_ChaChaPoly_BLAKE2s, prologue `yonder-e2e-v1`. Device = initiator,
knows host static key from the QR. Message 1 payload = `DeviceHello` JSON
(`pair_token` present on first contact). Message 2 payload = `HostHello` JSON; if `ok=false`
the host closes the link after sending it. Transport messages: `[flags][chunk]`, flag bit 1 =
more fragments. Every reassembled message is one UTF-8 JSON app message.

## App protocol (crates/yonder-proto/src/app.rs)

Client -> host: `{t:"req", id, req:{op,...}}`, `{t:"input", session, data(b64)}`,
`{t:"resize", session, cols, rows}`.
Host -> client: `{t:"res", id, ok, data|error}`, `{t:"event", event:{ev,...}}`.
The local control socket speaks the same JSON, newline-delimited, no Noise; it additionally
allows `create_pairing` and `status`.

## Host state on disk

Config dir (`dirs::config_dir()/yonder`, e.g. `~/Library/Application Support/yonder`,
`~/.config/yonder`, `%APPDATA%\yonder`):
- `config.toml`: name, relay_url, web_url, fs_roots, notify channels, agent overrides.
- `host.key` (0600): host keypair JSON.
- `devices.json`: authorized devices {public, name, client, paired_at, last_seen, permissions}.
- `audit.log`: JSON lines of file operations and pairing events.

Data dir (`dirs::data_local_dir()/yonder`):
- `sessions/<id>/meta.json`, `sessions/<id>/pty.log` (raw output, capped ring by rotation),
  `sessions/<id>/events.jsonl` (chat events), `sessions/<id>/supervisor.sock|pipe`.
- `uploads/` for `upload_temp`.

## Security invariants

- Relay never sees plaintext; there is no option to disable E2E.
- A device may do nothing before a successful Noise handshake and authorization:
  either its key is in `devices.json` (and not revoked) or it presents a valid unexpired
  one-time `pair_token` created by `yonder pair` on the host.
- Revocation is local to the host (`yonder devices revoke`, or `revoke_device` request).
- File operations require the `files` permission and paths inside `fs_roots`
  (default: home). Deletes go to the OS trash. Every file op is audited.
- Local control socket: unix socket with 0600 in a 0700 dir; Windows named pipe with an ACL
  restricted to the current user.
