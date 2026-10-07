//! Codex adapter: `codex app-server` (JSON-RPC 2.0, one JSON object per line).

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Value};
use yonder_proto::app::{
    AgentKind, Approval, ApprovalKind, ApprovalMode, ApprovalOption, ChatItem, ChatItemKind, ChatStatus, DeltaField,
    ItemStatus, OptionKind,
};

use crate::common::{item, now_ms, random_id, truncate_tail, MAX_OUTPUT};
use crate::driver::{run, Out, Protocol};
use crate::{AdapterCmd, AdapterEvent, AdapterHandle, AgentLaunch};

pub fn spawn(launch: AgentLaunch) -> Result<AdapterHandle> {
    let mut argv = launch.program.clone().unwrap_or_else(|| vec!["codex".into()]);
    argv.push("app-server".into());
    let st = CodexState::new(launch.clone());
    run(AgentKind::Codex, &launch, argv, st)
}

impl Protocol for CodexState {
    fn start(&mut self) -> Out {
        let mut out = Out::default();
        let init = self.request(
            "initialize",
            json!({"clientInfo": {"name": "yonder", "title": "yonder", "version": env!("CARGO_PKG_VERSION")}, "capabilities": {"experimentalApi": true}}),
        );
        out.send(init);
        out
    }
    fn on_message(&mut self, v: &Value) -> Out {
        CodexState::on_message(self, v)
    }
    fn on_cmd(&mut self, cmd: AdapterCmd) -> Out {
        CodexState::on_cmd(self, cmd)
    }
    fn on_exit(&mut self) -> Out {
        let mut out = Out::default();
        for (_, mut it) in self.items.drain() {
            if it.status == ItemStatus::InProgress {
                it.status = ItemStatus::Failed;
                out.event(AdapterEvent::Item(it));
            }
        }
        for id in self.approvals.keys().cloned().collect::<Vec<_>>() {
            out.event(AdapterEvent::ApprovalResolved { approval: id, option: "cancelled".into() });
        }
        self.approvals.clear();
        out
    }
    fn exit_error(&self) -> Option<String> {
        self.exit_error.clone()
    }
    fn idle_release(&self) -> Option<std::time::Duration> {
        release_after()
    }
    fn can_release(&self) -> bool {
        // Only a thread that exists and has nothing going on: no turn, approval, request in
        // flight or message waiting.
        self.thread.is_some() && !self.busy && self.approvals.is_empty() && self.pending.is_empty() && self.queued.is_empty() && self.after_turn.is_empty()
    }
    fn on_release(&mut self) -> Out {
        // The app-server is gone: everything it knew is reset; the thread id stays for resuming.
        let thread = self.thread.take();
        self.released = thread;
        self.turn = None;
        self.pending.clear();
        self.items.clear();
        self.mcp_starting.clear();
        // The next app-server starts from the mode/model in effect now.
        self.mode_dirty = false;
        if let Some(m) = self.turn_model.take() {
            self.launch.model = Some(m);
        }
        Out::default()
    }
    fn needs_agent(&self, cmd: &AdapterCmd) -> bool {
        matches!(cmd, AdapterCmd::Send { .. })
    }
    fn restart(&mut self) -> Out {
        self.restarting = true;
        let mut out = self.start();
        out.status(ChatStatus::Working, Some("正在重新连接 Codex".into()));
        out
    }
}

/// An idle chat stops its app-server after this long (default 90 s), releasing the thread's
/// writer lock so the desktop app or `codex resume` can continue the thread. A message from
/// the phone starts it again on the same thread. `YONDER_CODEX_IDLE_RELEASE_SECS=0` keeps the
/// app-server running.
fn release_after() -> Option<std::time::Duration> {
    let secs = std::env::var("YONDER_CODEX_IDLE_RELEASE_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(90);
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

enum Pending {
    Initialize,
    ThreadStart,
    /// `thread/resume` with its params, to fork instead when another process owns the thread.
    Resume(Value),
    /// `thread/turns/list` for the last turns of a resumed thread (history replay).
    RecentTurns,
    TurnStart,
    /// `turn/steer` with the text and attachments it carried, to fall back to a queued turn.
    Steer(String, Vec<std::path::PathBuf>),
    Other,
}

struct PendingApproval {
    rpc_id: Value,
    method: String,
    kind: ApprovalKind,
}

pub struct CodexState {
    launch: AgentLaunch,
    /// Approval mode in effect (starts from the launch mode; `set_approval_mode` changes it).
    mode: ApprovalMode,
    /// The mode changed after the thread started: the next `turn/start` carries the policy.
    mode_dirty: bool,
    /// Model chosen after the thread started: every `turn/start` from now on carries it (Codex
    /// keeps a turn's model for the following turns, sending it again is harmless).
    turn_model: Option<String>,
    /// Switched to full access while a turn ran: that turn keeps the policy it started with, so
    /// its command / file approvals are answered here until it ends. Later turns run with
    /// `approvalPolicy = never`; anything Codex still asks then goes to the user.
    auto_turn: bool,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    thread: Option<String>,
    turn: Option<String>,
    queued: Vec<(String, Vec<std::path::PathBuf>)>,
    approvals: HashMap<String, PendingApproval>,
    items: HashMap<String, ChatItem>,
    busy: bool,
    /// MCP servers still starting; a turn cannot build its tool list until they are done.
    mcp_starting: std::collections::BTreeSet<String>,
    /// MCP servers that failed to start, reported once (as one chat line) when startup settles.
    mcp_failed: Vec<String>,
    mcp_reported: bool,
    /// This Codex has no `turn/steer` (older releases): messages sent during a turn wait for it.
    no_steer: bool,
    /// Messages for the next turn: sent before the thread existed, or during a turn that
    /// cannot be steered.
    after_turn: Vec<(String, Vec<std::path::PathBuf>)>,
    /// Thread of a stopped (idle) app-server, resumed when the agent starts again.
    released: Option<String>,
    /// The app-server was started again after an idle stop: resume quietly (no history replay,
    /// no fork: the thread was ours).
    restarting: bool,
    pub exit_error: Option<String>,
}

/// `(approvalPolicy, sandbox)` for `thread/start` / `thread/resume`.
/// Auto is `never` in the workspace sandbox: `on-failure` is gone from newer Codex releases.
fn policy(mode: ApprovalMode) -> (&'static str, &'static str) {
    match mode {
        ApprovalMode::Ask => ("on-request", "workspace-write"),
        ApprovalMode::Auto => ("never", "workspace-write"),
        ApprovalMode::Yolo => ("never", "danger-full-access"),
    }
}

/// The `SandboxPolicy` object `turn/start` takes for a mode.
fn sandbox_policy(mode: ApprovalMode) -> Value {
    match mode {
        ApprovalMode::Yolo => json!({"type": "dangerFullAccess"}),
        ApprovalMode::Ask | ApprovalMode::Auto => json!({"type": "workspaceWrite"}),
    }
}

/// Turns of a resumed thread replayed into the chat (older ones stay in Codex's files).
const REPLAY_TURNS: u32 = 10;

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string)
}

impl CodexState {
    pub fn new(launch: AgentLaunch) -> Self {
        Self {
            mode: launch.approval,
            mode_dirty: false,
            turn_model: None,
            auto_turn: false,
            launch,
            next_id: 1,
            pending: HashMap::new(),
            thread: None,
            turn: None,
            queued: Vec::new(),
            approvals: HashMap::new(),
            items: HashMap::new(),
            busy: false,
            mcp_starting: Default::default(),
            mcp_failed: Vec::new(),
            mcp_reported: false,
            no_steer: false,
            after_turn: Vec::new(),
            released: None,
            restarting: false,
            exit_error: None,
        }
    }

    /// Status detail while a turn waits for MCP servers to start.
    fn mcp_wait_detail(&self) -> Option<String> {
        if self.mcp_starting.is_empty() {
            return None;
        }
        let names: Vec<&str> = self.mcp_starting.iter().take(3).map(String::as_str).collect();
        let more = if self.mcp_starting.len() > 3 { format!(" 等 {} 个", self.mcp_starting.len()) } else { String::new() };
        Some(format!("等待 MCP 服务启动：{}{more}", names.join("、")))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let kind = match method {
            "initialize" => Pending::Initialize,
            "thread/start" | "thread/fork" => Pending::ThreadStart,
            "thread/resume" => Pending::Resume(params.clone()),
            "thread/turns/list" => Pending::RecentTurns,
            "turn/start" => Pending::TurnStart,
            _ => Pending::Other,
        };
        self.pending.insert(id, kind);
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    /// Adds text to the running turn, or keeps it for the next one when this Codex cannot steer.
    fn steer(&mut self, text: String, attachments: Vec<std::path::PathBuf>, out: &mut Out) {
        let turn = self.turn.clone().unwrap_or_default();
        if turn.is_empty() || self.no_steer {
            self.hold(text, attachments, out);
            return;
        }
        let thread = self.thread.clone().unwrap_or_default();
        let input = Self::user_input(text.clone(), &attachments);
        let m = self.request("turn/steer", json!({"threadId": thread, "expectedTurnId": turn, "input": input}));
        if let Some(id) = m.get("id").and_then(|i| i.as_u64()) {
            self.pending.insert(id, Pending::Steer(text, attachments));
        }
        out.to_agent.push(m);
    }

    /// Keeps a message for the turn after the running one.
    fn hold(&mut self, text: String, attachments: Vec<std::path::PathBuf>, out: &mut Out) {
        self.after_turn.push((text, attachments));
        let mut it = item(format!("held-{}", random_id()), ChatItemKind::System, ItemStatus::Completed);
        it.text = Some("消息将在当前回合结束后发送".into());
        out.events.push(AdapterEvent::Item(it));
    }

    /// Starts the next turn with the oldest held message (one per turn, so each shows up as
    /// the user message it was).
    fn flush_held(&mut self, out: &mut Out) {
        if self.after_turn.is_empty() || self.busy {
            return;
        }
        let (text, attachments) = self.after_turn.remove(0);
        out.to_agent.push(self.turn_start(text, attachments));
        out.events.push(AdapterEvent::Status { status: ChatStatus::Working, detail: None });
    }

    /// `UserInput` items for a message and its attachments.
    fn user_input(text: String, attachments: &[std::path::PathBuf]) -> Vec<Value> {
        let mut input = vec![json!({"type": "text", "text": text, "text_elements": []})];
        for a in attachments {
            let is_image = a
                .extension()
                .map(|e| matches!(e.to_string_lossy().to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"))
                .unwrap_or(false);
            if is_image {
                input.push(json!({"type": "localImage", "path": a}));
            } else {
                input.push(json!({"type": "mention", "name": a.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), "path": a}));
            }
        }
        input
    }

    fn turn_start(&mut self, text: String, attachments: Vec<std::path::PathBuf>) -> Value {
        let input = Self::user_input(text, &attachments);
        let thread = self.thread.clone().unwrap_or_default();
        self.busy = true;
        self.auto_turn = false;
        let mut params = json!({"threadId": thread, "input": input});
        if std::mem::take(&mut self.mode_dirty) {
            // Applies to this turn and the following ones.
            params["approvalPolicy"] = json!(policy(self.mode).0);
            params["sandboxPolicy"] = sandbox_policy(self.mode);
        }
        if let Some(m) = &self.turn_model {
            params["model"] = json!(m);
        }
        self.request("turn/start", params)
    }

    /// JSON-RPC answer for a pending approval and the chosen option.
    fn decision(p: &PendingApproval, option_id: &str) -> Value {
        let decision = match (p.method.as_str(), option_id) {
            ("execCommandApproval" | "applyPatchApproval", "allow") => json!("approved"),
            ("execCommandApproval" | "applyPatchApproval", "allow_always") => json!("approved_for_session"),
            ("execCommandApproval" | "applyPatchApproval", "deny") => json!("denied"),
            ("execCommandApproval" | "applyPatchApproval", _) => json!("abort"),
            (_, "allow") => json!("accept"),
            (_, "allow_always") => json!("acceptForSession"),
            (_, "deny") => json!("decline"),
            _ => json!("cancel"),
        };
        json!({"jsonrpc": "2.0", "id": p.rpc_id, "result": {"decision": decision}})
    }

    /// Full access approves what the running turn (still on the old policy) asks. Permission
    /// requests carry a profile to grant and stay with the user.
    fn auto_approves(&self, kind: ApprovalKind) -> bool {
        self.mode == ApprovalMode::Yolo && self.auto_turn && matches!(kind, ApprovalKind::Command | ApprovalKind::FileChange)
    }

    pub fn on_cmd(&mut self, cmd: AdapterCmd) -> Out {
        let mut out = Out::default();
        match cmd {
            AdapterCmd::Send { text, attachments } => {
                if self.thread.is_none() {
                    self.queued.push((text, attachments));
                } else if self.busy {
                    self.steer(text, attachments, &mut out);
                } else {
                    out.to_agent.push(self.turn_start(text, attachments));
                }
            }
            AdapterCmd::Interrupt => {
                if let (Some(t), Some(turn)) = (self.thread.clone(), self.turn.clone()) {
                    out.to_agent.push(self.request("turn/interrupt", json!({"threadId": t, "turnId": turn})));
                }
            }
            AdapterCmd::Approve { approval_id, option_id } => {
                if let Some(p) = self.approvals.remove(&approval_id) {
                    out.to_agent.push(Self::decision(&p, &option_id));
                    out.events.push(AdapterEvent::ApprovalResolved { approval: approval_id, option: option_id });
                    if self.approvals.is_empty() && self.busy {
                        out.events.push(AdapterEvent::Status { status: ChatStatus::Working, detail: None });
                    }
                }
            }
            AdapterCmd::SetApprovalMode(mode) => {
                if mode != self.mode {
                    self.mode = mode;
                    // Before the thread exists `thread/start` uses the mode; after, the next turn does.
                    self.mode_dirty = self.thread.is_some() || self.pending.values().any(|p| matches!(p, Pending::ThreadStart | Pending::Resume(_)));
                }
                self.auto_turn = mode == ApprovalMode::Yolo && self.busy;
                out.events.push(AdapterEvent::ApprovalMode(mode));
                // The running turn keeps its policy; approve what it is waiting for instead.
                let ids: Vec<String> = self.approvals.iter().filter(|(_, p)| self.auto_approves(p.kind)).map(|(k, _)| k.clone()).collect();
                for id in ids {
                    if let Some(p) = self.approvals.remove(&id) {
                        out.to_agent.push(Self::decision(&p, "allow"));
                        out.events.push(AdapterEvent::ApprovalResolved { approval: id, option: "allow".into() });
                    }
                }
                if self.approvals.is_empty() && self.busy {
                    out.events.push(AdapterEvent::Status { status: ChatStatus::Working, detail: None });
                }
            }
            AdapterCmd::SetModel(model) => {
                let started = self.thread.is_some() || self.pending.values().any(|p| matches!(p, Pending::ThreadStart | Pending::Resume(_)));
                if started {
                    self.turn_model = Some(model.clone());
                } else {
                    // `thread/start` / `thread/resume` has not been sent yet: it takes the model.
                    self.launch.model = Some(model.clone());
                }
                out.events.push(AdapterEvent::Model(model));
            }
            AdapterCmd::Shutdown => {}
        }
        out
    }

    pub fn on_message(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        let method = v.get("method").and_then(|m| m.as_str());
        let id = v.get("id");
        match (method, id) {
            (Some(m), Some(rpc_id)) => self.on_server_request(m, rpc_id.clone(), v.get("params").cloned().unwrap_or(Value::Null), &mut out),
            (Some(m), None) => self.on_notification(m, v.get("params").cloned().unwrap_or(Value::Null), &mut out),
            (None, Some(rpc_id)) => self.on_response(rpc_id, v, &mut out),
            _ => {}
        }
        out
    }

    fn on_response(&mut self, rpc_id: &Value, v: &Value, out: &mut Out) {
        let Some(n) = rpc_id.as_u64() else { return };
        let Some(kind) = self.pending.remove(&n) else { return };
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("error").to_string();
            if let Pending::Steer(text, attachments) = kind {
                // Older Codex (no turn/steer) or the turn just ended: send it as its own turn.
                if msg.contains("unknown variant") || err.get("code").and_then(|c| c.as_i64()) == Some(-32601) {
                    self.no_steer = true;
                }
                if self.busy {
                    self.hold(text, attachments, out);
                } else {
                    out.to_agent.push(self.turn_start(text, attachments));
                    out.events.push(AdapterEvent::Status { status: ChatStatus::Working, detail: None });
                }
                return;
            }
            // The thread is open in another Codex (desktop app, TUI): continue in a fork of it.
            if let Pending::Resume(params) = &kind {
                if msg.contains("active writer") {
                    let mut it = item(format!("fork-{n}"), ChatItemKind::System, ItemStatus::Completed);
                    it.text = Some(if self.restarting {
                        "原会话已在桌面端或终端打开，这条消息改在它的副本里继续：原会话不受影响".into()
                    } else {
                        "原会话正在桌面端或终端运行，这里是它的副本：原会话不受影响，副本在你发消息前保持空闲".into()
                    });
                    out.events.push(AdapterEvent::Item(it));
                    let m = self.request("thread/fork", params.clone());
                    out.to_agent.push(m);
                    return;
                }
            }
            // History replay is optional (older Codex has no `thread/turns/list`).
            if matches!(kind, Pending::RecentTurns) {
                return;
            }
            let mut it = item(format!("err-{n}"), ChatItemKind::Error, ItemStatus::Failed);
            it.text = Some(msg.clone());
            out.events.push(AdapterEvent::Item(it));
            if matches!(kind, Pending::Initialize | Pending::ThreadStart | Pending::Resume(_)) {
                self.exit_error = Some(msg.clone());
                out.events.push(AdapterEvent::Status { status: ChatStatus::Error, detail: Some(msg) });
            } else if matches!(kind, Pending::TurnStart) {
                self.busy = false;
                out.events.push(AdapterEvent::Status { status: ChatStatus::Idle, detail: None });
                self.flush_held(out);
            }
            return;
        }
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        match kind {
            Pending::Initialize => {
                out.to_agent.push(json!({"jsonrpc": "2.0", "method": "initialized"}));
                let (approval, sandbox) = policy(self.mode);
                self.mode_dirty = false;
                let mut params = json!({"cwd": self.launch.cwd, "approvalPolicy": approval, "sandbox": sandbox});
                if let Some(m) = &self.launch.model {
                    params["model"] = json!(m);
                }
                // After an idle stop: the thread this chat had (wins over the launch resume id).
                let resume = self.released.clone().or_else(|| self.launch.resume.clone());
                let msg = match resume {
                    Some(t) => {
                        params["threadId"] = json!(t);
                        // Long threads are tens of MB with every turn; the last few come from
                        // `thread/turns/list` instead (Codex without it ignores the flag).
                        params["excludeTurns"] = json!(true);
                        self.request("thread/resume", params)
                    }
                    None => self.request("thread/start", params),
                };
                out.to_agent.push(msg);
            }
            Pending::ThreadStart | Pending::Resume(_) => {
                let thread = result.get("thread");
                if let Some(tid) = thread.and_then(|t| s(t, "id")) {
                    self.thread = Some(tid.clone());
                    out.events.push(AdapterEvent::AgentSession(tid));
                }
                // A model picked while the thread was starting wins over the one it started with.
                if let Some(m) = self.turn_model.clone().or_else(|| s(&result, "model")) {
                    out.events.push(AdapterEvent::Model(m));
                }
                // Replay history of a resumed thread: inline turns (older Codex), else the last
                // turns fetched separately. Not after an idle stop: the chat already has them.
                let quiet = std::mem::take(&mut self.restarting);
                self.released = None;
                let inline = thread.and_then(|t| t.get("turns")).and_then(|t| t.as_array()).filter(|t| !t.is_empty());
                match (inline, &self.thread) {
                    _ if quiet => {}
                    (Some(turns), _) => self.replay(turns, out),
                    (None, Some(tid)) if self.launch.resume.is_some() => {
                        let tid = tid.clone();
                        out.to_agent.push(self.request("thread/turns/list", json!({"threadId": tid, "limit": REPLAY_TURNS, "itemsView": "full"})));
                    }
                    _ => {}
                }
                if self.queued.is_empty() {
                    out.events.push(AdapterEvent::Status { status: ChatStatus::Idle, detail: None });
                }
                for (text, att) in std::mem::take(&mut self.queued) {
                    let m = self.turn_start(text, att);
                    out.to_agent.push(m);
                    out.events.push(AdapterEvent::Status { status: ChatStatus::Working, detail: None });
                }
            }
            Pending::TurnStart => {
                if let Some(t) = result.get("turn").and_then(|t| s(t, "id")) {
                    self.turn = Some(t);
                }
            }
            Pending::RecentTurns => {
                // Newest first; replay oldest first. Items the live stream already sent keep
                // their place (same ids).
                let mut turns: Vec<Value> = result.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
                turns.reverse();
                self.replay(&turns, out);
            }
            Pending::Steer(..) | Pending::Other => {}
        }
    }

    fn on_server_request(&mut self, method: &str, rpc_id: Value, params: Value, out: &mut Out) {
        let approval_id = format!("codex-{}", rpc_id);
        let cwd = s(&params, "cwd");
        let reason = s(&params, "reason");
        let item_id = s(&params, "itemId");
        let is_command = matches!(method, "item/commandExecution/requestApproval" | "execCommandApproval");
        // The command may come as a string or argv, or only on the item (seen on Windows).
        let command = match params.get("command") {
            Some(Value::Array(a)) => Some(a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")),
            Some(Value::String(c)) => Some(c.clone()),
            _ => None,
        }
        .filter(|c| !c.trim().is_empty())
        .or_else(|| if is_command { item_id.as_ref().and_then(|i| self.items.get(i)).and_then(|i| i.title.clone()) } else { None });
        let (kind, title, diff) = match method {
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                let title = match &command {
                    Some(c) => format!("运行命令: {}", short_cmd(c)),
                    None => "运行命令".to_string(),
                };
                (ApprovalKind::Command, title, None)
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => {
                let diff = item_id.as_ref().and_then(|i| self.items.get(i)).and_then(|i| i.diff.clone());
                (ApprovalKind::FileChange, "修改文件".to_string(), diff)
            }
            "item/permissions/requestApproval" => (ApprovalKind::Permission, "请求额外权限".to_string(), None),
            "item/tool/requestUserInput" | "mcpServer/elicitation/request" => {
                // Not supported from the phone yet: decline so the agent does not hang.
                out.to_agent.push(json!({"jsonrpc": "2.0", "id": rpc_id, "error": {"code": -32000, "message": "not supported by yonder client"}}));
                return;
            }
            _ => {
                out.to_agent.push(json!({"jsonrpc": "2.0", "id": rpc_id, "error": {"code": -32601, "message": "method not supported"}}));
                return;
            }
        };
        let mut options = vec![
            ApprovalOption { id: "allow".into(), label: "允许".into(), kind: OptionKind::Allow },
            ApprovalOption { id: "allow_always".into(), label: "本会话总是允许".into(), kind: OptionKind::AllowAlways },
            ApprovalOption { id: "deny".into(), label: "拒绝".into(), kind: OptionKind::Deny },
            ApprovalOption { id: "abort".into(), label: "拒绝并停止".into(), kind: OptionKind::Abort },
        ];
        if method == "item/permissions/requestApproval" {
            options.retain(|o| o.id != "allow_always");
        }
        let pending = PendingApproval { rpc_id, method: method.to_string(), kind };
        // Switched to full access while this turn still runs on the old policy: answer for the user.
        if self.auto_approves(kind) {
            out.to_agent.push(Self::decision(&pending, "allow"));
            return;
        }
        let approval = Approval {
            id: approval_id.clone(),
            kind,
            title,
            command: command.clone(),
            cwd,
            diff,
            reason,
            detail: None,
            options,
            item: item_id,
            ts: now_ms(),
        };
        self.approvals.insert(approval_id, pending);
        out.events.push(AdapterEvent::ApprovalRequested(approval));
        out.events.push(AdapterEvent::Status { status: ChatStatus::AwaitingApproval, detail: None });
    }

    fn on_notification(&mut self, method: &str, p: Value, out: &mut Out) {
        match method {
            "turn/started" => {
                self.busy = true;
                if let Some(t) = p.get("turn").and_then(|t| s(t, "id")) {
                    self.turn = Some(t);
                }
                out.events.push(AdapterEvent::Status { status: ChatStatus::Working, detail: self.mcp_wait_detail() });
            }
            "turn/completed" => {
                self.busy = false;
                self.turn = None;
                self.auto_turn = false;
                let turn = p.get("turn").cloned().unwrap_or(Value::Null);
                let status = s(&turn, "status").unwrap_or_default();
                if status == "failed" {
                    let msg = turn.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).unwrap_or("turn failed").to_string();
                    let mut it = item(format!("turnerr-{}", now_ms()), ChatItemKind::Error, ItemStatus::Failed);
                    it.text = Some(msg);
                    out.events.push(AdapterEvent::Item(it));
                }
                // Anything still in progress is done now.
                let open: Vec<String> = self.items.iter().filter(|(_, i)| i.status == ItemStatus::InProgress).map(|(k, _)| k.clone()).collect();
                for k in open {
                    if let Some(mut it) = self.items.remove(&k) {
                        it.status = if status == "interrupted" { ItemStatus::Declined } else { ItemStatus::Completed };
                        out.events.push(AdapterEvent::Item(it));
                    }
                }
                self.items.clear();
                out.events.push(AdapterEvent::Status { status: ChatStatus::Idle, detail: if status == "interrupted" { Some("interrupted".into()) } else { None } });
                // A message that could not steer the finished turn starts the next one.
                self.flush_held(out);
            }
            "item/started" | "item/completed" => {
                let completed = method == "item/completed";
                if let Some(it) = p.get("item") {
                    if let Some(ci) = self.map_item(it, completed) {
                        if completed {
                            self.items.remove(&ci.id);
                        } else {
                            self.items.insert(ci.id.clone(), ci.clone());
                        }
                        out.events.push(AdapterEvent::Item(ci));
                    }
                }
            }
            "item/agentMessage/delta" | "item/plan/delta" => {
                if let (Some(item), Some(d)) = (s(&p, "itemId"), s(&p, "delta")) {
                    out.events.push(AdapterEvent::Delta { item, field: DeltaField::Text, delta: d });
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                if let (Some(item), Some(d)) = (s(&p, "itemId"), s(&p, "delta")) {
                    out.events.push(AdapterEvent::Delta { item, field: DeltaField::Text, delta: d });
                }
            }
            "item/reasoning/summaryPartAdded" => {
                if let Some(item) = s(&p, "itemId") {
                    out.events.push(AdapterEvent::Delta { item, field: DeltaField::Text, delta: "\n\n".into() });
                }
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                if let (Some(item), Some(d)) = (s(&p, "itemId"), s(&p, "delta")) {
                    out.events.push(AdapterEvent::Delta { item, field: DeltaField::Output, delta: d });
                }
            }
            "serverRequest/resolved" => {
                if let Some(req) = p.get("requestId") {
                    let id = format!("codex-{req}");
                    if self.approvals.remove(&id).is_some() {
                        out.events.push(AdapterEvent::ApprovalResolved { approval: id, option: "resolved".into() });
                    }
                }
            }
            "thread/status/changed" => {
                let flags = p.get("status").and_then(|s| s.get("activeFlags")).and_then(|f| f.as_array()).cloned().unwrap_or_default();
                if flags.iter().any(|f| f.as_str() == Some("waitingOnApproval")) {
                    out.events.push(AdapterEvent::Status { status: ChatStatus::AwaitingApproval, detail: None });
                }
            }
            "model/rerouted" => {
                if let Some(m) = s(&p, "toModel").or_else(|| s(&p, "model")) {
                    out.events.push(AdapterEvent::Model(m));
                }
            }
            "mcpServer/startupStatus/updated" => {
                let Some(name) = s(&p, "name") else { return };
                let before = self.mcp_wait_detail();
                match s(&p, "status").as_deref() {
                    Some("starting") => {
                        self.mcp_starting.insert(name);
                    }
                    Some("failed") => {
                        self.mcp_starting.remove(&name);
                        if !self.mcp_failed.contains(&name) {
                            self.mcp_failed.push(name);
                        }
                    }
                    _ => {
                        self.mcp_starting.remove(&name);
                    }
                }
                // One line per agent process once every server has settled, not one per server.
                if self.mcp_starting.is_empty() && !self.mcp_failed.is_empty() && !self.mcp_reported {
                    self.mcp_reported = true;
                    let shown: Vec<&str> = self.mcp_failed.iter().take(5).map(String::as_str).collect();
                    let more = if self.mcp_failed.len() > 5 { format!(" 等 {} 个", self.mcp_failed.len()) } else { String::new() };
                    let mut it = item(format!("mcp-failed-{}", now_ms()), ChatItemKind::System, ItemStatus::Completed);
                    it.text = Some(format!("MCP 服务启动失败：{}{more}", shown.join("、")));
                    out.events.push(AdapterEvent::Item(it));
                }
                let after = self.mcp_wait_detail();
                if self.busy && self.approvals.is_empty() && before != after {
                    out.events.push(AdapterEvent::Status { status: ChatStatus::Working, detail: after });
                }
            }
            "error" => {
                let msg = p.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).map(str::to_string).or_else(|| s(&p, "message")).unwrap_or_else(|| "error".into());
                let will_retry = p.get("willRetry").and_then(|w| w.as_bool()).unwrap_or(false);
                if !will_retry {
                    let mut it = item(format!("err-{}", now_ms()), ChatItemKind::Error, ItemStatus::Failed);
                    it.text = Some(msg);
                    out.events.push(AdapterEvent::Item(it));
                }
            }
            _ => {}
        }
    }

    /// Map a Codex ThreadItem to a ChatItem.
    /// History items of resumed turns.
    fn replay(&self, turns: &[Value], out: &mut Out) {
        for turn in turns {
            for it in turn.get("items").and_then(|i| i.as_array()).into_iter().flatten() {
                if let Some(ci) = self.map_item(it, true) {
                    out.events.push(AdapterEvent::Item(ci));
                }
            }
        }
    }

    fn map_item(&self, it: &Value, completed: bool) -> Option<ChatItem> {
        let ty = s(it, "type")?;
        let id = s(it, "id")?;
        let status = if completed { ItemStatus::Completed } else { ItemStatus::InProgress };
        let mut ci = match ty.as_str() {
            "userMessage" => {
                let mut c = item(&id, ChatItemKind::User, ItemStatus::Completed);
                let mut text = String::new();
                for part in it.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
                    match s(part, "type").as_deref() {
                        Some("text") => text.push_str(&s(part, "text").unwrap_or_default()),
                        Some("localImage") | Some("mention") => c.paths.push(s(part, "path").unwrap_or_default()),
                        _ => {}
                    }
                }
                c.text = Some(text);
                c
            }
            "agentMessage" => {
                let mut c = item(&id, ChatItemKind::Agent, status);
                c.text = Some(s(it, "text").unwrap_or_default());
                c
            }
            "reasoning" => {
                let mut c = item(&id, ChatItemKind::Reasoning, status);
                let summary: Vec<String> = it.get("summary").and_then(|x| x.as_array()).into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
                c.text = Some(summary.join("\n\n"));
                c
            }
            "plan" => {
                let mut c = item(&id, ChatItemKind::Plan, status);
                c.text = s(it, "text");
                c
            }
            "commandExecution" => {
                let st = match s(it, "status").as_deref() {
                    Some("completed") => ItemStatus::Completed,
                    Some("failed") => ItemStatus::Failed,
                    Some("declined") => ItemStatus::Declined,
                    _ => ItemStatus::InProgress,
                };
                let mut c = item(&id, ChatItemKind::Command, st);
                c.title = s(it, "command").map(|cmd| unwrap_shell(&cmd));
                c.output = s(it, "aggregatedOutput").map(|o| truncate_tail(&o, MAX_OUTPUT));
                c.exit_code = it.get("exitCode").and_then(|x| x.as_i64()).map(|x| x as i32);
                c.duration_ms = it.get("durationMs").and_then(|x| x.as_u64());
                c
            }
            "fileChange" => {
                let st = match s(it, "status").as_deref() {
                    Some("completed") => ItemStatus::Completed,
                    Some("failed") => ItemStatus::Failed,
                    Some("declined") => ItemStatus::Declined,
                    _ => ItemStatus::InProgress,
                };
                let mut c = item(&id, ChatItemKind::FileChange, st);
                let mut diff = String::new();
                for ch in it.get("changes").and_then(|x| x.as_array()).into_iter().flatten() {
                    let path = s(ch, "path").unwrap_or_default();
                    c.paths.push(path.clone());
                    let d = s(ch, "diff").unwrap_or_default();
                    if !d.starts_with("---") && !d.starts_with("diff ") {
                        diff.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
                    }
                    diff.push_str(&d);
                    if !diff.ends_with('\n') {
                        diff.push('\n');
                    }
                }
                c.title = Some(match c.paths.len() {
                    1 => c.paths[0].clone(),
                    n => format!("{n} 个文件"),
                });
                c.diff = Some(truncate_tail(&diff, MAX_OUTPUT * 2));
                c
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let st = match s(it, "status").as_deref() {
                    Some("completed") => ItemStatus::Completed,
                    Some("failed") => ItemStatus::Failed,
                    _ => status,
                };
                let mut c = item(&id, ChatItemKind::Tool, st);
                let server = s(it, "server").or_else(|| s(it, "namespace"));
                let tool = s(it, "tool").unwrap_or_default();
                c.title = Some(match server {
                    Some(sv) => format!("{sv}.{tool}"),
                    None => tool,
                });
                let args = it.get("arguments").map(|a| serde_json::to_string_pretty(a).unwrap_or_default());
                let result = it.get("result").filter(|r| !r.is_null()).map(|r| serde_json::to_string_pretty(r).unwrap_or_default())
                    .or_else(|| it.get("contentItems").filter(|r| !r.is_null()).map(|r| serde_json::to_string_pretty(r).unwrap_or_default()))
                    .or_else(|| it.get("error").filter(|r| !r.is_null()).map(|r| r.to_string()));
                c.text = args;
                c.output = result.map(|r| truncate_tail(&r, MAX_OUTPUT));
                c.duration_ms = it.get("durationMs").and_then(|x| x.as_u64());
                c
            }
            "webSearch" => {
                let mut c = item(&id, ChatItemKind::WebSearch, status);
                c.title = s(it, "query");
                c
            }
            "imageView" | "imageGeneration" => {
                let mut c = item(&id, ChatItemKind::Tool, status);
                c.title = Some(if ty == "imageView" { "查看图片".into() } else { "生成图片".into() });
                if let Some(p) = s(it, "path").or_else(|| s(it, "savedPath")) {
                    c.paths.push(p);
                }
                c
            }
            "contextCompaction" => {
                let mut c = item(&id, ChatItemKind::System, ItemStatus::Completed);
                c.text = Some("上下文已压缩".into());
                c
            }
            _ => return None,
        };
        ci.ts = now_ms();
        Some(ci)
    }
}

fn short_cmd(c: &str) -> String {
    let c = unwrap_shell(c);
    if c.chars().count() > 80 {
        format!("{}…", c.chars().take(80).collect::<String>())
    } else {
        c
    }
}

/// `/bin/zsh -lc 'touch x'` -> `touch x`
fn unwrap_shell(c: &str) -> String {
    for prefix in ["/bin/zsh -lc ", "/bin/bash -lc ", "bash -lc ", "zsh -lc ", "/bin/sh -c ", "sh -c "] {
        if let Some(rest) = c.strip_prefix(prefix) {
            let rest = rest.trim();
            if rest.len() >= 2 && ((rest.starts_with('\'') && rest.ends_with('\'')) || (rest.starts_with('"') && rest.ends_with('"'))) {
                return rest[1..rest.len() - 1].replace("'\\''", "'");
            }
            return rest.to_string();
        }
    }
    c.to_string()
}

/// Recorded-fixture driven tests.
#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> AgentLaunch {
        AgentLaunch {
            agent: AgentKind::Codex,
            cwd: "/tmp".into(),
            model: None,
            approval: ApprovalMode::Ask,
            resume: None,
            env: Default::default(),
            program: None,
            login_shell: false,
        }
    }

    fn replay(name: &str) -> (CodexState, Vec<AdapterEvent>) {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let mut st = CodexState::new(launch());
        st.pending.insert(1, Pending::Initialize);
        st.next_id = 100;
        let mut evs = Vec::new();
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let o: Value = serde_json::from_str(line).unwrap();
            let Some(raw) = o.get("raw").and_then(|r| r.as_str()) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(raw) else { continue };
            // Responses in fixtures use the recorder's ids (2 = thread/start, 3 = turn/start).
            if let Some(n) = v.get("id").and_then(|i| i.as_u64()) {
                if v.get("method").is_none() {
                    match n {
                        1 => {}
                        2 => {
                            st.pending.insert(2, Pending::ThreadStart);
                        }
                        3 => {
                            st.pending.insert(3, Pending::TurnStart);
                        }
                        _ => {}
                    }
                }
            }
            let out = st.on_message(&v);
            evs.extend(out.events);
        }
        (st, evs)
    }

    #[test]
    fn pong_turn() {
        let (st, evs) = replay("codex_pong.jsonl");
        assert!(st.thread.is_some());
        assert!(evs.iter().any(|e| matches!(e, AdapterEvent::AgentSession(_))));
        let agent_text: Vec<String> = evs
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::Item(i) if i.kind == ChatItemKind::Agent && i.status == ItemStatus::Completed => i.text.clone(),
                _ => None,
            })
            .collect();
        assert!(agent_text.iter().any(|t| t.contains("PONG")), "{agent_text:?}");
        assert!(matches!(evs.last(), Some(AdapterEvent::Status { status: ChatStatus::Idle, .. })));
    }

    /// A turn waiting on slow MCP servers says so; a failed server shows up in the chat.
    #[test]
    fn mcp_startup_status() {
        let mut st = CodexState::new(launch());
        st.busy = true;
        let upd = |name: &str, status: &str, error: Value| {
            json!({"jsonrpc": "2.0", "method": "mcpServer/startupStatus/updated", "params": {"threadId": "t", "name": name, "status": status, "error": error}})
        };
        st.on_message(&upd("figma", "starting", Value::Null));
        let out = st.on_message(&upd("pencil", "starting", Value::Null));
        let detail = out.events.iter().find_map(|e| match e {
            AdapterEvent::Status { status: ChatStatus::Working, detail } => detail.clone(),
            _ => None,
        });
        assert_eq!(detail.as_deref(), Some("等待 MCP 服务启动：figma、pencil"));
        // A failure is reported once, after every server has settled.
        let out = st.on_message(&upd("pencil", "failed", json!("No such file or directory")));
        assert!(!out.events.iter().any(|e| matches!(e, AdapterEvent::Item(_))));
        let out = st.on_message(&upd("figma", "ready", Value::Null));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Status { status: ChatStatus::Working, detail: None })));
        let reports: Vec<String> = out
            .events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::Item(i) if i.kind == ChatItemKind::System => i.text.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(reports, vec!["MCP 服务启动失败：pencil".to_string()]);
        // Later status updates (e.g. a resumed thread) do not repeat it.
        st.on_message(&upd("pencil", "starting", Value::Null));
        let out = st.on_message(&upd("pencil", "failed", Value::Null));
        assert!(!out.events.iter().any(|e| matches!(e, AdapterEvent::Item(_))));
    }

    /// The recorded session from this machine had 14 failing servers: one chat line in total.
    #[test]
    fn mcp_failures_from_recording() {
        let (_, evs) = replay("codex_pong.jsonl");
        let system: Vec<_> = evs.iter().filter(|e| matches!(e, AdapterEvent::Item(i) if i.kind == ChatItemKind::System)).collect();
        assert_eq!(system.len(), 1, "{system:?}");
    }

    /// An approval without a command param shows the command of the item it belongs to.
    #[test]
    fn approval_command_from_item() {
        let mut st = CodexState::new(launch());
        st.busy = true;
        st.on_message(&json!({"jsonrpc": "2.0", "method": "item/started", "params": {"item": {"type": "commandExecution", "id": "c1", "command": "powershell.exe -Command \"New-Item x\"", "status": "inProgress"}}}));
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": 7, "method": "item/commandExecution/requestApproval", "params": {"itemId": "c1", "threadId": "t", "turnId": "u", "reason": "outside"}}));
        let a = out.events.iter().find_map(|e| match e {
            AdapterEvent::ApprovalRequested(a) => Some(a.clone()),
            _ => None,
        });
        let a = a.expect("approval");
        assert_eq!(a.command.as_deref(), Some("powershell.exe -Command \"New-Item x\""));
        assert!(a.title.contains("New-Item"), "{}", a.title);
    }

    #[test]
    fn escalation_approval() {
        let (_, evs) = replay("codex_escalate_touch.jsonl");
        let approval = evs.iter().find_map(|e| match e {
            AdapterEvent::ApprovalRequested(a) => Some(a.clone()),
            _ => None,
        });
        let a = approval.expect("approval requested");
        assert_eq!(a.kind, ApprovalKind::Command);
        assert!(a.command.as_deref().unwrap_or("").contains("approval_test.txt"));
        assert!(a.options.iter().any(|o| o.kind == OptionKind::Allow));
        let cmd = evs.iter().find_map(|e| match e {
            AdapterEvent::Item(i) if i.kind == ChatItemKind::Command && i.status == ItemStatus::Completed => Some(i.clone()),
            _ => None,
        });
        let cmd = cmd.expect("command item");
        assert!(cmd.title.as_deref().unwrap().starts_with("touch "));
        assert_eq!(cmd.exit_code, Some(0));
    }

    #[test]
    fn approve_maps_decision() {
        let mut st = CodexState::new(launch());
        let mut out = Out::default();
        st.on_server_request("item/commandExecution/requestApproval", json!(7), json!({"command": "ls", "itemId": "x"}), &mut out);
        let AdapterEvent::ApprovalRequested(a) = &out.events[0] else { panic!() };
        let out = st.on_cmd(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "allow".into() });
        assert_eq!(out.to_agent[0], json!({"jsonrpc": "2.0", "id": 7, "result": {"decision": "accept"}}));
        assert_eq!(unwrap_shell("/bin/zsh -lc 'echo '\\''hi'\\'''"), "echo 'hi'");
    }

    /// Auto must not send `on-failure`: current Codex rejects it at thread/start.
    #[test]
    fn policies_known_to_codex() {
        for m in [ApprovalMode::Ask, ApprovalMode::Auto, ApprovalMode::Yolo] {
            assert!(["untrusted", "on-request", "never"].contains(&policy(m).0), "{m:?}");
        }
        assert_eq!(policy(ApprovalMode::Yolo), ("never", "danger-full-access"));
        assert_eq!(sandbox_policy(ApprovalMode::Yolo), json!({"type": "dangerFullAccess"}));
    }

    /// Switching to full access mid-session: pending command approvals are accepted, a
    /// permission request stays, and the next turn carries the new policy (once).
    #[test]
    fn switch_to_yolo_mid_session() {
        let mut st = CodexState::new(launch());
        st.thread = Some("t1".into());
        st.busy = true;
        let mut out = Out::default();
        st.on_server_request("item/commandExecution/requestApproval", json!(7), json!({"command": "ls", "itemId": "x"}), &mut out);
        st.on_server_request("item/permissions/requestApproval", json!(8), json!({"itemId": "y", "permissions": {}}), &mut out);
        assert_eq!(st.approvals.len(), 2);

        let out = st.on_cmd(AdapterCmd::SetApprovalMode(ApprovalMode::Yolo));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::ApprovalMode(ApprovalMode::Yolo))));
        assert_eq!(out.to_agent, vec![json!({"jsonrpc": "2.0", "id": 7, "result": {"decision": "accept"}})]);
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::ApprovalResolved { option, .. } if option == "allow")));
        assert_eq!(st.approvals.len(), 1, "the permission request stays with the user");

        // Another command approval of the still-running turn is answered without asking.
        let mut out = Out::default();
        st.on_server_request("item/commandExecution/requestApproval", json!(9), json!({"command": "rm x", "itemId": "z"}), &mut out);
        assert!(out.events.is_empty());
        assert_eq!(out.to_agent, vec![json!({"jsonrpc": "2.0", "id": 9, "result": {"decision": "accept"}})]);

        // The next turn starts with the new policy, the one after it does not repeat it.
        st.busy = false;
        let out = st.on_cmd(AdapterCmd::Send { text: "go".into(), attachments: vec![] });
        let p = &out.to_agent[0]["params"];
        assert_eq!(p["approvalPolicy"], "never");
        assert_eq!(p["sandboxPolicy"], json!({"type": "dangerFullAccess"}));
        st.busy = false;
        st.turn = None;
        let out = st.on_cmd(AdapterCmd::Send { text: "again".into(), attachments: vec![] });
        assert!(out.to_agent[0]["params"].get("approvalPolicy").is_none());

        // And back to asking.
        st.busy = false;
        st.on_cmd(AdapterCmd::SetApprovalMode(ApprovalMode::Ask));
        let out = st.on_cmd(AdapterCmd::Send { text: "careful".into(), attachments: vec![] });
        assert_eq!(out.to_agent[0]["params"]["approvalPolicy"], "on-request");
        assert_eq!(out.to_agent[0]["params"]["sandboxPolicy"], json!({"type": "workspaceWrite"}));
    }

    /// Only the turn that was running at the switch is answered for the user: a later turn runs
    /// with `never`, so whatever Codex still asks there is shown.
    #[test]
    fn yolo_auto_accept_ends_with_the_turn() {
        let mut st = CodexState::new(launch());
        st.thread = Some("t1".into());
        st.busy = true;
        st.on_cmd(AdapterCmd::SetApprovalMode(ApprovalMode::Yolo));
        let mut out = Out::default();
        st.on_server_request("item/commandExecution/requestApproval", json!(3), json!({"command": "ls", "itemId": "a"}), &mut out);
        assert!(out.events.is_empty(), "answered for the user in the running turn");
        st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"id": "x", "status": "completed"}}}));
        st.on_cmd(AdapterCmd::Send { text: "next".into(), attachments: vec![] });
        let mut out = Out::default();
        st.on_server_request("item/commandExecution/requestApproval", json!(4), json!({"command": "rm -rf /", "itemId": "b"}), &mut out);
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::ApprovalRequested(_))));
        assert!(out.to_agent.is_empty());
        // Switching while idle answers nothing by itself either.
        let mut st = CodexState::new(launch());
        st.thread = Some("t1".into());
        st.on_cmd(AdapterCmd::SetApprovalMode(ApprovalMode::Yolo));
        st.busy = true;
        let mut out = Out::default();
        st.on_server_request("item/commandExecution/requestApproval", json!(5), json!({"command": "ls", "itemId": "c"}), &mut out);
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::ApprovalRequested(_))));
    }

    /// A mode set before the thread exists goes into thread/start instead of the first turn.
    #[test]
    fn mode_before_thread_start() {
        let mut st = CodexState::new(launch());
        st.pending.insert(1, Pending::Initialize);
        st.on_cmd(AdapterCmd::SetApprovalMode(ApprovalMode::Yolo));
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        let start = out.to_agent.iter().find(|m| m["method"] == "thread/start").unwrap();
        assert_eq!(start["params"]["approvalPolicy"], "never");
        assert_eq!(start["params"]["sandbox"], "danger-full-access");
        st.thread = Some("t".into());
        let out = st.on_cmd(AdapterCmd::Send { text: "hi".into(), attachments: vec![] });
        assert!(out.to_agent[0]["params"].get("approvalPolicy").is_none());
    }

    /// A model switch is reported at once and rides on every later `turn/start`; before the
    /// thread exists it goes into `thread/start` instead.
    #[test]
    fn switch_model() {
        let mut st = CodexState::new(launch());
        st.thread = Some("t1".into());
        let out = st.on_cmd(AdapterCmd::Send { text: "one".into(), attachments: vec![] });
        assert!(out.to_agent[0]["params"].get("model").is_none());
        st.busy = false;
        let out = st.on_cmd(AdapterCmd::SetModel("gpt-5.5".into()));
        assert!(out.to_agent.is_empty());
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Model(m) if m == "gpt-5.5")));
        let out = st.on_cmd(AdapterCmd::Send { text: "two".into(), attachments: vec![] });
        assert_eq!(out.to_agent[0]["method"], "turn/start");
        assert_eq!(out.to_agent[0]["params"]["model"], "gpt-5.5");
        st.busy = false;
        let out = st.on_cmd(AdapterCmd::Send { text: "three".into(), attachments: vec![] });
        assert_eq!(out.to_agent[0]["params"]["model"], "gpt-5.5");

        let mut st = CodexState::new(launch());
        st.pending.insert(1, Pending::Initialize);
        st.on_cmd(AdapterCmd::SetModel("gpt-5.4-mini".into()));
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        let start = out.to_agent.iter().find(|m| m["method"] == "thread/start").unwrap();
        assert_eq!(start["params"]["model"], "gpt-5.4-mini");
    }

    /// Codex 0.72 has no `turn/steer`: a message sent during a turn becomes the next turn
    /// instead of an error, and later messages during a turn wait without trying to steer.
    #[test]
    fn steer_falls_back_to_next_turn() {
        let mut st = CodexState::new(launch());
        st.thread = Some("t1".into());
        let first = st.on_cmd(AdapterCmd::Send { text: "one".into(), attachments: vec![] });
        let n = first.to_agent[0]["id"].clone();
        st.on_message(&json!({"jsonrpc": "2.0", "id": n, "result": {"turn": {"id": "turn-1"}}}));
        st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/started", "params": {"turn": {"id": "turn-1"}}}));

        let out = st.on_cmd(AdapterCmd::Send { text: "two".into(), attachments: vec![] });
        assert_eq!(out.to_agent[0]["method"], "turn/steer");
        let sid = out.to_agent[0]["id"].clone();
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": sid, "error": {"code": -32600, "message": "Invalid request: unknown variant `turn/steer`, expected one of `initialize`"}}));
        assert!(!out.events.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.kind == ChatItemKind::Error)), "{:?}", out.events);
        assert!(out.to_agent.is_empty(), "waits for the running turn");

        // Known now: the next message during the turn does not try to steer.
        let out = st.on_cmd(AdapterCmd::Send { text: "three".into(), attachments: vec![] });
        assert!(out.to_agent.is_empty());

        // The turn ends: "two" starts the next turn, then "three" after that one.
        let out = st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}}));
        let start = out.to_agent.iter().find(|m| m["method"] == "turn/start").expect("next turn");
        assert_eq!(start["params"]["input"][0]["text"], "two");
        let out = st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"id": "turn-2", "status": "completed"}}}));
        let start = out.to_agent.iter().find(|m| m["method"] == "turn/start").expect("turn after");
        assert_eq!(start["params"]["input"][0]["text"], "three");
    }

    /// A steer that loses the race with the end of its turn is sent as a new turn.
    #[test]
    fn steer_after_turn_ended_starts_a_turn() {
        let mut st = CodexState::new(launch());
        st.thread = Some("t1".into());
        st.busy = true;
        st.turn = Some("turn-1".into());
        let out = st.on_cmd(AdapterCmd::Send { text: "late".into(), attachments: vec![] });
        let sid = out.to_agent[0]["id"].clone();
        st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}}));
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": sid, "error": {"code": -32600, "message": "expected active turn id turn-1 but found none"}}));
        assert_eq!(out.to_agent[0]["method"], "turn/start");
        assert_eq!(out.to_agent[0]["params"]["input"][0]["text"], "late");
        assert!(!st.no_steer);
    }

    /// A thread the desktop app holds cannot be resumed: the chat continues in a fork of it.
    #[test]
    fn resume_of_a_thread_held_elsewhere_forks_it() {
        let mut st = CodexState::new(AgentLaunch { resume: Some("t1".into()), ..launch() });
        st.pending.insert(1, Pending::Initialize);
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        let resume = out.to_agent.iter().find(|m| m["method"] == "thread/resume").expect("resume");
        let rid = resume["id"].clone();
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32600, "message": "thread t1 already has an active writer"}}));
        assert_eq!(out.to_agent[0]["method"], "thread/fork");
        assert_eq!(out.to_agent[0]["params"]["threadId"], "t1");
        assert!(!out.events.iter().any(|e| matches!(e, AdapterEvent::Status { status: ChatStatus::Error, .. })));
        let fid = out.to_agent[0]["id"].clone();
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": fid, "result": {"thread": {"id": "t2", "turns": [{"items": [{"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "hi"}]}]}]}}}));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::AgentSession(t) if t == "t2")));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.kind == ChatItemKind::User)));
        assert_eq!(st.thread.as_deref(), Some("t2"));
    }

    /// An idle chat lets go of its thread; the next message resumes the same thread quietly
    /// with the model picked meanwhile.
    #[test]
    fn idle_release_and_quiet_resume() {
        let mut st = CodexState::new(launch());
        st.thread = Some("t1".into());
        assert!(st.can_release());
        st.busy = true;
        assert!(!st.can_release());
        st.busy = false;
        st.on_cmd(AdapterCmd::SetModel("gpt-5.5".into()));
        st.on_release();
        assert!(st.thread.is_none());
        assert!(st.needs_agent(&AdapterCmd::Send { text: "x".into(), attachments: vec![] }));
        assert!(!st.needs_agent(&AdapterCmd::SetModel("m".into())));
        // Restart: initialize, then the message waits for the thread.
        let out = st.restart();
        let init = out.to_agent[0]["id"].clone();
        st.on_cmd(AdapterCmd::Send { text: "again".into(), attachments: vec![] });
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": init, "result": {}}));
        let resume = out.to_agent.iter().find(|m| m["method"] == "thread/resume").expect("resume");
        assert_eq!(resume["params"]["threadId"], "t1");
        assert_eq!(resume["params"]["model"], "gpt-5.5");
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": resume["id"].clone(), "result": {"thread": {"id": "t1", "turns": []}}}));
        assert!(!out.to_agent.iter().any(|m| m["method"] == "thread/turns/list"), "no history replay");
        let turn = out.to_agent.iter().find(|m| m["method"] == "turn/start").expect("turn");
        assert_eq!(turn["params"]["input"][0]["text"], "again");
        assert!(!out.events.iter().any(|e| matches!(e, AdapterEvent::AgentSession(t) if t != "t1")));
    }

    /// Resuming asks for metadata only, then replays the last turns oldest first.
    #[test]
    fn resume_replays_recent_turns() {
        let mut st = CodexState::new(AgentLaunch { resume: Some("t1".into()), ..launch() });
        st.pending.insert(1, Pending::Initialize);
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        let resume = out.to_agent.iter().find(|m| m["method"] == "thread/resume").expect("resume");
        assert_eq!(resume["params"]["excludeTurns"], true);
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": resume["id"].clone(), "result": {"thread": {"id": "t1", "turns": []}}}));
        let list = out.to_agent.iter().find(|m| m["method"] == "thread/turns/list").expect("turns/list");
        assert_eq!(list["params"]["threadId"], "t1");
        let user = |id: &str, text: &str| json!({"type": "userMessage", "id": id, "content": [{"type": "text", "text": text}]});
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": list["id"].clone(), "result": {"data": [
            {"id": "b", "items": [user("u2", "second")]},
            {"id": "a", "items": [user("u1", "first")]}
        ]}}));
        let texts: Vec<String> = out.events.iter().filter_map(|e| match e { AdapterEvent::Item(i) => i.text.clone(), _ => None }).collect();
        assert_eq!(texts, vec!["first", "second"]);
        // Without `thread/turns/list` (older Codex) the chat just starts empty.
        let mut st = CodexState::new(AgentLaunch { resume: Some("t1".into()), ..launch() });
        st.thread = Some("t1".into());
        let m = st.request("thread/turns/list", json!({}));
        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": m["id"].clone(), "error": {"code": -32601, "message": "unknown method"}}));
        assert!(out.events.is_empty());
    }
}
