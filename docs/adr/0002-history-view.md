# 0002 History view: every agent session on a host, searchable from the phone

Date: 2026-10-05. Status: accepted.

## Context

The only way to reach earlier conversations was the collapsed "恢复历史会话" list in the New
Session dialog: one agent at a time, at most 50 entries, exact-folder match, no search. On top
of that, Codex `thread/list` without `modelProviders` only returns threads of the provider that
is configured right now. On the Mac that hid 143 of 437 interactive threads (recorded under
other providers), and on the Linux host (Codex 0.142) it returned nothing at all. Codex Desktop
threads (source `vscode`, originator "Codex Desktop") were therefore mostly invisible.
Hosts also hold hundreds of sessions (Mac: about 400 Codex, 940 pi files), many of them
test runs in temporary folders.

## Decision

1. **Host lists everything, filters itself.** `agent_history` takes an optional agent (absent:
   Codex + Claude + pi merged), `cwd`, `query`, `cursor`, `limit`, `all`. The host reads every
   session the agent knows about (Codex: `modelProviders: []`, explicit source kinds, state DB
   only, paged; Claude / pi: their session files), caches it for 20 s, and sorts, filters and
   pages it itself. Search behaves the same on every agent version (Codex 0.72 ignores
   `searchTerm`).
2. **Noise hidden by default.** Sub-agent threads are never listed. Unless `all`, the list leaves
   out `codex exec` runs, empty sessions, sessions in temporary folders and automated acceptance
   prompts. The eye toggle shows them.
3. **Folder facets.** The first page carries the folders of the listed sessions with counts,
   most recent first; the client shows them as chips.
4. **Read-only preview.** `agent_preview{agent, id}` returns the last user messages and final
   agent replies from the agent's own files (Codex rollout, Claude project file, pi session
   file) without starting the agent, so a phone can look before it resumes. Long turns read
   further back until enough messages are found.
5. **Continue = resume.** "继续对话" creates a chat that resumes the agent's own session id in its
   own folder, with the same approval rule as a new chat. If a yonder chat already runs that
   session, the button opens it instead.
6. **Web.** A history page per host (`#/h/<host>/history`, sidebar entry 历史): sticky search,
   agent segmented control, folder chips, day groups (今天 / 昨天 / 本周 / 近 30 天 / 更早),
   infinite scroll, preview sheet. The New Session dialog keeps its resume list on top of the
   same request.

## Consequences

- All of the Mac's Desktop threads are reachable from the phone (34 of the first 40 rows are
  Desktop threads); the Linux host goes from 0 to its 16 real threads.
- The relay sees nothing new: requests and previews travel inside the encrypted channel.
- Resuming a thread recorded under another model provider runs it with the host's current
  provider (Codex decides; not verified for every provider combination).
- The same thread open in Codex Desktop and in a yonder chat at once is still unsupported
  (open item in requirements.md).

## Verification

Unit tests in `yonder-agents` (filter / page / facets, temp folders, Codex thread mapping,
preview parsing for all three agents), `live_history_view` against the real Mac data, the mock
UI walk-through in WebKit (iPhone 15, light and dark: list, search, folder chip, agent filter,
infinite scroll, preview, continue; 44 px targets, no overflow), `pnpm e2e` (72/72), the UX
probe, and a real-fleet run through the public relay on mac, the Linux host and the Windows host (10/10).
