# Client protocol guide

How a client (web PWA, iOS app, Rust test client) talks to a yonder host. The wire types
are defined in `crates/yonder-proto` (Rust) and generated to `web/src/proto/generated/`
(TypeScript). This document fixes the *behavior* around those types.

## 1. Identity and pairing

- Every client device owns one static X25519 keypair (`generateKeypair()` in wasm,
  `Keypair::generate()` in Rust). Store it persistently (IndexedDB / Keychain). The public
  key (base64url, 32 bytes) is the device identity.
- A host shows a QR / link `<web_url>/#pair=<base64url(JSON PairPayload)>`:
  `{v:1, relay:"wss://…/v1/ws", host:"<host pub b64url>", host_name, token, exp}`.
  The fragment never reaches a server. The client stores `{host, host_name, relay}` and uses
  `token` once, in the first handshake (`DeviceHello.pair_token`). After a successful
  handshake the device is authorized permanently (until revoked on the host); later
  handshakes omit `pair_token`.
- `exp` is unix ms; expired tokens are rejected by the host (`pair_token_invalid`).

## 2. Relay connection (one WebSocket per relay URL)

Text frames are JSON control messages (`t` tag); binary frames are link data
`[u32 big-endian link id][payload]`, at most 4 + 65535 bytes.

1. Relay sends `{t:"challenge", relay_pub, nonce, protocol:1}`.
2. Client sends `{t:"auth", role:"device", public, proof, protocol:1}` where
   `proof = relayAuthProof(keypair, relay_pub, nonce)`.
3. Relay answers `{t:"welcome", you, role}` or `{t:"error", code, message}` and closes.
4. `{t:"watch", hosts:[…]}` subscribes to `{t:"presence", host, online}` for every paired
   host on this relay (send again whenever the list changes; also after reconnect).
5. `{t:"open", req, to: host}` → `{t:"opened", req, link, to}` or
   `{t:"open_failed", req, to, reason}` (`host_offline`, `rate_limited`, `too_many_links`).
6. `{t:"closed", link, reason}` means the link is gone (host restarted, host closed it).
   `{t:"close", link}` closes a link from the client side.
7. `{t:"ping", ts}` → `{t:"pong", ts}`. The relay also sends WebSocket pings and drops
   connections idle for 90 s; browsers answer pings automatically. Clients should send an
   app-level `ping` every 25 s to detect dead connections (no pong in 10 s = reconnect).

Reconnect with backoff (0.5 s, 1, 2, 4, … max 15 s) and immediately on
`visibilitychange` → visible and on `online`. After reconnecting: auth, watch, reopen links,
redo the Noise handshake, re-attach the sessions being viewed with `since` (section 5).

## 3. End-to-end channel (per link)

Noise `IK_25519_ChaChaPoly_BLAKE2s`, prologue `yonder-e2e-v1`, device = initiator.

1. After `opened`, send handshake message 1 as a binary frame on the link:
   `new Handshake(keypair, host).writeHello(JSON DeviceHello)` with
   `{protocol:1, device_name, client:"web"|"ios"|"cli", pair_token?}`.
2. The first binary frame back is message 2: `handshake.readResponse(bytes)` gives a
   `Channel`; `channel.hostHello()` is JSON `HostHello`
   `{protocol, ok, error?, host_name, os, version, permissions}`.
   If `ok` is false the host closes the link. `error` is one of `not_paired`,
   `pair_token_invalid`, `revoked`, `protocol_mismatch`, `relay_mismatch`.
3. Afterwards every binary frame on the link is one Noise transport message.
   `channel.encrypt(json)` returns an array of messages (fragments); send each as its own
   frame, in order. `channel.decrypt(bytes)` returns the complete JSON string once the last
   fragment arrives (else `undefined`). Any decrypt error: drop the link and reconnect.

## 4. App messages

Client → host (`AppClientMsg`):
- `{t:"req", id, req:{op, …}}`: exactly one `{t:"res", id, ok, data|error}` comes back.
  `data` is a `Response` (tagged by `kind`); `error` is `{code, message}` with code
  `not_found`, `forbidden`, `invalid`, `exists`, `busy`, `unsupported`, `internal`.
- `{t:"input", session, data}`: keystrokes for a terminal session (standard base64).
- `{t:"resize", session, cols, rows}`: terminal viewport of this client.
- `{t:"focus", session?}`: the session this client currently shows while visible and
  focused (omit `session` when hidden/backgrounded). The host suppresses notifications for
  sessions somebody is looking at.

Host → client (`AppHostMsg`): `res` as above and `{t:"event", event:{ev, …}}`.

Suggested request timeouts: 30 s; file operations 120 s. Request ids are per link.

### Requests

| op | response `kind` | notes |
|----|-----------------|-------|
| `ping` | `pong{ts}` | |
| `host_info` | `host_info{info}` | agents (+models, `default_approval`), recent dirs, fs roots, permissions, VAPID key |
| `list_sessions` | `sessions{sessions}` | newest `updated_at` first |
| `create_session{spec}` | `session{session}` | then `attach` it; a chat without `spec.approval` uses the agent's `default_approval` |
| `attach{session, since?}` | `attached{session, terminal?, chat?}` | section 5 |
| `detach{session}` | `ok` | stop stream events for that session |
| `kill{session}` | `ok` | session stays listed with state `exited` |
| `remove{session}` | `ok` | only exited sessions; deletes host-side logs |
| `rename{session, title}` | `session{session}` | |
| `continue_as_chat{session}` | `session{session}` | terminal agent session → new chat session resuming the agent's own session id; the terminal session is killed |
| `chat_send{session, text, attachments}` | `ok` | `attachments`: host paths from `upload_temp` (images) |
| `chat_interrupt{session}` | `ok` | |
| `approval_respond{session, approval, option}` | `ok` | `option` = `ApprovalOption.id` |
| `chat_older{session, before, limit?}` | `chat_older{items, more}` | chat items before `before` (an item id the client has), oldest first |
| `chat_thread{session, thread, before?, limit?}` | `chat_thread{items, more, seq}` | one sub-agent's thread (read-only view), oldest first; section 5 |
| `set_approval_mode{session, mode}` | `session{session}` | `ask`/`auto`/`yolo`, Codex and Claude chats; when `approval_live` is false the host restarts the chat (resuming the agent session) and returns the new session |
| `agent_history{agent?, cwd?, query?, cursor?, limit?, all}` | `agent_history{sessions, next_cursor?, folders, errors}` | the agents' own sessions on the host, newest first, paged (history view, resume picker); see ADR 0002 |
| `agent_preview{agent, id}` | `agent_preview{items, truncated}` | last user messages and final agent replies of one agent session, read without starting it |
| `fs_home` | `path{path}` | default folder for the file browser |
| `fs_list{path, hidden}` | `dir{listing}` | folders first; `parent` null at a root |
| `fs_stat{path}` | `stat{entry}` | |
| `fs_read{path, offset, len}` | `file_chunk{path, offset, data, eof, size}` | `len` ≤ 1 MiB |
| `fs_write{path, offset, data, finish, overwrite}` | `ok` | chunked upload, section 6 |
| `fs_mkdir{path}` / `fs_rename{from, to, overwrite}` / `fs_delete{path}` | `ok` | delete = OS trash |
| `upload_temp{name, data}` | `path{path}` | ≤ 20 MiB, for chat attachments |
| `http_fetch{url}` | `http_response{status, headers, data}` | GET a loopback URL on the host (artifact viewer); ADR 0003 |
| `tailnet_url{url}` | `tailnet_url{url, reachable}` | the loopback `url` on the host's Tailscale IPv4; `reachable` = port answers there; `not_found` without Tailscale; ADR 0005 |
| `list_devices` | `devices{devices}` | `current` marks the caller |
| `revoke_device{device}` | `ok` | closes that device's links |
| `notify_test` | `ok` or error | error message lists failing channels |
| `push_subscribe{endpoint, p256dh, auth, vapid_private?}` | `ok` | section 7 |
| `push_unsubscribe` | `ok` | |

`create_pairing`, `status`, `shutdown` are only accepted on the host's local control socket.
Permissions: session ops need `sessions`, `fs_*`/`upload_temp`/`http_fetch`/`tailnet_url` need `files`
(`forbidden` otherwise).

### Events

Sent to every client of the host: `session_updated{session}` (created or changed: state,
title, clients, chat status, pending approvals, approval mode, preview), `session_removed{session}`,
`notice{level, message}` (toast), `device_paired{device}`.

`SessionInfo.approval` is the mode of a Codex or Claude chat (absent otherwise);
`approval_live` says whether it can change in place. See docs/adr/0001-approval-modes.md.

Sent only to clients attached to the session: `pty_output`, `pty_snapshot`, `pty_resized`,
`chat_item`, `chat_delta`, `chat_snapshot`, `chat_status`, `approval_requested`,
`approval_resolved`. `chat_item` / `chat_delta` with `thread` belong to a sub-agent's thread
(section 5).

## 5. Attaching

### Terminal sessions

`attached.terminal = {reset, data, offset, cols, rows}`:
- `reset: true`: clear the terminal (`term.reset()`), then write `data`.
- `reset: false`: `data` continues exactly from the `since` offset you sent; append it.
- Remember `offset` (bytes of output consumed so far).

Live `pty_output{offset, data}`: `offset` is the global byte offset of the first byte.
Skip bytes you already have (`offset + len <= known`), trim overlaps, and if
`offset > known` there is a gap: re-attach with `since = known`.
`pty_snapshot{snapshot}`: same as an attach with `reset: true` (sent when this client fell
behind). `pty_resized{cols, rows}`: the PTY size changed (another client took over).

Sizing: the PTY has one size; the most recently active client wins. Send `resize` with
your fitted cols/rows right after attaching, when your viewport changes, and before
sending input if the PTY size (from `attached`/`pty_resized`) differs from yours.
To re-attach after a reconnect send `attach{session, since: offset}`.

Exited terminal sessions can still be attached (history replay, no input).

### Chat sessions

`attached.chat = {items, approvals, status, seq, truncated}` replaces the whole view.
Every chat event carries `seq` (per session, increasing by one per event):
ignore events with `seq <= current`; on a gap (`seq > current + 1`) re-attach.
- `chat_item{item}`: insert (append if the id is new) or replace in place.
- `chat_delta{item, field, delta}`: append `delta` to `item.text` (`field:"text"`) or
  `item.output` (`field:"output"`). Unknown item: create an in-progress agent item.
- `chat_status{status, detail?}`: `starting|idle|working|awaiting_approval|error|exited`.
- `approval_requested{approval}` / `approval_resolved{approval, option}`.
- `chat_snapshot{snapshot}`: replace everything (sent after the client fell behind).

User messages are echoed back as `user` items; clients may show an optimistic bubble and
drop it when the echo arrives. While the agent is `working`, `chat_send` queues or steers
(agent-specific) and `chat_interrupt` stops the turn.

Approval options have `kind`: `allow`, `allow_always`, `deny`, `abort` (deny and stop the
turn), `choice` (questions). Send the chosen `ApprovalOption.id`.

### Sub-agents

See docs/adr/0006-subagents.md. A sub-agent the agent spawned is one item of kind `subagent`
in the chat: `item.subagent = {id, name?, role?, model?, status, reply?}` with `status`
`running|done|failed|interrupted|closed`, and `item.text` its task. It is updated in place
(`chat_item` with the same id) as the sub-agent runs; Codex `wait` / `close` calls produce no
items of their own.

Items of the sub-agent's own thread carry `thread` = `subagent.id`. They are never part of
`attached.chat`, `chat_snapshot` or `chat_older`; live they arrive as `chat_item` /
`chat_delta` (with `thread` set) and count for the session's seq like any event. A chat view
must not show them (advance `seq`, drop the item). To show a sub-agent:

1. `chat_thread{session, thread: subagent.id}` returns the newest page (`items` oldest first,
   `more` = older items can be fetched with `before` = the oldest id) and the `seq` it reflects.
2. Apply live `chat_item` / `chat_delta` events with that `thread` and a seq above the answer's
   `seq`. Events that arrive while the request is in flight are applied on top of the answer.
3. Reload after a reconnect. The view is read-only: there is no input to sub-agents.

`Approval.thread` / `thread_name` say that a sub-agent raised the approval. It is answered
with `approval_respond` like any other; it also sits in the chat's `approvals`.

## 6. Files

Download: `fs_read` in 1 MiB chunks until `eof`; `size` is the file size at read time.
Upload: split into 256 KiB chunks; `fs_write{offset: 0, …}` starts a hidden temp file next
to the target, each next chunk must use `offset` = bytes sent so far, the last chunk has
`finish: true` and atomically renames. `exists` at offset 0 (or at finish) means the target
exists: ask the user, then restart with `overwrite: true`. All paths are absolute host
paths; use `listing.path` / `entry.path` as returned (Windows uses `\`, see
`host_info.path_sep`). File operations are confined to `host_info.fs_roots`.

## 7. Notifications (Web Push)

A browser can hold only one push subscription per origin, but a device may pair several
hosts. So the device generates its own ECDSA P-256 VAPID key pair once, subscribes with
`applicationServerKey` = its raw public key (65 bytes), and sends every paired host
`push_subscribe{endpoint, p256dh, auth, vapid_private}` where `vapid_private` is the JWK
`d` (base64url, 32 bytes). Hosts sign VAPID JWTs with that key and encrypt payloads
(RFC 8291) to the subscription, so the push service sees ciphertext only.

Push payload (decrypted in the service worker): JSON
`{title, body, url, tag, kind, session}` with `kind` one of `approval`, `turn_done`,
`exited`, `test`. `url` opens the session (section 8). On iOS, Web Push only works for a
PWA added to the Home Screen, and permission must be requested from a user gesture.

## 8. Deep links (web client)

- `#pair=<payload>`: pairing.
- `#/h/<host pub>/s/<session id>`: open a session.
- `#/h/<host pub>/files?path=<encoded path>`: open the file browser.
- `#/settings`: settings.
