# 0006 Sub-agents: one card per agent, a read-only thread, approvals attributed

Date: 2026-10-08. Status: accepted.

## Context

Codex and Claude Code can hand work to sub-agents. The phone showed none of it:

- Codex sends `collabAgentToolCall` items (`spawnAgent`, `wait`, `sendInput`, `closeAgent`,
  `resumeAgent`, ...) that `map_item` dropped. Each sub-agent runs in its own thread on the same
  `app-server` connection, so its notifications (turns, items, deltas) arrive on the parent's
  connection with the child's `threadId`. Until PR #7 its `turn/started` replaced the parent's
  turn id (Stop failed) and its `turn/completed` marked the parent idle; its items were mixed
  into the chat.
- Claude Code's Task tool (named `Agent` since 2.1) showed as one tool row; every message of the
  sub-agent (non-null `parent_tool_use_id`) was dropped.

Approvals were measured, not assumed (recorded runs, codex-cli 0.154 and Claude Code 2.1.63,
a sub-agent asked to `touch` a file outside the workspace):

- Codex sends `item/commandExecution/requestApproval` for the sub-agent on the parent's
  connection, `threadId` = the child's thread. The old adapter did raise it and could answer it,
  but without saying which agent asked, and the child's `turn/completed` that followed set the
  parent idle while it was still waiting on the sub-agent.
- Claude sends `can_use_tool` on the same control channel with `agent_id` and the sub-agent's
  `tool_use_id`. The old adapter raised and answered it, but the Bash call it belonged to was
  invisible (dropped with the rest of the sub-agent's messages).

So approvals were reachable, not lost; what was missing is attribution, visibility of the
sub-agent's work, and a parent status that does not lie.

## Decision

1. **One event model.** A sub-agent is a chat item of kind `subagent` with `ChatItem.subagent`
   (`id` = its thread, `name`, `role`, `model`, `status` running / done / failed / interrupted /
   closed, final `reply`) and `text` = its task. Codex: one card per `spawnAgent` call; every
   other collab call only updates the cards of its targets (`agentsStates`, `closeAgent` ->
   closed). The nickname and role are not in the collab item: the adapter asks
   `thread/read` once per spawned thread. Claude: one card per Task / Agent tool call (card id
   = thread id = tool-use id); the tool result sets done / failed and the reply, a background
   task stays running until `task_notification`.
2. **Sub-agent items live in their thread.** Items and deltas of a sub-agent carry
   `thread` (= `Subagent.id`). The chat log keeps them apart from the chat's own items (capped
   per thread and per chat); snapshots and `chat_older` never contain them, and clients drop
   any they see in the chat. Live they stream as ordinary `chat_item` / `chat_delta` events with
   `thread` set, so the per-session seq stays gap free.
3. **Read-only view.** New request `chat_thread{session, thread, before?, limit?}` returns a
   page of one thread (oldest first, `more`, and the `seq` it reflects). The supervisor of a
   running chat answers it (it holds every thread); for an ended chat the daemon reads the log.
   The daemon's own copy of a chat keeps no threads. The client opens the view by tapping the
   card (sheet on phones, side panel on desktop), applies live events with a seq above the
   page's, and offers no input. Codex resumes fetch the threads of the last few sub-agents with
   `thread/turns/list`; a card left running by an earlier app-server becomes interrupted.
4. **Approvals carry who asked.** `Approval.thread` / `thread_name` name the sub-agent.
   Approvals stay in the chat's single list (one place to answer, notifications unchanged apart
   from naming the sub-agent); the sub-agent's view shows its own pending ones too.
5. **Parent status.** Sub-agent turns never touch the parent's turn or status (PR #7). Codex
   keeps the app-server while any sub-agent runs (no idle release: it would kill them).
   Answering a sub-agent approval after the parent's turn ended leaves the chat idle.
6. **Sidebar and history.** Sub-agents are never sessions; history keeps hiding Codex threads
   with a parent and Claude sidechains.

## Consequences

- The phone sees what sub-agents do and who is asking, at the cost of more events per chat
  (sub-agent reasoning and output now reach the supervisor log and attached clients).
- `chat_thread` against a host without it fails with "unknown variant"; the client says the
  host is too old. Old supervisors close the connection on the new IPC request; the daemon then
  falls back to the log.
- Claude does not stream the sub-agent's final message; it arrives with the Task result and is
  added to the thread then.
- The relay sees nothing new: cards, threads and approvals travel inside the encrypted channel.

## Verification

Adapter tests replay real recordings (`codex_subagent_approval.jsonl`,
`codex_subagent_read.jsonl`, `claude_subagent_approval.jsonl`, sanitized); chat log tests for
thread pages and caps; host e2e through the relay with a fake Codex that spawns a sub-agent
(card, `chat_thread`, approval, live deltas, log fallback); web unit tests; the UX probe's
sub-agent flows in mock mode; `pnpm e2e` step `chat-subagent`; and real runs of codex-cli 0.154
and Claude Code 2.1.63 through a temporary daemon and relay with WebKit (iPhone 15): card, live
status, read-only thread, sub-agent approval answered from the phone, file created.
