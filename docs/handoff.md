# Handoff (2026-09-30)

## State

Everything in the v1 scope is built, deployed and verified end to end on the real fleet.

Deployed (all on the public relay `wss://relay.example.com/v1/ws`, web client at
`https://relay.example.com/`, see docs/deploy.md):

| Host | Binary | Service | Agents |
| --- | --- | --- | --- |
| the relay host | `/opt/yonder-relay/yonder-relay` + web in `/opt/yonder-relay/web` | systemd `yonder-relay` behind nginx :2097 | - |
| Mac (`mac`) | `~/.local/bin/yonder` | launchd `dev.yonder.daemon` | codex, claude, pi |
| the Linux host (`the Linux host`) | `~/.local/bin/yonder` (musl) | systemd --user `yonder` (linger on) | codex, claude |
| Windows host (`win`) | `%LOCALAPPDATA%\yonder\bin\yonder.exe` | scheduled task `yonder` (logon, by SID) | codex, claude |

## Verified (2026-09-28)

Real browser (Playwright bundled Chromium desktop, and WebKit with the iPhone 15 profile)
through the public relay against the three real daemons, 16/16 steps per engine:
pair by link, create a shell terminal (type, check output and cwd), take over a session
started with `yonder run -d` on the host, end and delete it from the UI (gone from
`yonder ls`), create a real Codex chat in ask mode that must write outside the workspace
(approval card, approve, file appears on the host, agent replies), upload 400 KB into the
work folder (SHA-256 checked on the host), download it back (content compared; on iOS
through the share sheet), settings. Script: `(local script)`.

The same UI flow also ran in a browser on the hosts themselves: Linux Chromium on
the Linux host and Windows Chromium on the Windows host (5/5 each, local daemon through the public relay).

`web/scripts/e2e-real.mjs` (`pnpm e2e`) is the self-contained version for development: it
starts a temporary relay, a daemon and a fake pi agent and drives the built web client
(terminal reattach after reload, chat reply, approval, interrupt, attachment, file
mkdir/rename/upload/download/delete, takeover, delete). 13/13 in Chromium and WebKit,
desktop and phone.

Also: `cargo test --workspace` (incl. the e2e through a relay), live agent tests
(`cargo test -p yonder-agents --test live -- --ignored`: codex approval, claude approval,
claude/pi pong, history), `pnpm test` (23), clippy clean.

## Not verified yet / open

- Scanning the QR code on the user's real iPhone and Web Push on the device (needs the
  PWA added to the Home Screen). WebKit emulation is not a real iPhone.
  iOS keeps Home Screen web app storage apart from Safari and pairing codes are single-use,
  so pair from inside the Home Screen app (the pairing sheet says so in Safari on iOS).
- Claude Code chat on the Windows host: the configured endpoint (a third-party endpoint) is unreachable from
  that machine and returns 403 through its local proxy; claude works on the Mac and in
  the terminal elsewhere. Not a yonder issue; fix the Claude config there if needed.
- the Linux host Claude Code 2.1.63 hangs on `claude -p` from a non-interactive shell (90 s
  timeout); Codex chat there works.
- Native iOS shell: waits for Xcode (needs the user's sudo for `mas install 497799835`).
- Windows-host Codex had no config; `~/.codex/config.toml` there was created with the maintainer's
  provider (model gpt-5.5) so Codex chats work.

## Gotchas found during acceptance (fixed)

- Codex default model must come from the user's config (`config/read`), not the catalog.
- A Codex turn can block for many minutes on a hung MCP server (figma via npx); the chat
  now shows "等待 MCP 服务启动：…" and failed servers.
- Windows: `schtasks /RU WORKGROUP\user` fails for local accounts; the task is created
  from XML with the user's SID. A running `yonder.exe` cannot be overwritten while session
  supervisors use it: rename it first (see docs/deploy.md).
- launchd: `bootout` returns before the job is gone; `service install` waits for it.
- Deleting a session retries removing its directory (Windows holds files briefly after the
  supervisor exits) and reports an error if it survives, instead of letting the session
  reappear at the next daemon start.
- MCP servers that fail to start are reported once per chat, in one message, after all of
  them have settled.
- Codex history/model requests that are optional (like `model/list`) no longer delay the
  required ones: they get a short grace period.
- Web Push to iPhones: Apple's push service answers 403 BadJwtToken when the VAPID `sub`
  claim is a placeholder (`mailto:x@localhost`, `.local`/`.test`/`.invalid`, dotless hosts),
  which the host used to send. The contact is now the web client URL when it is a public
  https URL, else the project URL, or `notify.vapid_subject` when set. Live check:
  `cargo test -p yonder-notify --test apple_live -- --ignored`.
- iOS only opens the share sheet within about 5 s of a tap; a download that takes longer
  now asks for one more tap ("下载完成 · 保存") instead of failing silently.
- File manager: an upload started right after opening a folder went to the previous folder
  (the route changes before React re-renders, and the old listing was still on screen). The
  upload now reads the folder from the route at call time, the old listing is dimmed and
  inert until the new one arrives, and e2e covers it (`files-upload-after-navigation`).
- Focus reporting also fires on `pagehide` / `freeze`, so a host keeps notifying about a
  session whose page iOS suspended without a visibility change. e2e step
  `chat-resume-after-network-loss` freezes the relay (SIGSTOP), expects the connection banner
  within the ping timeout, and checks the chat works again after it recovers (WebKit's
  `setOffline` does not cut open WebSockets, so it cannot simulate this).
- Pairing codes are kept in `pairing.json` (config dir, 0600) until used or expired, so a QR
  code shown before a daemon restart or upgrade still works. The terminal QR pins its colors
  (white on black) so it also scans on light terminal themes.

## 2026-09-30: approval modes and phone UX

- Approval modes (docs/adr/0001-approval-modes.md): a new chat follows the agent's own
  configuration on the host (Mac and the Linux host Codex: full access; Windows-host Codex has no
  `approval_policy`, so 询问 until picked once), an explicit pick is remembered per agent on
  the device, and a running Codex/Claude chat switches from the composer chip
  (`set_approval_mode`). Chats run by an older supervisor restart and resume in the new mode.
- Codex 0.72 (Windows host) has no `turn/steer`: a message sent during a turn used to fail with
  "unknown variant" and was lost. It is now held and sent as the next turn ("消息将在当前回合结束后发送").
- WebKit drops the click of a touch tap when `pointerdown` is `preventDefault`ed. The send
  button and the terminal key bar did that (to keep the keyboard up), so on iPhone tapping
  send or a key bar key did nothing. They now cancel `mousedown` instead.
- Phone UX pass: 44 px touch targets, the app sized to the visual viewport (keyboard aware:
  `web/src/lib/viewport.ts`), sheets animate and close by dragging the handle, menus flip up
  above the keyboard, the new-session dialog puts approval and the first message first with
  one full-width create button, the composer drops the keyboard after sending on phones.

Probe: `cd web && pnpm build && node scripts/ux-probe.mjs <outDir>` walks the phone flows of
the mock UI in WebKit (iPhone 15) with a simulated keyboard: tap counts, touch targets under
44 px, clipped controls, tiny text, keyboard occlusion, and whether taps on keep-focus buttons
still click. It prints a JSON report and exits 1 on a failed check.

Real-fleet run (public relay, WebKit iPhone 15, 19/19): `(local script) mac the Linux host win`
(new chat follows the host, switch to 询问 then full access while the card waits, next turn
does not ask, message during a turn). `restart-path.mjs` there covers the restart of a chat
whose supervisor predates `set_approval_mode` (needs `OLD_BIN`).

## 2026-10-05: history view

- Problem: Codex Desktop conversations were invisible. `thread/list` without `modelProviders`
  only returns the configured provider's threads (Mac: 143 of 437 hidden; the Linux host: 0), and
  the only history UI was the collapsed resume list in New Session (50 entries, one agent).
- Now (docs/adr/0002-history-view.md): 历史 per host in the sidebar. Search, agent filter, folder
  chips, day groups, infinite scroll, a read-only preview of the last messages, and 继续对话
  (resume in a yonder chat, or open the running one). Test runs, `exec` and temp folders are
  hidden behind the eye toggle.
- Deployed on mac, the Linux host, the Windows host and the web build on the relay host. Real-fleet run:
  `(local script) mac the Linux host win` (10/10). Mock phone
  walk-through: `history-probe.mjs` there.
- Not verified: the real iPhone; resuming a thread recorded under another provider; a thread
  open in Desktop and yonder at the same time.
