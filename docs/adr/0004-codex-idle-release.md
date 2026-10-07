# 0004 Idle Codex chats release their thread

Date: 2026-10-06. Status: accepted.

## Context

Codex allows one writer per thread: the app-server that resumed a thread holds
`~/.codex/thread-writer-locks/<thread>.lock` until it exits. A Yonder chat keeps its
`codex app-server` running for the life of the chat, so after continuing a desktop thread from
the phone, the desktop app showed "已在另一个应用中打开" and could not continue the thread
until the Yonder chat was ended. `thread/unsubscribe` does not release the lock (measured: the
app-server keeps the file open after `unsubscribed`).

## Decision

- The adapter driver supports releasing an idle agent process: when nothing is running or
  waiting (no turn, approval, request in flight or queued message) for a while, the process is
  stopped. The chat itself stays open; the supervisor and its state are unaffected.
- Codex uses it with 90 s (`YONDER_CODEX_IDLE_RELEASE_SECS`, `0` disables). The next message
  starts `codex app-server` again and resumes the same thread quietly (no history replay, the
  model and approval mode picked meanwhile apply). Commands that do not need the agent
  (model, approval mode) only update state while released.
- If the desktop app took the thread in the meantime, the resume fails with "active writer"
  and the message continues in a fork, with a note in the chat (same as opening a busy thread).
- Claude Code and pi keep their processes: they do not lock sessions this way.

## Consequences

- The first message after a pause waits for Codex to start (a few seconds, MCP servers
  included).
- Desktop and phone can take turns on one thread, as long as they do not write at the same
  time.
