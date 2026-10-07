# yonder-pty

PTY sessions owned by detached supervisor processes (macOS, Linux, Windows ConPTY).

## API

- `spawn_supervisor(program, argv_prefix, &SupervisorArgs) -> pid`: start a detached supervisor
  (`program argv_prefix... <base64 args>`). Unix: `setsid`. Windows: `DETACHED_PROCESS |
  CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB` (retried without
  breakaway). stdout/stderr go to `<dir>/supervisor.log`.
- `supervisor_main(SupervisorArgs)`: the supervisor entry point (call from your binary's hidden
  subcommand after `SupervisorArgs::decode`).
- `SupervisorClient::connect(dir, id)` / `connect_retry`: talk to a supervisor. `info`, `input`,
  `resize`, `kill`, `subscribe(from_offset)`, `send(SupRequest)`, `recv() -> SupEvent`,
  `into_split()` for a reader task + writer.
- `supervisor_alive(dir, id)`, `read_exit(dir)`, `list_session_dirs(root)`, `resolve_command`.

## Files in a session dir

- `pty.log` raw output; `pty.base` global offset of its first byte (rotation at 8 MiB keeps 4 MiB).
- `exit.json` `{code, signal, ended_at}` once the child exited.
- `sup.sock` (Unix) supervisor socket; dir is chmod 0700. Windows uses the named pipe
  `\\.\pipe\yonder-sup-<id>` with DACL owner+SYSTEM only.
- `supervisor.log` supervisor diagnostics.

## IPC

Frames: `u32` big-endian length + JSON. Requests (`t`): `hello`, `input{data b64}`,
`resize{cols,rows}`, `kill`, `snapshot`, `subscribe{from_offset}`, `shutdown`.
Events (`t`): `info{info}`, `output{offset,data}`, `snapshot{data,offset,cols,rows}`,
`resized`, `exited{exit}`, `title{title}`, `error{message}`.
`subscribe` replays `[from_offset, end)` from the log when retained and <= 2 MiB, otherwise
sends a rendered snapshot (vt100) first; then live output without gaps or duplicates.

## Behavior notes

- DSR cursor-position requests (`ESC[6n`) are answered by the supervisor from its vt100 screen.
  ConPTY emits one at startup and stalls output until answered.
- Kill: Unix SIGHUP+SIGTERM to the process group, SIGKILL after 3 s. Windows: the child is
  placed in a Job Object with KILL_ON_JOB_CLOSE and the job is terminated.
- After the child exits the supervisor keeps serving for `linger_secs`, then exits.

## Verified

`cargo test -p yonder-pty` passes on macOS (arm64), Linux x86_64 (musl build on Ubuntu 24.04)
and Windows 11 build 26200 (x86_64-pc-windows-gnu build, ConPTY).
