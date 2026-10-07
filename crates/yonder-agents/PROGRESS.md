# yonder-agents progress log

Owner of this crate: the chat-adapter worker. Only `crates/yonder-agents/**` is edited here.

## Done

- Read docs/architecture.md, docs/requirements.md, crates/yonder-proto/src/app.rs.
- Crate skeleton on disk (Cargo.toml, src/lib.rs with the public API shape).
- Codex app-server: recorded a real PONG turn and a real approval round trip (scratch
  recordings in ~/run/tmp/20260927-yonder-agents/rec, to be copied into tests/fixtures).

## Next

1. Record Claude Code stream-json (plain turn, can_use_tool approval, interrupt, resume).
2. Record pi RPC (plain turn, tool call, abort, resume).
3. Implement: process spawning (login shell, Windows .cmd shims), JSONL framing, the three
   adapters, history listing, detection, terminal argv.
4. Unit tests over recorded fixtures; ignored live integration tests; clippy; windows-gnu check.

## Findings

### Workspace

- `members = ["crates/*"]`: while another worker's crate dir has no Cargo.toml yet
  (seen: crates/yonder-pty at 14:23), every cargo command in the workspace fails.

### Codex CLI 0.142.5 (`codex app-server`)

- JSON-RPC 2.0, one JSON object per line on stdio. `initialize` {clientInfo:{name,title,version},
  capabilities:null} -> response; then `initialized` notification.
- `thread/start` {cwd, approvalPolicy, sandbox, model?} -> result.thread.id (also `model`,
  `approvalPolicy`, `sandbox` echo). `thread/resume` {threadId, ...same overrides}.
- `turn/start` {threadId, input:[{type:"text", text, text_elements:[]}]} -> result.turn.id,
  then `turn/started`, `item/started`/`item/completed` per item, deltas
  (`item/agentMessage/delta`, `item/reasoning/summaryTextDelta`, ...), `turn/completed`
  with turn.status completed|interrupted|failed.
- Item ids come from the model provider (e.g. `msg_...`, `rs_...`, `toolu_...`) and are
  stable between item/started, deltas and item/completed.
- User hooks from ~/.codex/hooks.json fire (`hook/started`/`hook/completed`); ignore.
- Approval: with approvalPolicy `on-request` + sandbox `workspace-write`, `touch x` inside
  cwd runs WITHOUT approval (sandbox allows it; $TMPDIR and /tmp are writable roots too).
  Writing outside the writable roots makes the model request escalation:
  server request `item/commandExecution/requestApproval` (id is a number, params.itemId =
  command item id, params.command, params.cwd, params.reason), preceded by
  `thread/status/changed` activeFlags ["waitingOnApproval"]. Reply
  `{"id":<same>,"result":{"decision":"accept"}}`; server then sends
  `serverRequest/resolved` {requestId} and the command runs. Verified live: file created.
