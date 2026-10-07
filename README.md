# yonder

Drive CLI agents and terminals on your own machines from your phone or any browser,
through a self-hosted relay that only ever sees end-to-end encrypted traffic.

- Any CLI works as a terminal session (PTY on macOS and Linux, ConPTY on Windows).
- Codex, Claude Code and pi also get a chat view: streaming replies, tool calls, diffs and
  approval cards you can answer from the phone.
- Sessions run in their own processes on the host; close the browser, restart the daemon,
  come back later and reattach.
- File manager (browse, upload, download, rename, delete), push notifications for
  approvals and finished turns, pairing by QR code.
- Noise IK between the device and the host; the relay forwards ciphertext and cannot read
  or inject anything.

Status: working on macOS, Linux and Windows hosts; web client (PWA) for iPhone, Android and
desktop browsers. See docs/deploy.md to set it up and docs/handoff.md for the current state.

## Layout

- crates/yonder-proto   wire protocol, framing, Noise, pairing (TS types generated for web)
- crates/yonder-relay   relay server (WebSocket, TLS or behind a proxy, ciphertext only)
- crates/yonder-pty     cross-platform PTY and detached session supervisors
- crates/yonder-agents  Codex / Claude Code / pi chat adapters, history, model lists
- crates/yonder-fs      sandboxed file operations
- crates/yonder-notify  ntfy, Bark, webhooks and Web Push (RFC 8291)
- crates/yonder-host    host daemon: relay link, sessions, chat supervisors, control socket
- crates/yonder-cli     the `yonder` binary (daemon, service install, pair, run, attach, ...)
- crates/yonder-wasm    Noise and keys for the web client
- web/                  the web client (React, Vite, xterm.js)
- docs/                 architecture, client protocol, deployment, handoff

## Quick start

```sh
yonder init --name laptop --relay wss://relay.example.com/v1/ws
yonder service install
yonder pair          # scan the QR code with your phone
```

`yonder run codex` starts a terminal session locally that you can pick up from the phone;
`yonder ls`, `yonder attach <id>`, `yonder devices` and `yonder status` do what they say.

## Development

```sh
cargo test --workspace                       # includes an end-to-end test through a relay
cargo test -p yonder-agents --test live -- --ignored --test-threads 1   # real agents
cd web && pnpm test && pnpm build
cd web && pnpm e2e all all                  # UI end to end: temp relay + daemon + fake agent (needs debug build + pnpm build)
cargo test -p yonder-proto --features ts     # regenerate web/src/proto/generated
```
