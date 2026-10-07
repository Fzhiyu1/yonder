# 0001 Approval modes: host default, per-agent memory, switching a running chat

Date: 2026-09-30. Status: accepted.

## Context

Every chat started from the phone asked for approval, even on hosts where the agent itself
is configured to never ask (Codex `approval_policy = "never"` with `danger-full-access`,
Claude `permissions.defaultMode = "bypassPermissions"`). The New Session dialog reset the
mode to 询问 each time and always sent it, so the host configuration never applied, and a
running chat had no way to change its mode.

## Decision

1. **Host default.** The host reports each agent's configured mode as
   `AgentAvailability.default_approval` (Codex: `config/read` `approval_policy` +
   `sandbox_mode`; Claude: `~/.claude/settings.json` `permissions.defaultMode`; `null` when
   the agent has no setting). `create_session` for a chat without `spec.approval` uses it.
   Terminal sessions are untouched: the agent reads its own configuration there.
2. **Per-agent memory on the device.** An explicit pick in the dialog is remembered per agent
   (`settings.approvalByAgent`) and wins over the host default on that device. Without a
   pick and without a host default, the dialog shows 询问.
3. **Switching a running chat.** New request `set_approval_mode{session, mode}`; the session
   carries `approval` (Codex and Claude chats only) and `approval_live`.
   - `approval_live = true`: the supervisor changes the mode in place and reports it back.
   - `approval_live = false` (supervisor from an older release): the host ends the chat and
     starts a new one that resumes the agent's own session in the new mode. The client asks
     first and then moves to the new session id.
4. **What each agent does on a switch.**
   - Codex: the next `turn/start` carries `approvalPolicy` + `sandboxPolicy` (Ask:
     `on-request` + workspace-write, Auto: `never` + workspace-write, Full access: `never` +
     danger-full-access). The turn running at the moment of a switch to full access keeps
     its policy, so yonder answers its command and file-change approvals (pending and new)
     until that turn ends. Permission-profile requests stay with the user.
   - Claude: `set_permission_mode` control request (Ask `default`, Auto `acceptEdits`, Full
     access `bypassPermissions`); chats start with `--allow-dangerously-skip-permissions` so
     a later switch to bypass is allowed. Prompts pending at the switch, and those racing it
     until Claude confirms, are allowed; after the confirmation, whatever Claude still asks
     (explicit `ask` rules, safety checks) goes to the user. Questions always do. A refused
     switch is shown and the previous mode restored.
   - pi: no approval gate of its own (it asks through extension UI requests), so no mode.

## Consequences

- New chats on a host configured for full access run without prompts, matching the CLI.
- A host without a configured mode (Windows-host Codex today) starts in 询问 until the user picks
  a mode once on that device.
- Auto-answering is scoped to what the old policy would have asked in the running turn; it
  never extends to later turns, which run under the agent's own new policy.
- Relay sees nothing new: the mode travels inside the encrypted channel like any request.

## Verification

Unit tests in `yonder-agents` (switch mid-turn, auto-accept scope, refused switch), host
e2e with a fake Codex (`crates/yonder-host/tests/fixtures/fake-codex.mjs`), live tests
against real CLIs (`live_codex_switch_mode`, `live_claude_switch_mode`), the web e2e steps
`chat-approval-mode` and `chat-approval-mode-remembered`, and a real-fleet run over the
public relay.
