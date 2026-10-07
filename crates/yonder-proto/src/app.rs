//! Application protocol carried inside the encrypted channel.
//!
//! The same JSON messages are also spoken over the host's local control socket by the
//! `yonder` CLI (newline-delimited JSON, trusted by OS permissions, no Noise).
//!
//! Client -> host: [`ClientMsg`]. Host -> client: [`HostMsg`].
//! Every `req` gets exactly one `res` with the same id. Events are pushed:
//! `session_updated` / `session_removed` go to every connected client; stream events
//! (`pty_*`, `chat_*`, `approval_*`) only to clients attached to that session.
//!
//! Binary payloads (terminal bytes, file chunks) are standard base64 strings.
//! Timestamps are unix milliseconds.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

macro_rules! ts_export {
    ($($item:item)*) => {
        $(
            #[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, optional_fields))]
            $item
        )*
    };
}

ts_export! {
    /// Client (web, iOS, CLI) -> host.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "t", rename_all = "snake_case")]
    #[cfg_attr(feature = "ts", ts(rename = "AppClientMsg"))]
    pub enum ClientMsg {
        Req { id: u64, req: Request },
        /// Keystrokes / paste for an attached terminal session (base64). No response.
        Input { session: String, data: String },
        /// Viewport size of this client for a terminal session. Last active client wins.
        Resize { session: String, cols: u16, rows: u16 },
        /// The session this client is actively showing (visible and focused), or none.
        /// The host skips notifications for sessions someone is looking at.
        Focus {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            session: Option<String>,
        },
    }

    /// Host -> client.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "t", rename_all = "snake_case")]
    #[cfg_attr(feature = "ts", ts(rename = "AppHostMsg"))]
    pub enum HostMsg {
        Res {
            id: u64,
            ok: bool,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            data: Option<Response>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            error: Option<ApiError>,
        },
        Event { event: Event },
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum Request {
        Ping,
        HostInfo,
        ListSessions,
        CreateSession { spec: SessionSpec },
        /// Start receiving stream events for a session. `since` is the pty byte offset or
        /// chat seq the client already has (the host may still send a full snapshot).
        Attach {
            session: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            since: Option<u64>,
        },
        Detach { session: String },
        /// Items of one sub-agent's thread (read-only view), oldest first: the newest page, or
        /// the page right before `before`. Live items of the thread arrive as `chat_item` /
        /// `chat_delta` with `thread` set; apply those with a seq above the answer's `seq`.
        ChatThread {
            session: String,
            /// `Subagent::id` of the card.
            thread: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            before: Option<String>,
            /// Page size (default 200, max 500).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            limit: Option<u32>,
        },
        /// Chat items right before `before` (an item id the client has), oldest first. The
        /// `attached` snapshot carries only the newest page; scrolling up fetches more.
        ChatOlder {
            session: String,
            before: String,
            /// Page size (default 40, max 200).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            limit: Option<u32>,
        },
        /// Terminate the session's process tree. The session stays listed as exited.
        Kill { session: String },
        /// Forget an exited session and delete its local log.
        Remove { session: String },
        Rename { session: String, title: String },
        /// Re-open a terminal agent session as a chat session using the agent's own
        /// resume support. The terminal session is killed. Responds with the new session.
        ContinueAsChat { session: String },
        /// Send a user message. If the agent is busy it is queued/steered by the adapter.
        /// `attachments` are absolute host paths (e.g. from `upload_temp`).
        ChatSend {
            session: String,
            text: String,
            #[serde(default)]
            attachments: Vec<String>,
        },
        ChatInterrupt { session: String },
        ApprovalRespond { session: String, approval: String, option: String },
        /// Switch the model of a running chat. Applies from the next turn (Codex), or right
        /// away (Claude, pi). The session's `model` follows once the agent confirms.
        SetChatModel { session: String, model: String },
        /// GET `url` from a web server on the host's own loopback interface (an agent's dev
        /// server) for in-app previews. Only `http://localhost`, `127.0.0.1` and `[::1]` are
        /// allowed; redirects are not followed. Bodies over 20 MiB are refused.
        HttpFetch { url: String },
        /// The same loopback `url` (as accepted by `HttpFetch`) rewritten to this host's
        /// Tailscale address, so a device on the tailnet can open it in its own browser.
        /// `reachable` tells whether the port answers on that address (dev servers often
        /// listen on loopback only). `not_found` when the host has no Tailscale address.
        TailnetUrl { url: String },
        /// Change how much a running chat asks before acting. Codex and Claude switch in place
        /// (`SessionInfo::approval_live`); otherwise the host restarts the chat by resuming the
        /// agent's own session with the new mode and responds with the new session.
        SetApprovalMode { session: String, mode: ApprovalMode },
        /// Sessions the agent itself knows about on this host (history view, "resume" pickers),
        /// newest first, one page at a time. `query` matches title, first message and folder
        /// (case-insensitive). Unless `all`, sessions in temporary folders, empty ones and
        /// non-interactive runs (`codex exec`) are left out. Pass the previous `next_cursor` as
        /// `cursor` for the next page.
        AgentHistory {
            /// One agent, or every chat-capable agent on the host when absent.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            agent: Option<AgentKind>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cwd: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            query: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cursor: Option<String>,
            /// Page size (default 50, max 200).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            limit: Option<u32>,
            #[serde(default)]
            all: bool,
        },
        /// The last messages (user and final agent replies) of an agent session, read from the
        /// agent's own files without starting it.
        AgentPreview { agent: AgentKind, id: String },
        FsHome,
        FsList {
            path: String,
            #[serde(default)]
            hidden: bool,
        },
        FsStat { path: String },
        /// Read up to `len` bytes (max 1 MiB) at `offset`.
        FsRead { path: String, offset: u64, len: u32 },
        /// Chunked upload. Chunks are written to a temporary sibling file; `finish`
        /// atomically renames it to `path`. `offset` must equal bytes written so far.
        FsWrite {
            path: String,
            offset: u64,
            data: String,
            finish: bool,
            #[serde(default)]
            overwrite: bool,
        },
        FsMkdir { path: String },
        FsRename {
            from: String,
            to: String,
            #[serde(default)]
            overwrite: bool,
        },
        /// Move to the OS trash (never unlinks).
        FsDelete { path: String },
        /// Store a small file (max 20 MiB) in the host upload dir; responds with its path.
        UploadTemp { name: String, data: String },
        ListDevices,
        RevokeDevice { device: String },
        /// Local control socket only: create a one-time pairing token + QR URL.
        CreatePairing {
            #[serde(default)]
            permissions: Vec<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            ttl_secs: Option<u64>,
        },
        /// Local control socket only: daemon status.
        Status,
        /// Local control socket only: stop the daemon. Sessions keep running.
        Shutdown,
        NotifyTest,
        /// Register a Web Push subscription for this device. A browser can hold only one
        /// subscription per origin, so the device generates one VAPID key pair and gives
        /// every paired host the private scalar (`vapid_private`, base64url 32 bytes) to sign
        /// with. Without it the host signs with its own key (`HostInfo::vapid_public`).
        PushSubscribe {
            endpoint: String,
            p256dh: String,
            auth: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            vapid_private: Option<String>,
        },
        /// Remove this device's Web Push subscriptions.
        PushUnsubscribe,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum Response {
        Pong { ts: u64 },
        HostInfo { info: HostInfo },
        Sessions { sessions: Vec<SessionInfo> },
        Session { session: SessionInfo },
        Attached {
            session: SessionInfo,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            terminal: Option<TerminalSnapshot>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            chat: Option<ChatSnapshot>,
        },
        AgentHistory {
            sessions: Vec<AgentSessionSummary>,
            /// More sessions follow: pass this as `cursor`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            next_cursor: Option<String>,
            /// First page only: folders of the matching sessions (without `cwd` / `query`),
            /// most recently used first.
            #[serde(default)]
            folders: Vec<HistoryFolder>,
            /// Agents whose history could not be read (the others are still listed).
            #[serde(default)]
            errors: Vec<String>,
        },
        ChatOlder {
            /// Oldest first.
            items: Vec<ChatItem>,
            /// Even older items exist.
            more: bool,
        },
        ChatThread {
            /// Oldest first.
            items: Vec<ChatItem>,
            /// Even older items exist.
            more: bool,
            /// Seq of the last event reflected in `items`.
            seq: u64,
        },
        HttpResponse {
            status: u16,
            /// Response headers (lowercase names), e.g. `content-type`.
            headers: Vec<(String, String)>,
            /// Body, base64.
            data: String,
        },
        TailnetUrl { url: String, reachable: bool },
        AgentPreview {
            /// Oldest first; user messages and the last agent message of each turn.
            items: Vec<ChatItem>,
            /// Earlier messages exist.
            truncated: bool,
        },
        Dir { listing: DirListing },
        Stat { entry: FileEntry },
        FileChunk { path: String, offset: u64, data: String, eof: bool, size: u64 },
        Path { path: String },
        Devices { devices: Vec<DeviceInfo> },
        Pairing { payload: crate::pairing::PairPayload, url: String },
        Status { status: HostStatus },
        Ok,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ApiError {
        /// `not_found`, `forbidden`, `invalid`, `exists`, `busy`, `unsupported`, `internal`.
        pub code: String,
        pub message: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "ev", rename_all = "snake_case")]
    pub enum Event {
        /// Session added or changed. Sent to all connected clients.
        SessionUpdated { session: SessionInfo },
        SessionRemoved { session: String },
        /// Terminal output; `offset` is the byte offset of the first byte of `data`.
        PtyOutput { session: String, offset: u64, data: String },
        /// The terminal must be cleared and redrawn from `snapshot` (e.g. after the client
        /// fell behind). Later `pty_output` continues from `snapshot.offset`.
        PtySnapshot { session: String, snapshot: TerminalSnapshot },
        PtyResized { session: String, cols: u16, rows: u16 },
        /// Insert or replace a chat item (by id).
        ChatItem { session: String, seq: u64, item: ChatItem },
        /// Append streaming text to an existing item field. `thread`: the item belongs to that
        /// sub-agent's thread (see `ChatItem::thread`).
        ChatDelta {
            session: String,
            seq: u64,
            item: String,
            field: DeltaField,
            delta: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            thread: Option<String>,
        },
        /// Replace the whole chat view (e.g. after the client fell behind).
        ChatSnapshot { session: String, snapshot: ChatSnapshot },
        ChatStatus {
            session: String,
            seq: u64,
            status: ChatStatus,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            detail: Option<String>,
        },
        ApprovalRequested { session: String, seq: u64, approval: Approval },
        ApprovalResolved { session: String, seq: u64, approval: String, option: String },
        /// Host-level toast for the UI.
        Notice { level: NoticeLevel, message: String },
        /// A new device completed pairing.
        DevicePaired { device: DeviceInfo },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SessionKind { Terminal, Chat }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AgentKind { Codex, Claude, Pi, Shell, Custom }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SessionOrigin {
        /// Created from a remote client (phone/web).
        Remote,
        /// Created on the host via `yonder run`.
        Local,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SessionState { Starting, Running, Exited, Failed }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ChatStatus { Starting, Idle, Working, AwaitingApproval, Error, Exited }

    /// How much the agent should ask before acting (chat sessions).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ApprovalMode {
        /// Ask before commands / edits that the agent considers risky (default).
        #[default]
        Ask,
        /// Agent's automatic mode (e.g. accept edits in workspace, sandboxed commands).
        Auto,
        /// Never ask.
        Yolo,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct SessionSpec {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub kind: Option<SessionKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent: Option<AgentKind>,
        /// Explicit argv. Default: the agent's CLI for terminal sessions, the user's shell
        /// for `shell`. Required for `custom`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub command: Option<Vec<String>>,
        /// Working directory. Default: home.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cols: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub rows: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub env: Option<BTreeMap<String, String>>,
        /// Agent session id to resume (codex thread id, claude session id, pi session id).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resume: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub approval: Option<ApprovalMode>,
        /// Chat only: first user message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub prompt: Option<String>,
        /// Set by the host for `yonder run`; ignored from remote clients.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub origin: Option<SessionOrigin>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SessionInfo {
        pub id: String,
        pub kind: SessionKind,
        pub agent: AgentKind,
        pub title: String,
        pub command: Vec<String>,
        pub cwd: String,
        pub origin: SessionOrigin,
        pub state: SessionState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pid: Option<u32>,
        pub created_at: u64,
        pub updated_at: u64,
        pub cols: u16,
        pub rows: u16,
        /// Number of clients currently attached.
        pub clients: u32,
        /// The agent's own session id (for resume / continue as chat).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub chat_status: Option<ChatStatus>,
        pub pending_approvals: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<String>,
        /// Chat sessions of agents that gate actions (Codex, Claude): the current mode.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub approval: Option<ApprovalMode>,
        /// `set_approval_mode` applies in place (no restart).
        #[serde(default)]
        pub approval_live: bool,
        /// Short text for list views (last agent message, last terminal line).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub preview: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TerminalSnapshot {
        /// Client must clear its terminal before writing `data`.
        pub reset: bool,
        /// Base64 bytes that reproduce the current screen (replay or rendered snapshot).
        pub data: String,
        /// Byte offset right after `data`; live `pty_output` continues from here.
        pub offset: u64,
        pub cols: u16,
        pub rows: u16,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ChatSnapshot {
        pub items: Vec<ChatItem>,
        /// Approvals still waiting for a decision.
        pub approvals: Vec<Approval>,
        pub status: ChatStatus,
        /// Seq of the last event reflected in this snapshot.
        pub seq: u64,
        /// True when older items were omitted.
        pub truncated: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ChatItemKind {
        User,
        Agent,
        Reasoning,
        Plan,
        Command,
        FileChange,
        Tool,
        WebSearch,
        Error,
        System,
        /// A sub-agent spawned by the agent (card; details in `ChatItem::subagent`).
        Subagent,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ItemStatus { InProgress, Completed, Failed, Declined }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum DeltaField { Text, Output }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ChatItem {
        pub id: String,
        pub kind: ChatItemKind,
        pub status: ItemStatus,
        /// Markdown for messages, reasoning, plans.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        /// Command line, tool name, search query, file summary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub title: Option<String>,
        /// Command output or tool result (may be truncated by the host).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub output: Option<String>,
        /// Unified diff for file changes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub diff: Option<String>,
        /// Files touched, or image paths attached to a user message.
        #[serde(default)]
        pub paths: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub duration_ms: Option<u64>,
        pub ts: u64,
        /// The item belongs to the thread of this sub-agent (`Subagent::id`), not to the chat
        /// itself: it is only shown in the sub-agent's read-only view. Snapshots and
        /// `chat_older` carry the chat's own items only; `chat_thread` reads a sub-agent's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub thread: Option<String>,
        /// `kind == subagent`: the sub-agent this card stands for. `text` is its task (prompt).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub subagent: Option<Subagent>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SubagentStatus {
        Running,
        /// Finished its task (the parent may give it more work later).
        Done,
        Failed,
        Interrupted,
        /// Closed by the parent agent.
        Closed,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Subagent {
        /// Id of the sub-agent's thread (Codex thread id, Claude Code Task tool-use id); the
        /// `thread` of its items and approvals. Empty until the agent has been created.
        pub id: String,
        /// Display name: Codex nickname, Claude Code task description.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        /// Agent type / role: Codex agent role, Claude Code `subagent_type`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub role: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<String>,
        pub status: SubagentStatus,
        /// Its final reply, once there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reply: Option<String>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ApprovalKind { Command, FileChange, Tool, Permission, Question }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum OptionKind {
        Allow,
        /// Allow and remember for the rest of the session.
        AllowAlways,
        Deny,
        /// Deny and stop the current turn.
        Abort,
        /// Free choice (questions).
        Choice,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ApprovalOption {
        pub id: String,
        pub label: String,
        pub kind: OptionKind,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Approval {
        pub id: String,
        pub kind: ApprovalKind,
        /// One-line summary for lists and notifications.
        pub title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub diff: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
        /// Extra detail, e.g. pretty-printed tool input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
        pub options: Vec<ApprovalOption>,
        /// Related chat item id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub item: Option<String>,
        pub ts: u64,
        /// Raised inside this sub-agent (`Subagent::id`); answered like any other approval.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub thread: Option<String>,
        /// Display name of that sub-agent (`Subagent::name`), when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub thread_name: Option<String>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum NoticeLevel { Info, Warning, Error }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AgentAvailability {
        pub agent: AgentKind,
        pub available: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub path: Option<String>,
        /// Chat (structured) mode supported on this host.
        pub chat: bool,
        /// Best-effort model ids for the model picker (may be empty).
        #[serde(default)]
        pub models: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub default_model: Option<String>,
        /// The mode matching the agent's own configuration on this host (e.g. Codex
        /// `approval_policy = "never"` + full access, Claude `defaultMode = bypassPermissions`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub default_approval: Option<ApprovalMode>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct HostInfo {
        /// Display name chosen at setup.
        pub name: String,
        pub hostname: String,
        /// `macos`, `linux`, `windows`.
        pub os: String,
        pub arch: String,
        pub version: String,
        pub home: String,
        pub shell: String,
        pub agents: Vec<AgentAvailability>,
        pub recent_dirs: Vec<String>,
        /// Permissions of the requesting device: `sessions`, `files`.
        pub permissions: Vec<String>,
        /// Roots the file manager may access.
        pub fs_roots: Vec<String>,
        pub path_sep: String,
        /// Host key fingerprint, for display.
        pub fingerprint: String,
        /// VAPID public key (base64url) when Web Push is enabled on this host.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub vapid_public: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct HostStatus {
        pub name: String,
        pub host_pub: String,
        pub fingerprint: String,
        pub version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub relay_url: Option<String>,
        pub relay_connected: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub relay_error: Option<String>,
        /// Encrypted client links currently open.
        pub clients: u32,
        pub sessions: u32,
        pub uptime_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub web_url: Option<String>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum FileKind { File, Dir, Symlink, Other }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct FileEntry {
        pub name: String,
        pub path: String,
        pub kind: FileKind,
        pub size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mtime: Option<u64>,
        pub readonly: bool,
        pub hidden: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DirListing {
        pub path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub parent: Option<String>,
        pub entries: Vec<FileEntry>,
        pub truncated: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DeviceInfo {
        /// Device public key (base64url).
        pub public: String,
        pub name: String,
        pub client: String,
        pub paired_at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub last_seen: Option<u64>,
        pub permissions: Vec<String>,
        /// True for the device making the request.
        pub current: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct HistoryFolder {
        pub path: String,
        pub count: u32,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AgentSessionSummary {
        pub id: String,
        pub agent: AgentKind,
        pub title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub updated_at: Option<u64>,
        /// First user message when it differs from the title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub preview: Option<String>,
        /// Where the session was started: `desktop`, `ide`, `cli`, `yonder`, `sdk`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<String>,
        /// The session's files changed within the last minute: probably mid-turn in another
        /// program (desktop app, terminal). Continuing it from here starts a copy.
        #[serde(default)]
        pub active: bool,
    }
}

pub const PERM_SESSIONS: &str = "sessions";
pub const PERM_FILES: &str = "files";

/// Max bytes per `fs_read`.
pub const FS_READ_MAX: u32 = 1024 * 1024;
/// Max bytes for `upload_temp`.
pub const UPLOAD_TEMP_MAX: usize = 20 * 1024 * 1024;

impl ApiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
    pub fn not_found(m: impl Into<String>) -> Self { Self::new("not_found", m) }
    pub fn forbidden(m: impl Into<String>) -> Self { Self::new("forbidden", m) }
    pub fn invalid(m: impl Into<String>) -> Self { Self::new("invalid", m) }
    pub fn exists(m: impl Into<String>) -> Self { Self::new("exists", m) }
    pub fn busy(m: impl Into<String>) -> Self { Self::new("busy", m) }
    pub fn unsupported(m: impl Into<String>) -> Self { Self::new("unsupported", m) }
    pub fn internal(m: impl Into<String>) -> Self { Self::new("internal", m) }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl HostMsg {
    pub fn ok(id: u64, data: Response) -> Self {
        HostMsg::Res { id, ok: true, data: Some(data), error: None }
    }
    pub fn err(id: u64, error: ApiError) -> Self {
        HostMsg::Res { id, ok: false, data: None, error: Some(error) }
    }
    pub fn event(event: Event) -> Self {
        HostMsg::Event { event }
    }
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Codex => "codex",
            AgentKind::Claude => "claude",
            AgentKind::Pi => "pi",
            AgentKind::Shell => "shell",
            AgentKind::Custom => "custom",
        }
    }

    /// The agent gates commands / edits behind approvals, so an approval mode applies.
    /// (pi runs tools without asking; its dialogs are questions, not approvals.)
    pub fn has_approvals(self) -> bool {
        matches!(self, AgentKind::Codex | AgentKind::Claude)
    }
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMode::Ask => "ask",
            ApprovalMode::Auto => "auto",
            ApprovalMode::Yolo => "yolo",
        }
    }
}

impl ChatItem {
    pub fn new(id: impl Into<String>, kind: ChatItemKind, status: ItemStatus, ts: u64) -> Self {
        Self {
            id: id.into(),
            kind,
            status,
            text: None,
            title: None,
            output: None,
            diff: None,
            paths: Vec::new(),
            exit_code: None,
            duration_ms: None,
            ts,
            thread: None,
            subagent: None,
        }
    }
}

/// Current unix time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes() {
        let m = ClientMsg::Req { id: 3, req: Request::FsList { path: "/tmp".into(), hidden: false } };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"t":"req","id":3,"req":{"op":"fs_list","path":"/tmp","hidden":false}}"#);

        let m: ClientMsg = serde_json::from_str(r#"{"t":"req","id":1,"req":{"op":"list_sessions"}}"#).unwrap();
        assert!(matches!(m, ClientMsg::Req { id: 1, req: Request::ListSessions }));

        let r = HostMsg::ok(1, Response::Ok);
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"t":"res","id":1,"ok":true,"data":{"kind":"ok"}}"#);

        let e = HostMsg::err(2, ApiError::not_found("x"));
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"t":"res","id":2,"ok":false,"error":{"code":"not_found","message":"x"}}"#
        );

        let ev = HostMsg::event(Event::PtyOutput { session: "s1".into(), offset: 0, data: "aGk=".into() });
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"t":"event","event":{"ev":"pty_output","session":"s1","offset":0,"data":"aGk="}}"#
        );

        let spec: SessionSpec = serde_json::from_str(r#"{"kind":"chat","agent":"codex","cwd":"/x"}"#).unwrap();
        assert_eq!(spec.agent, Some(AgentKind::Codex));
        assert_eq!(spec.kind, Some(SessionKind::Chat));
    }
}
