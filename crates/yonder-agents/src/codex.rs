//! Codex adapter: `codex app-server` (JSON-RPC 2.0, one JSON object per line).

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Value};
use yonder_proto::app::{
    AgentKind, Approval, ApprovalKind, ApprovalMode, ApprovalOption, ChatItem, ChatItemKind, ChatStatus, DeltaField,
    ItemStatus, OptionKind, SubagentStatus,
};

use crate::common::{item, now_ms, random_id, set_subagent_status, subagent_active, subagent_card, truncate_tail, MAX_OUTPUT};
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
        // Sub-agents live inside the app-server: they are gone with it.
        for card in self.cards.values_mut() {
            if subagent_active(card) {
                set_subagent_status(card, SubagentStatus::Interrupted);
                out.event(AdapterEvent::Item(card.clone()));
            }
        }
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
        // Sub-agents keep running after the turn that spawned them; stopping the app-server
        // would kill them.
        self.thread.is_some()
            && !self.busy
            && self.approvals.is_empty()
            && self.pending.is_empty()
            && self.queued.is_empty()
            && self.after_turn.is_empty()
            && !self.cards.values().any(subagent_active)
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
    /// `thread/read` of a sub-agent's thread (its nickname and role).
    SubRead(String),
    /// `thread/turns/list` of a sub-agent's thread (history of a resumed chat).
    SubTurns(String),
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
    /// Sub-agent cards by card id (the id of the `spawnAgent` call).
    cards: HashMap<String, ChatItem>,
    /// Sub-agent thread id -> card id.
    subs: HashMap<String, String>,
    /// Sub-agent thread id -> its latest agent message (the card's reply when its turn ends).
    sub_text: HashMap<String, String>,
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
/// Sub-agents of a resumed thread whose threads are fetched for their read-only views.
const REPLAY_SUBAGENTS: usize = 8;

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
            cards: HashMap::new(),
            subs: HashMap::new(),
            sub_text: HashMap::new(),
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
                    // A sub-agent may ask after the parent's turn ended.
                    if self.approvals.is_empty() {
                        out.events.push(AdapterEvent::Status { status: if self.busy { ChatStatus::Working } else { ChatStatus::Idle }, detail: None });
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
            // History replay and sub-agent details are optional (older Codex has no
            // `thread/turns/list`; a sub-agent's thread may be gone).
            if matches!(kind, Pending::RecentTurns | Pending::SubRead(_) | Pending::SubTurns(_)) {
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
            Pending::SubRead(tid) => {
                let t = result.get("thread").cloned().unwrap_or(Value::Null);
                let Some(card) = self.subs.get(&tid).and_then(|c| self.cards.get_mut(c)) else { return };
                let Some(sub) = card.subagent.as_mut() else { return };
                let before = sub.clone();
                if let Some(n) = s(&t, "agentNickname").filter(|n| !n.is_empty()) {
                    sub.name = Some(n);
                }
                if let Some(r) = s(&t, "agentRole").filter(|r| !r.is_empty()) {
                    sub.role = Some(r);
                }
                if sub.model.is_none() {
                    sub.model = s(&t, "model").filter(|m| !m.is_empty());
                }
                if *sub != before {
                    out.events.push(AdapterEvent::Item(card.clone()));
                }
            }
            Pending::SubTurns(tid) => {
                let mut turns: Vec<Value> = result.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
                turns.reverse();
                for turn in &turns {
                    for it in turn.get("items").and_then(|i| i.as_array()).into_iter().flatten() {
                        if let Some(mut ci) = self.map_item(it, true) {
                            ci.thread = Some(tid.clone());
                            out.events.push(AdapterEvent::Item(ci));
                        }
                    }
                }
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
        // Sub-agents share this connection: their requests carry their own thread id.
        let thread = s(&params, "threadId").filter(|t| self.thread.as_ref().is_some_and(|own| own != t));
        let thread_name = thread.as_ref().and_then(|t| self.sub_name(t));
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
            thread,
            thread_name,
        };
        self.approvals.insert(approval_id, pending);
        out.events.push(AdapterEvent::ApprovalRequested(approval));
        out.events.push(AdapterEvent::Status { status: ChatStatus::AwaitingApproval, detail: None });
    }

    /// Sub-agents run in their own threads on the same app-server connection, so their
    /// notifications arrive here too: the thread id when it is not this chat's own. Their turns
    /// must not replace this thread's turn id (`turn/interrupt` would fail with "expected active
    /// turn id ... but found ...") or end it.
    fn other_thread(&self, p: &Value) -> Option<String> {
        match (p.get("threadId").and_then(|t| t.as_str()), self.thread.as_deref()) {
            (Some(t), Some(own)) if t != own => Some(t.to_string()),
            _ => None,
        }
    }

    /// Display name of a sub-agent (nickname, else role) by thread id.
    fn sub_name(&self, tid: &str) -> Option<String> {
        let sub = self.subs.get(tid).and_then(|c| self.cards.get(c)).and_then(|c| c.subagent.as_ref())?;
        sub.name.clone().or_else(|| sub.role.clone())
    }

    /// Streaming deltas: (item, field, text).
    fn delta(method: &str, p: &Value) -> Option<(String, DeltaField, String)> {
        let field = match method {
            "item/agentMessage/delta" | "item/plan/delta" | "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => DeltaField::Text,
            "item/reasoning/summaryPartAdded" => return Some((s(p, "itemId")?, DeltaField::Text, "\n\n".into())),
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => DeltaField::Output,
            _ => return None,
        };
        Some((s(p, "itemId")?, field, s(p, "delta")?))
    }

    /// Notifications of a sub-agent's thread: its items go to its read-only view (`thread`),
    /// its turns drive its card.
    fn on_sub_notification(&mut self, tid: &str, method: &str, p: Value, out: &mut Out) {
        if let Some((item, field, delta)) = Self::delta(method, &p) {
            out.events.push(AdapterEvent::Delta { item, field, delta, thread: Some(tid.to_string()) });
            return;
        }
        match method {
            "turn/started" => self.update_card(tid, out, |c| {
                if c.status != SubagentStatus::Closed {
                    c.status = SubagentStatus::Running;
                }
            }),
            "turn/completed" => {
                let turn = p.get("turn").cloned().unwrap_or(Value::Null);
                let status = s(&turn, "status").unwrap_or_default();
                if status == "failed" {
                    let msg = turn.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).unwrap_or("turn failed").to_string();
                    let mut it = item(format!("turnerr-{}", now_ms()), ChatItemKind::Error, ItemStatus::Failed);
                    it.text = Some(msg);
                    it.thread = Some(tid.to_string());
                    out.events.push(AdapterEvent::Item(it));
                }
                let open: Vec<String> = self
                    .items
                    .iter()
                    .filter(|(_, i)| i.status == ItemStatus::InProgress && i.thread.as_deref() == Some(tid))
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in open {
                    if let Some(mut it) = self.items.remove(&k) {
                        it.status = if status == "interrupted" { ItemStatus::Declined } else { ItemStatus::Completed };
                        out.events.push(AdapterEvent::Item(it));
                    }
                }
                let reply = self.sub_text.get(tid).cloned();
                self.update_card(tid, out, |c| {
                    if c.status != SubagentStatus::Closed {
                        c.status = match status.as_str() {
                            "failed" => SubagentStatus::Failed,
                            "interrupted" => SubagentStatus::Interrupted,
                            _ => SubagentStatus::Done,
                        };
                    }
                    if reply.is_some() {
                        c.reply = reply;
                    }
                });
            }
            "item/started" | "item/completed" => {
                let completed = method == "item/completed";
                let Some(it) = p.get("item") else { return };
                if s(it, "type").as_deref() == Some("collabAgentToolCall") {
                    self.collab(it, completed, Some(tid), out);
                    return;
                }
                let Some(mut ci) = self.map_item(it, completed) else { return };
                ci.thread = Some(tid.to_string());
                if completed {
                    self.items.remove(&ci.id);
                    if ci.kind == ChatItemKind::Agent {
                        if let Some(t) = ci.text.clone().filter(|t| !t.trim().is_empty()) {
                            self.sub_text.insert(tid.to_string(), t);
                        }
                    }
                } else {
                    self.items.insert(ci.id.clone(), ci.clone());
                }
                out.events.push(AdapterEvent::Item(ci));
            }
            "thread/status/changed" => {
                let st = p.get("status").cloned().unwrap_or(Value::Null);
                let flags = st.get("activeFlags").and_then(|f| f.as_array()).cloned().unwrap_or_default();
                if flags.iter().any(|f| f.as_str() == Some("waitingOnApproval")) {
                    out.events.push(AdapterEvent::Status { status: ChatStatus::AwaitingApproval, detail: None });
                }
                if s(&st, "type").as_deref() == Some("systemError") {
                    self.update_card(tid, out, |c| c.status = SubagentStatus::Failed);
                }
            }
            "error" if !p.get("willRetry").and_then(|w| w.as_bool()).unwrap_or(false) => {
                let msg = p.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).map(str::to_string).unwrap_or_else(|| "error".into());
                let mut it = item(format!("err-{}", now_ms()), ChatItemKind::Error, ItemStatus::Failed);
                it.text = Some(msg);
                it.thread = Some(tid.to_string());
                out.events.push(AdapterEvent::Item(it));
            }
            // MCP startup, warnings, token usage of the sub-agent: not this chat's.
            _ => {}
        }
    }

    /// Changes the card of the sub-agent with thread `tid` and sends it when it changed.
    fn update_card(&mut self, tid: &str, out: &mut Out, f: impl FnOnce(&mut yonder_proto::app::Subagent)) {
        let Some(card) = self.subs.get(tid).and_then(|c| self.cards.get_mut(c)) else { return };
        let Some(sub) = card.subagent.as_mut() else { return };
        let before = (sub.clone(), card.status);
        f(sub);
        let status = sub.status;
        set_subagent_status(card, status);
        if card.subagent.as_ref() != Some(&before.0) || card.status != before.1 {
            out.events.push(AdapterEvent::Item(card.clone()));
        }
    }

    /// A `collabAgentToolCall` item: `spawnAgent` creates a card, every other call (wait,
    /// send_input, close_agent, resume_agent, ...) only updates the cards of its targets.
    /// `thread`: the sub-agent that made the call (a nested spawn), None for this chat.
    fn collab(&mut self, it: &Value, completed: bool, thread: Option<&str>, out: &mut Out) {
        let tool = s(it, "tool").unwrap_or_default();
        let call_failed = matches!(s(it, "status").as_deref(), Some("failed"));
        let receivers: Vec<String> = it.get("receiverThreadIds").and_then(|r| r.as_array()).into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
        if tool == "spawnAgent" {
            let Some(id) = s(it, "id") else { return };
            let card = self.cards.entry(id.clone()).or_insert_with(|| {
                let mut c = subagent_card(&id, "", SubagentStatus::Running);
                c.thread = thread.map(str::to_string);
                c
            });
            let before = (card.subagent.clone(), card.text.clone(), card.status);
            if let Some(p) = s(it, "prompt").filter(|p| !p.trim().is_empty()) {
                card.text = Some(p);
            }
            let mut new_thread = None;
            if let Some(sub) = card.subagent.as_mut() {
                if let Some(m) = s(it, "model").filter(|m| !m.is_empty()) {
                    sub.model = Some(m);
                }
                if let Some(r) = receivers.first().filter(|r| sub.id != **r) {
                    sub.id = r.clone();
                    new_thread = Some(r.clone());
                }
            }
            if completed && call_failed && receivers.is_empty() {
                set_subagent_status(card, SubagentStatus::Failed);
            }
            let changed = before != (card.subagent.clone(), card.text.clone(), card.status);
            let card = card.clone();
            if let Some(r) = new_thread {
                self.subs.insert(r.clone(), id);
                // Nickname and role are only on the sub-agent's thread.
                let m = self.request("thread/read", json!({"threadId": r}));
                if let Some(n) = m.get("id").and_then(|i| i.as_u64()) {
                    self.pending.insert(n, Pending::SubRead(r));
                }
                out.to_agent.push(m);
            }
            if changed {
                out.events.push(AdapterEvent::Item(card));
            }
        }
        // Every call reports the latest known state of its targets.
        if let Some(states) = it.get("agentsStates").and_then(|a| a.as_object()) {
            for (tid, st) in states {
                let status = match s(st, "status").as_deref() {
                    Some("pendingInit" | "running") => SubagentStatus::Running,
                    Some("completed") => SubagentStatus::Done,
                    Some("errored") => SubagentStatus::Failed,
                    Some("interrupted") => SubagentStatus::Interrupted,
                    Some("shutdown") => SubagentStatus::Closed,
                    _ => continue,
                };
                let reply = s(st, "message").filter(|m| !m.trim().is_empty());
                self.update_card(tid, out, |c| {
                    // Closed stays closed until the agent is resumed.
                    if c.status != SubagentStatus::Closed || tool == "resumeAgent" {
                        c.status = status;
                    }
                    if reply.is_some() {
                        c.reply = reply;
                    }
                });
            }
        }
        match tool.as_str() {
            "closeAgent" if completed && !call_failed => {
                for r in &receivers {
                    self.update_card(r, out, |c| c.status = SubagentStatus::Closed);
                }
            }
            "sendInput" | "sendMessage" | "followupTask" | "resumeAgent" if !completed => {
                for r in &receivers {
                    self.update_card(r, out, |c| c.status = SubagentStatus::Running);
                }
            }
            _ => {}
        }
    }

    fn on_notification(&mut self, method: &str, p: Value, out: &mut Out) {
        if let Some(tid) = self.other_thread(&p).filter(|_| method != "serverRequest/resolved") {
            self.on_sub_notification(&tid, method, p, out);
            return;
        }
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
                // Anything still in progress is done now (sub-agents may outlive the turn).
                let open: Vec<String> = self.items.iter().filter(|(_, i)| i.status == ItemStatus::InProgress && i.thread.is_none()).map(|(k, _)| k.clone()).collect();
                for k in open {
                    if let Some(mut it) = self.items.remove(&k) {
                        it.status = if status == "interrupted" { ItemStatus::Declined } else { ItemStatus::Completed };
                        out.events.push(AdapterEvent::Item(it));
                    }
                }
                self.items.retain(|_, i| i.thread.is_some());
                out.events.push(AdapterEvent::Status { status: ChatStatus::Idle, detail: if status == "interrupted" { Some("interrupted".into()) } else { None } });
                // A message that could not steer the finished turn starts the next one.
                self.flush_held(out);
            }
            "item/started" | "item/completed" => {
                let completed = method == "item/completed";
                if let Some(it) = p.get("item") {
                    if s(it, "type").as_deref() == Some("collabAgentToolCall") {
                        self.collab(it, completed, None, out);
                        return;
                    }
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
            m if Self::delta(m, &p).is_some() => {
                if let Some((item, field, delta)) = Self::delta(m, &p) {
                    out.events.push(AdapterEvent::Delta { item, field, delta, thread: None });
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

    /// History items of resumed turns. Sub-agent cards come with the last state Codex
    /// recorded; the sub-agents' own threads are fetched for their read-only views.
    fn replay(&mut self, turns: &[Value], out: &mut Out) {
        let known: std::collections::HashSet<String> = self.subs.keys().cloned().collect();
        for turn in turns {
            for it in turn.get("items").and_then(|i| i.as_array()).into_iter().flatten() {
                if s(it, "type").as_deref() == Some("collabAgentToolCall") {
                    self.collab(it, true, None, out);
                } else if let Some(ci) = self.map_item(it, true) {
                    out.events.push(AdapterEvent::Item(ci));
                }
            }
        }
        let mut fresh: Vec<String> = self.subs.keys().filter(|t| !known.contains(*t)).cloned().collect();
        fresh.sort();
        // Sub-agents of an earlier app-server are gone: one still marked running was cut off.
        for tid in &fresh {
            self.update_card(tid, out, |c| {
                if c.status == SubagentStatus::Running {
                    c.status = SubagentStatus::Interrupted;
                }
            });
        }
        // Thread ids are time-ordered (UUIDv7): fetch the newest few.
        for tid in fresh.into_iter().rev().take(REPLAY_SUBAGENTS) {
            let m = self.request("thread/turns/list", json!({"threadId": tid, "limit": REPLAY_TURNS, "itemsView": "full"}));
            if let Some(n) = m.get("id").and_then(|i| i.as_u64()) {
                self.pending.insert(n, Pending::SubTurns(tid));
            }
            out.to_agent.push(m);
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

    /// A sub-agent's turns arrive on the parent's connection: they must not replace the
    /// parent's turn, end it, or be what Stop interrupts.
    #[test]
    fn sub_agent_turns_do_not_touch_the_parent_turn() {
        let mut st = CodexState::new(launch());
        st.thread = Some("parent".into());
        st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/started", "params": {"threadId": "parent", "turn": {"id": "turn-p"}}}));
        st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/started", "params": {"threadId": "child", "turn": {"id": "turn-c"}}}));
        assert_eq!(st.turn.as_deref(), Some("turn-p"));

        let out = st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"threadId": "child", "turn": {"id": "turn-c", "status": "completed"}}}));
        assert!(out.events.is_empty(), "{:?}", out.events);
        assert!(st.busy);

        let out = st.on_cmd(AdapterCmd::Interrupt);
        assert_eq!(out.to_agent[0]["method"], "turn/interrupt");
        assert_eq!(out.to_agent[0]["params"]["threadId"], "parent");
        assert_eq!(out.to_agent[0]["params"]["turnId"], "turn-p");
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

    /// Final state of every item (by id, in first-seen order), deltas applied.
    fn final_items(evs: &[AdapterEvent]) -> Vec<ChatItem> {
        let mut order: Vec<String> = Vec::new();
        let mut map: HashMap<String, ChatItem> = HashMap::new();
        for e in evs {
            match e {
                AdapterEvent::Item(i) => {
                    if !map.contains_key(&i.id) {
                        order.push(i.id.clone());
                    }
                    map.insert(i.id.clone(), i.clone());
                }
                AdapterEvent::Delta { item, field, delta, thread } => {
                    let it = map.entry(item.clone()).or_insert_with(|| {
                        order.push(item.clone());
                        let mut c = ChatItem::new(item.clone(), ChatItemKind::Agent, ItemStatus::InProgress, 0);
                        c.thread = thread.clone();
                        c
                    });
                    assert_eq!(&it.thread, thread, "delta for {item} in another thread");
                    match field {
                        DeltaField::Text => it.text.get_or_insert_with(String::new).push_str(delta),
                        DeltaField::Output => it.output.get_or_insert_with(String::new).push_str(delta),
                    }
                }
                _ => {}
            }
        }
        order.into_iter().filter_map(|id| map.remove(&id)).collect()
    }

    /// Real run (codex-cli 0.154): the parent spawns one sub-agent, waits for it, closes it. The
    /// sub-agent asks to run a command outside the sandbox.
    #[test]
    fn subagent_card_thread_and_approval_from_recording() {
        let (st, evs) = replay("codex_subagent_approval.jsonl");
        let parent = st.thread.clone().unwrap();
        let child = "01a115ca-eec7-75a3-9ebb-d0408cc774d4";
        let items = final_items(&evs);

        // One card for the spawn; wait / close only update it.
        let cards: Vec<&ChatItem> = items.iter().filter(|i| i.kind == ChatItemKind::Subagent).collect();
        assert_eq!(cards.len(), 1, "{cards:?}");
        let card = cards[0];
        assert!(card.thread.is_none());
        let sub = card.subagent.as_ref().unwrap();
        assert_eq!(sub.id, child);
        assert_eq!(sub.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(sub.status, SubagentStatus::Closed);
        assert_eq!(sub.reply.as_deref(), Some("DONE"));
        assert_eq!(card.status, ItemStatus::Completed);
        assert!(card.text.as_deref().unwrap().starts_with("Run the shell command: touch"));
        // The card went through running before it ended.
        assert!(evs.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.id == card.id && i.subagent.as_ref().unwrap().status == SubagentStatus::Running)));
        assert!(!items.iter().any(|i| i.kind == ChatItemKind::Tool), "collab calls are not tool rows: {items:?}");

        // The sub-agent's own items belong to its thread, the parent's to the chat.
        let child_items: Vec<&ChatItem> = items.iter().filter(|i| i.thread.as_deref() == Some(child)).collect();
        assert!(child_items.iter().any(|i| i.kind == ChatItemKind::User && i.text.as_deref().unwrap().contains("touch")));
        let cmd = child_items.iter().find(|i| i.kind == ChatItemKind::Command).expect("sub-agent command");
        assert_eq!(cmd.status, ItemStatus::Completed);
        assert!(child_items.iter().any(|i| i.kind == ChatItemKind::Agent && i.text.as_deref() == Some("DONE")));
        assert!(child_items.iter().any(|i| i.kind == ChatItemKind::Reasoning));
        let own: Vec<&ChatItem> = items.iter().filter(|i| i.thread.is_none() && i.kind == ChatItemKind::Agent).collect();
        assert_eq!(own.len(), 1, "{own:?}");
        assert!(items.iter().all(|i| i.thread.is_none() || i.thread.as_deref() == Some(child)));

        // The approval reaches the chat, attributed to the sub-agent.
        let approval = evs
            .iter()
            .find_map(|e| match e {
                AdapterEvent::ApprovalRequested(a) => Some(a.clone()),
                _ => None,
            })
            .expect("sub-agent approval");
        assert_eq!(approval.thread.as_deref(), Some(child));
        assert_eq!(approval.kind, ApprovalKind::Command);
        assert_eq!(approval.command.as_deref(), Some("/bin/bash -lc 'touch /tmp/work/cx1/outside/sub_approval.txt'"));
        assert_ne!(approval.thread.as_deref(), Some(parent.as_str()));
        // Codex resolved it (the recorder answered): it is gone.
        assert!(evs.iter().any(|e| matches!(e, AdapterEvent::ApprovalResolved { approval: a, .. } if *a == approval.id)));
        assert!(st.approvals.is_empty());

        // The sub-agent's turn did not end the parent's: idle only once, at the very end.
        let idles = evs.iter().filter(|e| matches!(e, AdapterEvent::Status { status: ChatStatus::Idle, .. })).count();
        assert_eq!(idles, 2, "thread start + parent turn end");
        assert!(matches!(evs.last(), Some(AdapterEvent::Status { status: ChatStatus::Idle, .. })));
    }

    /// The spawn asks Codex for the sub-agent's thread; its nickname names the card.
    #[test]
    fn subagent_nickname_from_thread_read() {
        let path = format!("{}/tests/fixtures/codex_subagent_read.jsonl", env!("CARGO_MANIFEST_DIR"));
        let lines: Vec<Value> = std::fs::read_to_string(path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        // The recorder's own `thread/read` answer (id 4) stands in for ours.
        let read = lines
            .iter()
            .filter_map(|o| o.get("raw").and_then(|r| r.as_str()))
            .filter_map(|r| serde_json::from_str::<Value>(r).ok())
            .find(|v| v.get("id") == Some(&json!(4)) && v.get("result").is_some())
            .unwrap();
        let mut st = CodexState::new(launch());
        st.pending.insert(1, Pending::Initialize);
        st.next_id = 100;
        let mut asked = None;
        let mut evs = Vec::new();
        for o in &lines {
            let Some(raw) = o.get("raw").and_then(|r| r.as_str()) else { continue };
            let v: Value = serde_json::from_str(raw).unwrap();
            match v.get("id").and_then(|i| i.as_u64()) {
                Some(2) if v.get("method").is_none() => {
                    st.pending.insert(2, Pending::ThreadStart);
                }
                Some(3) if v.get("method").is_none() => {
                    st.pending.insert(3, Pending::TurnStart);
                }
                Some(4 | 5) if v.get("method").is_none() => continue,
                _ => {}
            }
            let out = st.on_message(&v);
            if let Some(m) = out.to_agent.iter().find(|m| m["method"] == "thread/read") {
                asked = Some(m.clone());
            }
            evs.extend(out.events);
        }
        let asked = asked.expect("thread/read for the sub-agent");
        assert_eq!(asked["params"]["threadId"], "01a115ff-bc19-76a0-95f5-72544357b88b");
        let mut answer = read.clone();
        answer["id"] = asked["id"].clone();
        let out = st.on_message(&answer);
        let card = out
            .events
            .iter()
            .find_map(|e| match e {
                AdapterEvent::Item(i) if i.kind == ChatItemKind::Subagent => Some(i.clone()),
                _ => None,
            })
            .expect("card updated");
        let sub = card.subagent.unwrap();
        assert_eq!(sub.name.as_deref(), Some("Linnaeus"));
        // It was closed meanwhile; naming it keeps that.
        assert_eq!(sub.status, SubagentStatus::Closed);
        assert_eq!(sub.reply.as_deref(), Some("DONE"));
        // A sub-agent approval names it once the name is known.
        assert_eq!(st.sub_name("01a115ff-bc19-76a0-95f5-72544357b88b").as_deref(), Some("Linnaeus"));
    }

    fn spawn_item(id: &str, status: &str, receivers: &[&str], states: Value) -> Value {
        json!({"type": "collabAgentToolCall", "id": id, "tool": "spawnAgent", "status": status, "senderThreadId": "parent",
            "receiverThreadIds": receivers, "prompt": "look around", "model": "m1", "reasoningEffort": "low", "agentsStates": states})
    }

    /// A sub-agent still running keeps the app-server (it would die with it); answering its
    /// approval after the parent turn ended leaves the chat idle, not working.
    #[test]
    fn running_subagent_blocks_idle_release_and_approval_after_turn() {
        let mut st = CodexState::new(launch());
        st.thread = Some("parent".into());
        st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/started", "params": {"threadId": "parent", "turn": {"id": "tp"}}}));
        let out = st.on_message(&json!({"jsonrpc": "2.0", "method": "item/completed", "params": {"threadId": "parent", "item": spawn_item("sp1", "completed", &["kid"], json!({"kid": {"status": "pendingInit", "message": null}}))}}));
        let read = out.to_agent.iter().find(|m| m["method"] == "thread/read").unwrap();
        st.on_message(&json!({"id": read["id"].clone(), "result": {"thread": {"id": "kid", "agentNickname": "Ada", "agentRole": "explorer"}}}));
        st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"threadId": "parent", "turn": {"id": "tp", "status": "completed"}}}));
        assert!(!st.busy);
        assert!(!st.can_release(), "sub-agent still running");

        let out = st.on_message(&json!({"jsonrpc": "2.0", "id": 7, "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "kid", "turnId": "tk", "itemId": "c1", "command": "ls", "cwd": "/tmp"}}));
        let a = out.events.iter().find_map(|e| match e { AdapterEvent::ApprovalRequested(a) => Some(a.clone()), _ => None }).unwrap();
        assert_eq!(a.thread.as_deref(), Some("kid"));
        assert_eq!(a.thread_name.as_deref(), Some("Ada"));
        let out = st.on_cmd(AdapterCmd::Approve { approval_id: a.id, option_id: "allow".into() });
        assert_eq!(out.to_agent[0]["result"]["decision"], "accept");
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Status { status: ChatStatus::Idle, .. })));

        let out = st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"threadId": "kid", "turn": {"id": "tk", "status": "completed"}}}));
        let card = out.events.iter().find_map(|e| match e { AdapterEvent::Item(i) => Some(i.clone()), _ => None }).unwrap();
        assert_eq!(card.subagent.unwrap().status, SubagentStatus::Done);
        assert!(st.can_release());
    }

    /// Sub-agent items carry their thread through deltas and turn ends, and its failed turn
    /// marks the card failed.
    #[test]
    fn subagent_deltas_and_failure() {
        let mut st = CodexState::new(launch());
        st.thread = Some("parent".into());
        st.on_message(&json!({"jsonrpc": "2.0", "method": "item/completed", "params": {"threadId": "parent", "item": spawn_item("sp1", "completed", &["kid"], json!({}))}}));
        let out = st.on_message(&json!({"jsonrpc": "2.0", "method": "item/commandExecution/outputDelta", "params": {"threadId": "kid", "itemId": "c1", "delta": "x"}}));
        assert!(matches!(&out.events[0], AdapterEvent::Delta { thread: Some(t), field: DeltaField::Output, .. } if t == "kid"));
        st.on_message(&json!({"jsonrpc": "2.0", "method": "item/started", "params": {"threadId": "kid", "item": {"type": "commandExecution", "id": "c1", "command": "sleep 9", "status": "inProgress"}}}));
        let out = st.on_message(&json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"threadId": "kid", "turn": {"id": "tk", "status": "failed", "error": {"message": "boom"}}}}));
        let items = final_items(&out.events);
        assert!(items.iter().any(|i| i.kind == ChatItemKind::Error && i.thread.as_deref() == Some("kid")));
        assert!(items.iter().any(|i| i.id == "c1" && i.thread.as_deref() == Some("kid") && i.status == ItemStatus::Completed));
        let card = items.iter().find(|i| i.kind == ChatItemKind::Subagent).unwrap();
        assert_eq!(card.subagent.as_ref().unwrap().status, SubagentStatus::Failed);
        assert_eq!(card.status, ItemStatus::Failed);
        // The parent is untouched.
        assert!(!out.events.iter().any(|e| matches!(e, AdapterEvent::Status { .. })));
    }

    /// Resuming a thread shows its sub-agent cards with their last state and fetches the
    /// sub-agents' threads for their views. A card left running was cut off by the old
    /// app-server.
    #[test]
    fn resume_replays_subagent_cards() {
        let mut l = launch();
        l.resume = Some("parent".into());
        let mut st = CodexState::new(l);
        st.thread = Some("parent".into());
        let turns = vec![json!({"id": "t1", "items": [
            {"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "go"}]},
            spawn_item("sp1", "completed", &["kid1"], json!({"kid1": {"status": "pendingInit"}})),
            spawn_item("sp2", "completed", &["kid2"], json!({"kid2": {"status": "pendingInit"}})),
            {"type": "collabAgentToolCall", "id": "w1", "tool": "wait", "status": "completed", "senderThreadId": "parent", "receiverThreadIds": ["kid1"], "agentsStates": {"kid1": {"status": "completed", "message": "found it"}}},
        ]})];
        let mut out = Out::default();
        st.replay(&turns, &mut out);
        let items = final_items(&out.events);
        let cards: Vec<&ChatItem> = items.iter().filter(|i| i.kind == ChatItemKind::Subagent).collect();
        assert_eq!(cards.len(), 2);
        let s1 = cards[0].subagent.as_ref().unwrap();
        assert_eq!((s1.status, s1.reply.as_deref()), (SubagentStatus::Done, Some("found it")));
        assert_eq!(cards[1].subagent.as_ref().unwrap().status, SubagentStatus::Interrupted);
        let lists: Vec<&Value> = out.to_agent.iter().filter(|m| m["method"] == "thread/turns/list").collect();
        assert_eq!(lists.len(), 2);
        // Their answer lands in the sub-agent's thread.
        let id = lists[0]["id"].clone();
        let tid = lists[0]["params"]["threadId"].as_str().unwrap().to_string();
        let out = st.on_message(&json!({"id": id, "result": {"data": [{"id": "tk", "items": [{"type": "agentMessage", "id": "m9", "text": "hi"}]}]}}));
        let items = final_items(&out.events);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].thread.as_deref(), Some(tid.as_str()));
    }
}
