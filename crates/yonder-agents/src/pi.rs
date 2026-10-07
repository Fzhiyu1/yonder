//! pi adapter: `pi --mode rpc` (JSONL commands on stdin, responses and events on stdout).
//!
//! pi does not gate tool execution (no approvals). Extension dialogs
//! (`extension_ui_request` select / confirm / input / editor) are surfaced as
//! question-type approvals; fire-and-forget UI requests are ignored.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Value};
use yonder_proto::app::{
    AgentKind, Approval, ApprovalKind, ApprovalOption, ChatItem, ChatItemKind, ChatStatus, DeltaField, ItemStatus,
    OptionKind,
};

use crate::claude::image_mime;
use crate::common::{item, now_ms, random_id, strip_ansi, truncate_tail, MAX_OUTPUT};
use crate::driver::{run, Out, Protocol};
use crate::{AdapterCmd, AdapterEvent, AdapterHandle, AgentLaunch};

pub fn argv(launch: &AgentLaunch) -> Vec<String> {
    let mut argv = launch.program.clone().unwrap_or_else(|| vec!["pi".into()]);
    argv.extend(["--mode".into(), "rpc".into()]);
    if let Some(m) = launch.model.as_ref().filter(|m| !m.is_empty()) {
        argv.extend(["--model".into(), m.clone()]);
    }
    if let Some(r) = launch.resume.as_ref().filter(|r| !r.is_empty()) {
        argv.extend(["--session".into(), r.clone()]);
    }
    argv
}

pub fn spawn(launch: AgentLaunch) -> Result<AdapterHandle> {
    let st = PiState::new(launch.clone());
    run(AgentKind::Pi, &launch, argv(&launch), st)
}

struct UiRequest {
    method: String,
    choices: Vec<String>,
}

pub struct PiState {
    next_id: u64,
    ready: bool,
    queued: Vec<(String, Vec<std::path::PathBuf>)>,
    busy: bool,
    session: Option<String>,
    model: Option<String>,
    /// Assistant message counter (item ids).
    msg_seq: u64,
    /// contentIndex -> item id for the current assistant message.
    blocks: HashMap<u64, String>,
    open: HashMap<String, ChatItem>,
    tool_started: HashMap<String, u64>,
    ui: HashMap<String, UiRequest>,
    aborting: bool,
    exit_error: Option<String>,
    resumed: bool,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string)
}

fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| s(p, "text")).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

fn tool_item(id: &str, name: &str, args: &Value, status: ItemStatus) -> ChatItem {
    let lname = name.to_ascii_lowercase();
    match lname.as_str() {
        "bash" => {
            let mut it = item(id, ChatItemKind::Command, status);
            it.title = s(args, "command").or_else(|| Some(name.to_string()));
            it
        }
        "edit" | "write" => {
            let mut it = item(id, ChatItemKind::FileChange, status);
            let path = s(args, "path").or_else(|| s(args, "file_path")).unwrap_or_default();
            it.title = Some(if path.is_empty() { name.to_string() } else { path.clone() });
            if !path.is_empty() {
                it.paths = vec![path.clone()];
            }
            if args.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                return it;
            }
            let mut diff = format!("--- a/{path}\n+++ b/{path}\n");
            let mut hunk = |old: &str, new: &str| {
                diff.push_str(&format!("@@ -1,{} +1,{} @@\n", old.lines().count(), new.lines().count()));
                for l in old.lines() {
                    diff.push_str(&format!("-{l}\n"));
                }
                for l in new.lines() {
                    diff.push_str(&format!("+{l}\n"));
                }
            };
            if lname == "write" {
                hunk("", &s(args, "content").unwrap_or_default());
            } else {
                let edits: Vec<Value> = match args.get("edits").and_then(|e| e.as_array()) {
                    Some(a) => a.clone(),
                    None => vec![args.clone()],
                };
                for e in edits {
                    let old = s(&e, "oldText").or_else(|| s(&e, "old_string")).unwrap_or_default();
                    let new = s(&e, "newText").or_else(|| s(&e, "new_string")).unwrap_or_default();
                    hunk(&old, &new);
                }
            }
            it.diff = Some(truncate_tail(&diff, MAX_OUTPUT * 2));
            it
        }
        _ => {
            let mut it = item(id, ChatItemKind::Tool, status);
            let arg = ["path", "pattern", "query", "url", "command"].iter().find_map(|k| s(args, k));
            it.title = Some(match arg {
                Some(a) => format!("{name} {a}"),
                None => name.to_string(),
            });
            if args.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                it.text = Some(serde_json::to_string_pretty(args).unwrap_or_default());
            }
            it
        }
    }
}

impl PiState {
    pub fn new(launch: AgentLaunch) -> Self {
        Self {
            next_id: 1,
            ready: false,
            queued: Vec::new(),
            busy: false,
            session: None,
            model: None,
            msg_seq: 0,
            blocks: HashMap::new(),
            open: HashMap::new(),
            tool_started: HashMap::new(),
            ui: HashMap::new(),
            aborting: false,
            exit_error: None,
            resumed: launch.resume.is_some(),
        }
    }

    fn cmd(&mut self, ty: &str, mut extra: Value) -> Value {
        let id = format!("y{}", self.next_id);
        self.next_id += 1;
        extra["id"] = json!(id);
        extra["type"] = json!(ty);
        extra
    }

    fn prompt(&mut self, text: String, attachments: Vec<std::path::PathBuf>) -> Value {
        use base64::Engine;
        let mut msg = text;
        let mut images = Vec::new();
        for a in &attachments {
            if let Some(mime) = image_mime(a) {
                if let Ok(bytes) = std::fs::read(a) {
                    images.push(json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(bytes), "mimeType": mime}));
                    continue;
                }
            }
            msg.push_str(&format!("\n@{}", a.display()));
        }
        let mut body = json!({"message": msg});
        if !images.is_empty() {
            body["images"] = json!(images);
        }
        if self.busy {
            body["streamingBehavior"] = json!("steer");
        }
        self.busy = true;
        self.cmd("prompt", body)
    }

    fn close_open(&mut self, out: &mut Out, status: ItemStatus) {
        let mut open: Vec<ChatItem> = self.open.drain().map(|(_, v)| v).collect();
        open.sort_by_key(|i| i.ts);
        for mut it in open {
            if it.status == ItemStatus::InProgress {
                it.status = status;
                out.event(AdapterEvent::Item(it));
            }
        }
        self.blocks.clear();
    }

    fn set_model(&mut self, full: String, out: &mut Out) {
        if !full.is_empty() && self.model.as_deref() != Some(full.as_str()) {
            self.model = Some(full.clone());
            out.event(AdapterEvent::Model(full));
        }
    }

    fn on_state(&mut self, data: &Value, out: &mut Out) {
        if let Some(sid) = s(data, "sessionId") {
            if self.session.as_deref() != Some(sid.as_str()) {
                self.session = Some(sid.clone());
                out.event(AdapterEvent::AgentSession(sid));
            }
        }
        if let Some(m) = data.get("model") {
            let id = s(m, "id").unwrap_or_default();
            let full = match s(m, "provider") {
                Some(p) if !id.is_empty() => format!("{p}/{id}"),
                _ => id,
            };
            self.set_model(full, out);
        }
    }

    fn on_history(&mut self, msgs: &[Value], out: &mut Out) {
        let mut tools: Vec<(String, ChatItem)> = Vec::new();
        for (n, m) in msgs.iter().enumerate() {
            match s(m, "role").as_deref() {
                Some("user") => {
                    let mut it = item(format!("pi-h{n}"), ChatItemKind::User, ItemStatus::Completed);
                    it.text = Some(text_of(m.get("content")));
                    out.event(AdapterEvent::Item(it));
                }
                Some("assistant") => {
                    for (i, c) in m.get("content").and_then(|c| c.as_array()).into_iter().flatten().enumerate() {
                        match s(c, "type").as_deref() {
                            Some("text") => {
                                let mut it = item(format!("pi-h{n}-{i}"), ChatItemKind::Agent, ItemStatus::Completed);
                                it.text = s(c, "text");
                                out.event(AdapterEvent::Item(it));
                            }
                            Some("toolCall") => {
                                let id = s(c, "id").unwrap_or_else(|| format!("pi-h{n}-{i}"));
                                let args = c.get("arguments").cloned().unwrap_or(json!({}));
                                let it = tool_item(&id, &s(c, "name").unwrap_or_default(), &args, ItemStatus::Completed);
                                out.event(AdapterEvent::Item(it.clone()));
                                tools.push((id, it));
                            }
                            _ => {}
                        }
                    }
                }
                Some("toolResult") => {
                    let Some(id) = s(m, "toolCallId") else { continue };
                    if let Some((_, mut it)) = tools.iter().find(|(t, _)| *t == id).cloned() {
                        it.output = Some(truncate_tail(&text_of(m.get("content")), MAX_OUTPUT));
                        if m.get("isError").and_then(|b| b.as_bool()).unwrap_or(false) {
                            it.status = ItemStatus::Failed;
                        }
                        out.event(AdapterEvent::Item(it));
                    }
                }
                _ => {}
            }
        }
    }

    fn on_message_update(&mut self, v: &Value, out: &mut Out) {
        let Some(ev) = v.get("assistantMessageEvent") else { return };
        let idx = ev.get("contentIndex").and_then(|i| i.as_u64()).unwrap_or(0);
        let ty = s(ev, "type").unwrap_or_default();
        match ty.as_str() {
            "text_start" | "thinking_start" => {
                let id = format!("pi-{}-{}-{idx}", self.session.as_deref().unwrap_or("s"), self.msg_seq);
                let kind = if ty == "text_start" { ChatItemKind::Agent } else { ChatItemKind::Reasoning };
                let mut it = item(&id, kind, ItemStatus::InProgress);
                it.text = Some(String::new());
                self.blocks.insert(idx, id.clone());
                self.open.insert(id, it.clone());
                out.event(AdapterEvent::Item(it));
            }
            "text_delta" | "thinking_delta" => {
                let Some(id) = self.blocks.get(&idx).cloned() else { return };
                let d = s(ev, "delta").unwrap_or_default();
                if let Some(it) = self.open.get_mut(&id) {
                    it.text.get_or_insert_with(String::new).push_str(&d);
                }
                out.event(AdapterEvent::Delta { item: id, field: DeltaField::Text, delta: d, thread: None });
            }
            "text_end" | "thinking_end" => {
                let Some(id) = self.blocks.remove(&idx) else { return };
                if let Some(mut it) = self.open.remove(&id) {
                    if let Some(c) = s(ev, "content") {
                        it.text = Some(c);
                    }
                    it.status = ItemStatus::Completed;
                    out.event(AdapterEvent::Item(it));
                }
            }
            "toolcall_start" => {
                let id = s(ev, "id").unwrap_or_else(|| format!("pi-{}-{idx}", self.msg_seq));
                let it = tool_item(&id, &s(ev, "toolName").unwrap_or_default(), &json!({}), ItemStatus::InProgress);
                self.blocks.insert(idx, id.clone());
                self.open.insert(id, it.clone());
                out.event(AdapterEvent::Item(it));
            }
            "toolcall_end" => {
                self.blocks.remove(&idx);
                if let Some(tc) = ev.get("toolCall") {
                    let id = s(tc, "id").unwrap_or_default();
                    let args = tc.get("arguments").cloned().unwrap_or(json!({}));
                    let it = tool_item(&id, &s(tc, "name").unwrap_or_default(), &args, ItemStatus::InProgress);
                    self.open.insert(id, it.clone());
                    out.event(AdapterEvent::Item(it));
                }
            }
            _ => {}
        }
    }

    fn on_ui_request(&mut self, v: &Value, out: &mut Out) {
        let id = s(v, "id").unwrap_or_default();
        let method = s(v, "method").unwrap_or_default();
        match method.as_str() {
            "notify" => {
                if s(v, "notifyType").as_deref() == Some("error") {
                    let mut it = item(format!("pi-note-{}", random_id()), ChatItemKind::System, ItemStatus::Completed);
                    it.text = Some(strip_ansi(&s(v, "message").unwrap_or_default()));
                    out.event(AdapterEvent::Item(it));
                }
            }
            "select" | "confirm" | "input" | "editor" => {
                let choices: Vec<String> = if method == "select" {
                    v.get("options").and_then(|o| o.as_array()).into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
                } else {
                    Vec::new()
                };
                let mut options: Vec<ApprovalOption> = match method.as_str() {
                    "select" => choices
                        .iter()
                        .enumerate()
                        .map(|(i, c)| ApprovalOption { id: format!("choice:{i}"), label: c.clone(), kind: OptionKind::Choice })
                        .collect(),
                    "confirm" => vec![ApprovalOption { id: "allow".into(), label: "确认".into(), kind: OptionKind::Allow }],
                    _ => vec![ApprovalOption { id: "allow".into(), label: "使用默认值".into(), kind: OptionKind::Allow }],
                };
                options.push(ApprovalOption { id: "deny".into(), label: "取消".into(), kind: OptionKind::Deny });
                let approval = Approval {
                    id: format!("pi-ui-{id}"),
                    kind: ApprovalKind::Question,
                    title: s(v, "title").unwrap_or_else(|| "pi 需要确认".into()),
                    command: None,
                    cwd: None,
                    diff: None,
                    reason: None,
                    detail: s(v, "message").or_else(|| s(v, "prefill")).or_else(|| s(v, "placeholder")),
                    options,
                    item: None,
                    ts: now_ms(),
                    thread: None,
                    thread_name: None,
                };
                self.ui.insert(approval.id.clone(), UiRequest { method, choices });
                out.event(AdapterEvent::ApprovalRequested(approval));
                out.status(ChatStatus::AwaitingApproval, None);
            }
            // setStatus, setWidget, setTitle, set_editor_text: TUI only.
            _ => {}
        }
    }

    fn on_tool_end(&mut self, v: &Value, out: &mut Out) {
        let id = s(v, "toolCallId").unwrap_or_default();
        let is_error = v.get("isError").and_then(|b| b.as_bool()).unwrap_or(false);
        let text = text_of(v.get("result").and_then(|r| r.get("content")));
        let mut it = self
            .open
            .remove(&id)
            .unwrap_or_else(|| tool_item(&id, &s(v, "toolName").unwrap_or_default(), &json!({}), ItemStatus::InProgress));
        let aborted = is_error && self.aborting && text.to_ascii_lowercase().contains("abort");
        it.status = if aborted {
            ItemStatus::Declined
        } else if is_error {
            ItemStatus::Failed
        } else {
            ItemStatus::Completed
        };
        it.output = Some(truncate_tail(&text, MAX_OUTPUT));
        if it.kind == ChatItemKind::Command {
            it.exit_code = if aborted {
                None
            } else if is_error {
                let code = text
                    .to_ascii_lowercase()
                    .rfind("exit code")
                    .and_then(|i| text[i + 9..].trim_start_matches([' ', ':']).split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()));
                code.or(Some(1))
            } else {
                Some(0)
            };
        }
        if let Some(t) = self.tool_started.remove(&id) {
            it.duration_ms = Some(now_ms().saturating_sub(t));
        }
        out.event(AdapterEvent::Item(it));
    }
}

impl Protocol for PiState {
    fn start(&mut self) -> Out {
        let mut out = Out::default();
        let m = self.cmd("get_state", json!({}));
        out.send(m);
        if self.resumed {
            let m = self.cmd("get_messages", json!({}));
            out.send(m);
        }
        out
    }

    fn on_message(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        let ty = s(v, "type").unwrap_or_default();
        match ty.as_str() {
            "response" => {
                let command = s(v, "command").unwrap_or_default();
                if !v.get("success").and_then(|b| b.as_bool()).unwrap_or(false) {
                    let msg = s(v, "error").unwrap_or_else(|| format!("{command} failed"));
                    let mut it = item(format!("err-{}", random_id()), ChatItemKind::Error, ItemStatus::Failed);
                    it.text = Some(msg.clone());
                    out.event(AdapterEvent::Item(it));
                    if command == "prompt" {
                        self.busy = false;
                        out.status(ChatStatus::Idle, None);
                    }
                    if command == "get_state" && !self.ready {
                        self.exit_error = Some(msg);
                    }
                    // The host showed the requested model already: report the one still in use.
                    if command == "set_model" {
                        if let Some(m) = self.model.clone() {
                            out.event(AdapterEvent::Model(m));
                        }
                    }
                    return out;
                }
                match command.as_str() {
                    "get_state" => {
                        if let Some(d) = v.get("data") {
                            self.on_state(d, &mut out);
                        }
                        if !self.ready {
                            self.ready = true;
                            out.status(ChatStatus::Idle, None);
                            for (text, att) in std::mem::take(&mut self.queued) {
                                let m = self.prompt(text, att);
                                out.send(m);
                                out.status(ChatStatus::Working, None);
                            }
                        }
                    }
                    "get_messages" => {
                        let msgs = v.get("data").and_then(|d| d.get("messages")).and_then(|m| m.as_array()).cloned().unwrap_or_default();
                        self.on_history(&msgs, &mut out);
                    }
                    "set_model" => {
                        // `data` is the Model now in use.
                        if let Some(d) = v.get("data") {
                            self.on_state(&json!({"model": d}), &mut out);
                        }
                    }
                    _ => {}
                }
            }
            "agent_start" => {
                self.busy = true;
                out.status(ChatStatus::Working, None);
            }
            "message_start" => {
                if v.get("message").and_then(|m| s(m, "role")).as_deref() == Some("assistant") {
                    self.msg_seq += 1;
                    self.blocks.clear();
                }
            }
            "message_update" => self.on_message_update(v, &mut out),
            "message_end" => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                if s(&m, "role").as_deref() == Some("assistant") {
                    // Complete text/thinking blocks that never got an *_end.
                    let ids: Vec<String> = self.blocks.drain().map(|(_, id)| id).collect();
                    for id in ids {
                        let is_text = self.open.get(&id).map(|it| matches!(it.kind, ChatItemKind::Agent | ChatItemKind::Reasoning)).unwrap_or(false);
                        if is_text {
                            if let Some(mut it) = self.open.remove(&id) {
                                it.status = ItemStatus::Completed;
                                out.event(AdapterEvent::Item(it));
                            }
                        }
                    }
                    if s(&m, "stopReason").as_deref() == Some("error") {
                        let msg = s(&m, "errorMessage").unwrap_or_else(|| "error".into());
                        if !(self.aborting && msg.to_ascii_lowercase().contains("abort")) {
                            let mut it = item(format!("err-{}", random_id()), ChatItemKind::Error, ItemStatus::Failed);
                            it.text = Some(msg);
                            out.event(AdapterEvent::Item(it));
                        }
                    }
                    if let (Some(p), Some(mo)) = (s(&m, "provider"), s(&m, "model")) {
                        self.set_model(format!("{p}/{mo}"), &mut out);
                    }
                }
            }
            "tool_execution_start" => {
                let id = s(v, "toolCallId").unwrap_or_default();
                self.tool_started.insert(id.clone(), now_ms());
                if let std::collections::hash_map::Entry::Vacant(slot) = self.open.entry(id.clone()) {
                    let args = v.get("args").cloned().unwrap_or(json!({}));
                    let it = tool_item(&id, &s(v, "toolName").unwrap_or_default(), &args, ItemStatus::InProgress);
                    slot.insert(it.clone());
                    out.event(AdapterEvent::Item(it));
                }
            }
            "tool_execution_update" => {
                let id = s(v, "toolCallId").unwrap_or_default();
                let partial = text_of(v.get("partialResult").and_then(|p| p.get("content")));
                if !partial.is_empty() {
                    if let Some(it) = self.open.get_mut(&id) {
                        it.output = Some(truncate_tail(&partial, MAX_OUTPUT));
                        out.event(AdapterEvent::Item(it.clone()));
                    }
                }
            }
            "tool_execution_end" => self.on_tool_end(v, &mut out),
            "agent_end" => {
                if !v.get("willRetry").and_then(|b| b.as_bool()).unwrap_or(false) {
                    let interrupted = self.aborting;
                    self.close_open(&mut out, if interrupted { ItemStatus::Declined } else { ItemStatus::Completed });
                    self.busy = false;
                    self.aborting = false;
                    let ids: Vec<String> = self.ui.drain().map(|(k, _)| k).collect();
                    for id in ids {
                        out.event(AdapterEvent::ApprovalResolved { approval: id, option: "cancelled".into() });
                    }
                    out.status(ChatStatus::Idle, interrupted.then(|| "interrupted".to_string()));
                    // New sessions get their file/id lazily; refresh.
                    let m = self.cmd("get_state", json!({}));
                    out.send(m);
                }
            }
            "auto_retry_start" => {
                let mut it = item(format!("pi-retry-{}", random_id()), ChatItemKind::System, ItemStatus::Completed);
                it.text = Some(format!("重试中: {}", s(v, "errorMessage").unwrap_or_default()));
                out.event(AdapterEvent::Item(it));
            }
            "compaction_end" => {
                let mut it = item(format!("pi-compact-{}", random_id()), ChatItemKind::System, ItemStatus::Completed);
                it.text = Some("上下文已压缩".into());
                out.event(AdapterEvent::Item(it));
            }
            "extension_ui_request" => self.on_ui_request(v, &mut out),
            _ => {}
        }
        out
    }

    fn on_cmd(&mut self, cmd: AdapterCmd) -> Out {
        let mut out = Out::default();
        match cmd {
            AdapterCmd::Send { text, attachments } => {
                let mut it = item(format!("user-{}", random_id()), ChatItemKind::User, ItemStatus::Completed);
                it.text = Some(text.clone());
                it.paths = attachments.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                out.event(AdapterEvent::Item(it));
                if !self.ready {
                    self.queued.push((text, attachments));
                } else {
                    let m = self.prompt(text, attachments);
                    out.send(m);
                    out.status(ChatStatus::Working, None);
                }
            }
            AdapterCmd::Interrupt => {
                if self.busy {
                    self.aborting = true;
                    let m = self.cmd("abort", json!({}));
                    out.send(m);
                }
            }
            AdapterCmd::Approve { approval_id, option_id } => {
                if let Some(req) = self.ui.remove(&approval_id) {
                    let rid = approval_id.trim_start_matches("pi-ui-").to_string();
                    let resp = if option_id == "deny" {
                        if req.method == "confirm" {
                            json!({"type": "extension_ui_response", "id": rid, "confirmed": false})
                        } else {
                            json!({"type": "extension_ui_response", "id": rid, "cancelled": true})
                        }
                    } else if let Some(i) = option_id.strip_prefix("choice:").and_then(|i| i.parse::<usize>().ok()) {
                        json!({"type": "extension_ui_response", "id": rid, "value": req.choices.get(i).cloned().unwrap_or_default()})
                    } else if req.method == "confirm" {
                        json!({"type": "extension_ui_response", "id": rid, "confirmed": true})
                    } else {
                        json!({"type": "extension_ui_response", "id": rid, "value": ""})
                    };
                    out.send(resp);
                    out.event(AdapterEvent::ApprovalResolved { approval: approval_id, option: option_id });
                    if self.ui.is_empty() && self.busy {
                        out.status(ChatStatus::Working, None);
                    }
                }
            }
            // pi runs tools without asking: there is no mode to change.
            AdapterCmd::SetApprovalMode(_) => {}
            AdapterCmd::SetModel(model) => {
                // Models are "provider/id"; a bare id keeps the current provider.
                let (provider, id) = match model.split_once('/') {
                    Some((p, id)) => (p.to_string(), id.to_string()),
                    None => {
                        let p = self.model.as_deref().and_then(|m| m.split_once('/')).map(|(p, _)| p.to_string()).unwrap_or_default();
                        (p, model.clone())
                    }
                };
                if provider.is_empty() || id.is_empty() {
                    let mut it = item(format!("pi-model-{}", random_id()), ChatItemKind::System, ItemStatus::Completed);
                    it.text = Some(format!("切换模型失败：无法识别 {model}（应为 provider/model）"));
                    out.event(AdapterEvent::Item(it));
                } else {
                    let m = self.cmd("set_model", json!({"provider": provider, "modelId": id}));
                    out.send(m);
                }
            }
            AdapterCmd::Shutdown => {}
        }
        out
    }

    fn on_exit(&mut self) -> Out {
        let mut out = Out::default();
        self.close_open(&mut out, ItemStatus::Failed);
        out
    }

    fn exit_error(&self) -> Option<String> {
        self.exit_error.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::tests::final_items;
    use yonder_proto::app::ApprovalMode;

    fn launch() -> AgentLaunch {
        AgentLaunch {
            agent: AgentKind::Pi,
            cwd: "/tmp".into(),
            model: None,
            approval: ApprovalMode::Ask,
            resume: None,
            env: Default::default(),
            program: None,
            login_shell: false,
        }
    }

    fn replay(name: &str) -> (PiState, Vec<AdapterEvent>) {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let mut st = PiState::new(launch());
        let mut evs = Vec::new();
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let o: Value = serde_json::from_str(line).unwrap();
            if o.get("dir").and_then(|d| d.as_str()) == Some("out") {
                if o["msg"]["type"] == "abort" {
                    evs.extend(st.on_cmd(AdapterCmd::Interrupt).events);
                }
                continue;
            }
            let Some(raw) = o.get("raw").and_then(|r| r.as_str()) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(raw) else { continue };
            evs.extend(st.on_message(&v).events);
        }
        (st, evs)
    }

    #[test]
    fn pong() {
        let (st, evs) = replay("pi_pong.jsonl");
        assert!(st.session.is_some());
        assert_eq!(st.model.as_deref(), Some("deepseek/deepseek-v4-flash"));
        let items = final_items(&evs);
        assert!(
            items.iter().any(|i| i.kind == ChatItemKind::Agent && i.text.as_deref() == Some("PONG") && i.status == ItemStatus::Completed),
            "{items:?}"
        );
        assert!(!st.busy);
    }

    #[test]
    fn tool_touch() {
        let (_, evs) = replay("pi_tool_touch.jsonl");
        let items = final_items(&evs);
        let cmd = items.iter().find(|i| i.kind == ChatItemKind::Command).expect("command");
        assert_eq!(cmd.title.as_deref(), Some("touch approval_test.txt && echo created"));
        assert_eq!(cmd.output.as_deref(), Some("created\n"));
        assert_eq!(cmd.exit_code, Some(0));
        assert_eq!(cmd.status, ItemStatus::Completed);
        assert!(items.iter().any(|i| i.kind == ChatItemKind::Reasoning));
        assert!(items.iter().any(|i| i.kind == ChatItemKind::Agent && i.text.as_deref() == Some("DONE")));
    }

    #[test]
    fn interrupt() {
        let (st, evs) = replay("pi_interrupt.jsonl");
        assert!(!st.busy);
        let items = final_items(&evs);
        let cmd = items.iter().find(|i| i.kind == ChatItemKind::Command).expect("command");
        assert_eq!(cmd.status, ItemStatus::Declined);
        assert!(!items.iter().any(|i| i.kind == ChatItemKind::Error), "{items:?}");
        assert!(evs.iter().any(|e| matches!(e, AdapterEvent::Status { status: ChatStatus::Idle, detail: Some(d) } if d == "interrupted")));
    }

    #[test]
    fn ui_dialogs() {
        let mut st = PiState::new(launch());
        st.ready = true;
        let out = st.on_message(&json!({"type":"extension_ui_request","id":"u1","method":"select","title":"Pick","options":["a","b"]}));
        let AdapterEvent::ApprovalRequested(a) = &out.events[0] else { panic!() };
        assert_eq!(a.options[1].label, "b");
        let out = st.on_cmd(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "choice:1".into() });
        assert_eq!(out.to_agent[0], json!({"type":"extension_ui_response","id":"u1","value":"b"}));
        let out = st.on_message(&json!({"type":"extension_ui_request","id":"u2","method":"confirm","title":"Sure?","message":"x"}));
        let AdapterEvent::ApprovalRequested(a) = &out.events[0] else { panic!() };
        let out = st.on_cmd(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "deny".into() });
        assert_eq!(out.to_agent[0], json!({"type":"extension_ui_response","id":"u2","confirmed":false}));
        assert!(st
            .on_message(&json!({"type":"extension_ui_request","id":"u3","method":"setStatus","statusKey":"k","statusText":"t"}))
            .events
            .is_empty());
    }

    #[test]
    fn argv_resume() {
        let mut l = launch();
        l.resume = Some("01a0".into());
        assert_eq!(argv(&l), vec!["pi", "--mode", "rpc", "--session", "01a0"]);
    }

    /// set_model splits "provider/id"; the model changes when pi confirms it.
    #[test]
    fn switch_model() {
        let mut st = PiState::new(launch());
        st.ready = true;
        st.model = Some("deepseek/deepseek-v4-flash".into());
        let out = st.on_cmd(AdapterCmd::SetModel("openrouter/anthropic/claude-sonnet-4.5".into()));
        let m = &out.to_agent[0];
        assert_eq!(m["type"], "set_model");
        assert_eq!(m["provider"], "openrouter");
        assert_eq!(m["modelId"], "anthropic/claude-sonnet-4.5");
        assert!(m["id"].is_string());
        assert!(out.events.is_empty());
        let out = st.on_message(&json!({"type":"response","id":m["id"],"command":"set_model","success":true,"data":{"id":"anthropic/claude-sonnet-4.5","provider":"openrouter","name":"Sonnet"}}));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Model(x) if x == "openrouter/anthropic/claude-sonnet-4.5")));

        // A bare id keeps the provider; a refusal is shown and the model stays.
        let out = st.on_cmd(AdapterCmd::SetModel("other".into()));
        assert_eq!(out.to_agent[0]["provider"], "openrouter");
        assert_eq!(out.to_agent[0]["modelId"], "other");
        let out = st.on_message(&json!({"type":"response","command":"set_model","success":false,"error":"Model not found: openrouter/other"}));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.kind == ChatItemKind::Error)));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Model(x) if x == "openrouter/anthropic/claude-sonnet-4.5")));
        assert_eq!(st.model.as_deref(), Some("openrouter/anthropic/claude-sonnet-4.5"));
    }
}
