# yonder requirements (v0, from grill session 2026-09-27)

yonder lets a phone drive CLI agents and servers running on your own machines
through a self-hosted, end-to-end encrypted relay. Agent-agnostic by design.

## Decisions

| # | Topic | Decision |
|---|-------|----------|
| 1 | Audience | Personal use first, then open source. No account system; nothing about the author's hosts hardcoded. |
| 2 | Session model | PTY is the base layer for every session (any CLI works). Recognized agents additionally get a structured chat view. |
| 3 | Session origin | Create from phone, or attach to sessions started on the computer via `yonder run <cmd>`. Multi-client attach (tmux-like). Unwrapped processes cannot be adopted. |
| 4 | Security | Mandatory end-to-end encryption. Relay forwards ciphertext only; no opt-out. |
| 5 | Client | Native iOS app (SwiftUI; terminal via SwiftTerm, to verify). Dev builds signed with a free Apple ID (7-day expiry, no APNs). Protocol stays client-agnostic. |
| 6 | Host platforms | macOS, Linux, native Windows (ConPTY, Win10 1809+). Service: launchd / systemd / logon-start task. |
| 7 | Language | Rust for host and relay; shared crate for protocol and crypto. PTY via portable-pty (to verify on all 3 OSes first). |
| 8 | Chat adapters (v1) | Codex (`codex app-server`), pi (RPC mode), Claude Code (stream-json / SDK). All map to one event model: message, tool_call, approval_request, diff, status. Integration details to verify per agent. |
| 9 | View switching | View type fixed at creation. "Continue as chat" re-opens the agent via its own session id (resume); no live migration. |
| 10 | Process model | Per-session supervisor process owns PTY/RPC pipes and writes a local event log. Host daemon restarts/upgrades do not kill agents. Machine reboot requires resume. |
| 11 | Pairing | QR pairing (`yonder pair`): host pubkey + relay URL + one-time code, then Noise handshake. Per-host authorized device list; revoke locally, never via relay. Phone key in Keychain. |
| 12 | Relay | Single binary, WebSocket over TLS, built-in ACME; `--behind-proxy` for nginx setups. Stateless apart from a small in-memory ciphertext buffer. Target RSS: tens of MB. |
| 13 | Notifications | Pluggable channels, sent directly by host: ntfy, Bark, webhook in v1; APNs later. Default payload is metadata only (host, session, event type). |
| 14 | Files | Bidirectional transfer plus full remote file manager. Safeguards: per-device permission tiers, trash instead of delete, Face ID gate, configurable root allowlist (default home), local audit log. |
| 15 | v1 scope | Everything above, delivered in layers, each verified on Mac, Linux (the Linux host) and Windows. |
| 16 | Name | yonder. Crates: yonder-proto, yonder-relay, yonder-host, yonder-cli (all free on crates.io as of 2026-09-27). |

## Delivery layers

1. Protocol crate and crypto; cross-platform PTY spike on Mac, Linux, Windows.
2. Relay and host; end-to-end flow with a CLI client.
3. iOS terminal view on the author's phone.
4. Codex, pi and Claude Code chat adapters.
5. File manager and notification channels.

## Open items to verify before building

- portable-pty behavior on ConPTY (resize, Ctrl-C, detach).
- SwiftTerm maintenance status.
- Exact integration surfaces of codex app-server, pi RPC mode, Claude Code stream-json and permission forwarding.
- Whether Codex TUI and app-server can share one thread.
- App Store / trademark check for the name.

## Prerequisites

- Xcode installed on the Mac (currently only Command Line Tools).
- Relay deployment target: the relay host behind existing nginx and acme.sh.
