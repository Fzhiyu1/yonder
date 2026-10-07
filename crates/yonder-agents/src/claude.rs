//! Claude Code adapter: `claude -p --input-format stream-json --output-format stream-json`
//! with `--permission-prompt-tool stdio` (SDK control protocol for tool approvals).
//!
//! Streaming: `stream_event` carries Anthropic API events (content_block_start/delta/stop)
//! for text, thinking and tool_use blocks; the full `assistant` message follows each API
//! message; tool results arrive as `user` messages with `tool_result` blocks; a turn ends
//! with `result`.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Value};
use yonder_proto::app::{
    AgentKind, Approval, ApprovalKind, ApprovalMode, ApprovalOption, ChatItem, ChatItemKind, ChatStatus, DeltaField,
    ItemStatus, OptionKind, SubagentStatus,
};

use crate::common::{item, now_ms, random_id, set_subagent_status, strip_ansi, subagent_active, subagent_card, truncate_tail, MAX_OUTPUT};
use crate::driver::{run, Out, Protocol};
use crate::{AdapterCmd, AdapterEvent, AdapterHandle, AgentLaunch};

/// Claude's `--permission-mode` for a yonder approval mode.
fn permission_mode(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Ask => "default",
        ApprovalMode::Auto => "acceptEdits",
        ApprovalMode::Yolo => "bypassPermissions",
    }
}

pub fn argv(launch: &AgentLaunch) -> Vec<String> {
    let mut argv = launch.program.clone().unwrap_or_else(|| vec!["claude".into()]);
    let mode = permission_mode(launch.approval);
    for a in [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-mode",
        mode,
        "--permission-prompt-tool",
        "stdio",
        // Lets `set_permission_mode` switch to bypassPermissions later (newer releases refuse
        // it otherwise); it does not bypass anything by itself.
        "--allow-dangerously-skip-permissions",
    ] {
        argv.push(a.to_string());
    }
    if let Some(m) = launch.model.as_ref().filter(|m| !m.is_empty()) {
        argv.extend(["--model".into(), m.clone()]);
    }
    if let Some(r) = launch.resume.as_ref().filter(|r| !r.is_empty()) {
        argv.extend(["--resume".into(), r.clone()]);
    }
    argv
}

pub fn spawn(launch: AgentLaunch) -> Result<AdapterHandle> {
    let st = ClaudeState::new(launch.clone());
    run(AgentKind::Claude, &launch, argv(&launch), st)
}

/// One content block being streamed.
struct Block {
    item: String,
    tool: Option<String>,
    json: String,
}

struct PendingApproval {
    request_id: String,
    input: Value,
    suggestions: Option<Value>,
    kind: ApprovalKind,
}

pub struct ClaudeState {
    launch: AgentLaunch,
    /// Approval mode in effect.
    mode: ApprovalMode,
    /// Outstanding `set_permission_mode` requests -> (mode before, mode requested).
    mode_requests: HashMap<String, (ApprovalMode, ApprovalMode)>,
    /// Outstanding `set_model` requests -> (model before, model requested).
    model_requests: HashMap<String, (Option<String>, String)>,
    init_id: String,
    initialized: bool,
    queued: Vec<Value>,
    busy: bool,
    session: Option<String>,
    /// Current API message id (item ids are derived from it).
    msg: Option<String>,
    blocks: HashMap<u64, Block>,
    /// Items not yet completed.
    open: HashMap<String, ChatItem>,
    /// tool_use id -> (tool name, start time).
    tools: HashMap<String, (String, u64)>,
    approvals: HashMap<String, PendingApproval>,
    model: Option<String>,
    /// Sub-agent (Task tool) cards by tool-use id, which is also the sub-agent's thread id
    /// (`parent_tool_use_id` of its messages).
    cards: HashMap<String, ChatItem>,
    /// Background sub-agents: their cards stay running after the turn until Claude reports them.
    background: std::collections::HashSet<String>,
    /// `agent_id` / `task_id` of a sub-agent -> its tool-use id.
    tasks: HashMap<String, String>,
    exit_error: Option<String>,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string)
}

pub(crate) fn image_mime(p: &std::path::Path) -> Option<&'static str> {
    match p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).as_deref() {
        Some("png") => Some("image/png"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        _ => None,
    }
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

fn tool_title(name: &str, input: &Value) -> String {
    let arg = ["file_path", "path", "pattern", "url", "query", "description", "prompt", "skill"]
        .iter()
        .find_map(|k| s(input, k))
        .map(|a| short(&a, 60));
    match arg {
        Some(a) => format!("{name} {a}"),
        None => name.to_string(),
    }
}

fn tool_kind(name: &str) -> ChatItemKind {
    match name {
        "Bash" | "BashOutput" | "PowerShell" => ChatItemKind::Command,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ChatItemKind::FileChange,
        "WebSearch" => ChatItemKind::WebSearch,
        "TodoWrite" => ChatItemKind::Plan,
        _ => ChatItemKind::Tool,
    }
}

fn parse_exit_code(text: &str) -> Option<i32> {
    let idx = text.find("Exit code ")?;
    text[idx + 10..].split(|c: char| !c.is_ascii_digit() && c != '-').next()?.parse().ok()
}

/// Unified diff for Edit / MultiEdit / Write inputs (before the tool runs).
fn edit_diff(tool: &str, path: &str, input: &Value) -> String {
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    let mut hunk = |old: &str, new: &str| {
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();
        out.push_str(&format!("@@ -1,{} +1,{} @@\n", old_lines.len(), new_lines.len()));
        for l in old_lines {
            out.push_str(&format!("-{l}\n"));
        }
        for l in new_lines {
            out.push_str(&format!("+{l}\n"));
        }
    };
    match tool {
        "Write" => hunk("", &s(input, "content").unwrap_or_default()),
        "MultiEdit" => {
            for e in input.get("edits").and_then(|e| e.as_array()).into_iter().flatten() {
                hunk(&s(e, "old_string").unwrap_or_default(), &s(e, "new_string").unwrap_or_default());
            }
        }
        "NotebookEdit" => hunk("", &s(input, "new_source").unwrap_or_default()),
        _ => hunk(&s(input, "old_string").unwrap_or_default(), &s(input, "new_string").unwrap_or_default()),
    }
    truncate_tail(&out, MAX_OUTPUT * 2)
}

/// Claude's `structuredPatch` (hunks with `lines`) -> unified diff.
fn structured_patch_diff(path: &str, patch: &[Value]) -> Option<String> {
    if patch.is_empty() {
        return None;
    }
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for h in patch {
        let n = |k: &str| h.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        out.push_str(&format!("@@ -{},{} +{},{} @@\n", n("oldStart"), n("oldLines"), n("newStart"), n("newLines")));
        for l in h.get("lines").and_then(|l| l.as_array()).into_iter().flatten() {
            if let Some(l) = l.as_str() {
                out.push_str(l);
                out.push('\n');
            }
        }
    }
    Some(out)
}

/// The tool that runs a sub-agent (`Task`, renamed `Agent` in Claude Code 2.1).
fn is_task(name: &str) -> bool {
    matches!(name, "Task" | "Agent")
}

/// Sub-agent card for a Task tool call (`thread` = its tool-use id).
fn task_card(id: &str, input: &Value) -> ChatItem {
    let mut it = subagent_card(id, id, SubagentStatus::Running);
    it.text = s(input, "prompt");
    if let Some(sub) = it.subagent.as_mut() {
        sub.name = s(input, "description").filter(|d| !d.is_empty());
        sub.role = s(input, "subagent_type").filter(|d| !d.is_empty());
        sub.model = s(input, "model").filter(|d| !d.is_empty());
    }
    it
}

/// Chat item for a tool_use block built from its (possibly empty) input.
fn tool_item(id: &str, name: &str, input: &Value, status: ItemStatus) -> ChatItem {
    if is_task(name) {
        return task_card(id, input);
    }
    let kind = tool_kind(name);
    let mut it = item(id, kind, status);
    match kind {
        ChatItemKind::Command => {
            it.title = s(input, "command").or_else(|| Some(name.to_string()));
            it.text = s(input, "description");
        }
        ChatItemKind::FileChange => {
            let path = s(input, "file_path").or_else(|| s(input, "notebook_path")).unwrap_or_default();
            if !path.is_empty() {
                it.paths = vec![path.clone()];
            }
            it.title = Some(if path.is_empty() { name.to_string() } else { path.clone() });
            if input.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                it.diff = Some(edit_diff(name, &path, input));
            }
        }
        ChatItemKind::WebSearch => it.title = s(input, "query"),
        ChatItemKind::Plan => {
            let todos = input.get("todos").and_then(|t| t.as_array()).cloned().unwrap_or_default();
            let lines: Vec<String> = todos
                .iter()
                .map(|t| {
                    let mark = match s(t, "status").as_deref() {
                        Some("completed") => "x",
                        Some("in_progress") => "~",
                        _ => " ",
                    };
                    format!("- [{mark}] {}", s(t, "content").unwrap_or_default())
                })
                .collect();
            it.text = Some(lines.join("\n"));
        }
        _ => {
            it.title = Some(tool_title(name, input));
            if input.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                it.text = Some(serde_json::to_string_pretty(input).unwrap_or_default());
            }
        }
    }
    it
}

impl ClaudeState {
    pub fn new(launch: AgentLaunch) -> Self {
        Self {
            mode: launch.approval,
            mode_requests: HashMap::new(),
            model_requests: HashMap::new(),
            launch,
            init_id: format!("req_init_{}", random_id()),
            initialized: false,
            queued: Vec::new(),
            busy: false,
            session: None,
            msg: None,
            blocks: HashMap::new(),
            open: HashMap::new(),
            tools: HashMap::new(),
            approvals: HashMap::new(),
            model: None,
            cards: HashMap::new(),
            background: Default::default(),
            tasks: HashMap::new(),
            exit_error: None,
        }
    }

    fn user_message(&self, text: &str, attachments: &[std::path::PathBuf]) -> (Value, ChatItem) {
        use base64::Engine;
        let mut content = Vec::new();
        let mut refs = String::new();
        for a in attachments {
            if let Some(mime) = image_mime(a) {
                if let Ok(bytes) = std::fs::read(a) {
                    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                    content.push(json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}}));
                    continue;
                }
            }
            refs.push_str(&format!("\n@{}", a.display()));
        }
        content.push(json!({"type": "text", "text": format!("{text}{refs}")}));
        let mut it = item(format!("user-{}", random_id()), ChatItemKind::User, ItemStatus::Completed);
        it.text = Some(text.to_string());
        it.paths = attachments.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let msg = json!({
            "type": "user",
            "session_id": self.session.clone().unwrap_or_default(),
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": content},
        });
        (msg, it)
    }

    fn set_session(&mut self, sid: String, out: &mut Out) {
        if self.session.as_deref() != Some(sid.as_str()) {
            self.session = Some(sid.clone());
            out.event(AdapterEvent::AgentSession(sid));
        }
    }

    fn set_model(&mut self, m: String, out: &mut Out) {
        if self.model.as_deref() != Some(m.as_str()) {
            self.model = Some(m.clone());
            out.event(AdapterEvent::Model(m));
        }
    }

    fn finish_turn(&mut self, out: &mut Out, interrupted: bool) {
        self.busy = false;
        let mut open: Vec<ChatItem> = self.open.drain().map(|(_, v)| v).collect();
        open.sort_by_key(|i| i.ts);
        for mut it in open {
            if it.kind == ChatItemKind::Subagent {
                continue;
            }
            if it.status == ItemStatus::InProgress {
                it.status = if interrupted { ItemStatus::Declined } else { ItemStatus::Completed };
                out.event(AdapterEvent::Item(it));
            }
        }
        // Foreground sub-agents end with the turn; background ones report later.
        let ids: Vec<String> = self.cards.iter().filter(|(k, c)| subagent_active(c) && !self.background.contains(*k)).map(|(k, _)| k.clone()).collect();
        for id in ids {
            let status = if interrupted { SubagentStatus::Interrupted } else { SubagentStatus::Done };
            self.update_card(&id, out, |c| c.status = status);
        }
        self.blocks.clear();
        self.tools.clear();
        let ids: Vec<String> = self.approvals.drain().map(|(k, _)| k).collect();
        for id in ids {
            out.event(AdapterEvent::ApprovalResolved { approval: id, option: "cancelled".into() });
        }
        out.status(ChatStatus::Idle, interrupted.then(|| "interrupted".to_string()));
    }

    fn on_stream_event(&mut self, ev: &Value, out: &mut Out) {
        let index = ev.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        match ev.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "message_start" => {
                self.msg = ev.get("message").and_then(|m| s(m, "id")).or_else(|| Some(format!("msg-{}", random_id())));
                self.blocks.clear();
            }
            "content_block_start" => {
                let cb = ev.get("content_block").cloned().unwrap_or(Value::Null);
                let base = self.msg.clone().unwrap_or_else(|| "msg".into());
                match s(&cb, "type").as_deref() {
                    Some("text") | Some("thinking") => {
                        let thinking = s(&cb, "type").as_deref() == Some("thinking");
                        let id = format!("{base}-{index}");
                        let kind = if thinking { ChatItemKind::Reasoning } else { ChatItemKind::Agent };
                        let mut it = item(&id, kind, ItemStatus::InProgress);
                        it.text = Some(s(&cb, if thinking { "thinking" } else { "text" }).unwrap_or_default());
                        self.blocks.insert(index, Block { item: id.clone(), tool: None, json: String::new() });
                        self.open.insert(id, it.clone());
                        out.event(AdapterEvent::Item(it));
                    }
                    Some("tool_use") | Some("server_tool_use") => {
                        let id = s(&cb, "id").unwrap_or_else(|| format!("{base}-{index}"));
                        let name = s(&cb, "name").unwrap_or_default();
                        let it = tool_item(&id, &name, &json!({}), ItemStatus::InProgress);
                        self.tools.insert(id.clone(), (name.clone(), now_ms()));
                        self.blocks.insert(index, Block { item: id.clone(), tool: Some(name), json: String::new() });
                        self.show_tool(it, out);
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let delta = ev.get("delta").cloned().unwrap_or(Value::Null);
                let Some(b) = self.blocks.get_mut(&index) else { return };
                let text = match s(&delta, "type").as_deref() {
                    Some("text_delta") => s(&delta, "text"),
                    Some("thinking_delta") => s(&delta, "thinking"),
                    Some("input_json_delta") => {
                        b.json.push_str(&s(&delta, "partial_json").unwrap_or_default());
                        None
                    }
                    _ => None,
                };
                if let Some(t) = text {
                    if let Some(it) = self.open.get_mut(&b.item) {
                        it.text.get_or_insert_with(String::new).push_str(&t);
                    }
                    out.event(AdapterEvent::Delta { item: b.item.clone(), field: DeltaField::Text, delta: t, thread: None });
                }
            }
            "content_block_stop" => {
                let Some(b) = self.blocks.remove(&index) else { return };
                match b.tool {
                    None => {
                        if let Some(mut it) = self.open.remove(&b.item) {
                            it.status = ItemStatus::Completed;
                            out.event(AdapterEvent::Item(it));
                        }
                    }
                    Some(name) => {
                        // Tool input complete: show it while the tool runs.
                        let input: Value = serde_json::from_str(&b.json).unwrap_or(json!({}));
                        let it = tool_item(&b.item, &name, &input, ItemStatus::InProgress);
                        self.show_tool(it, out);
                    }
                }
            }
            _ => {}
        }
    }

    /// Full assistant message: authoritative content (also covers runs without partials).
    fn on_assistant(&mut self, v: &Value, out: &mut Out) {
        let msg = v.get("message").cloned().unwrap_or(Value::Null);
        // Messages of a sub-agent carry the tool-use id of its Task call: they belong to its
        // thread (read-only view), not to the chat.
        let thread = s(v, "parent_tool_use_id");
        match (&thread, s(&msg, "model")) {
            (None, Some(m)) => self.set_model(m, out),
            (Some(t), Some(m)) => self.update_card(t, out, |c| {
                c.model.get_or_insert(m);
            }),
            _ => {}
        }
        let mid = s(&msg, "id").or_else(|| self.msg.clone()).unwrap_or_else(|| format!("msg-{}", random_id()));
        for (i, c) in msg.get("content").and_then(|c| c.as_array()).into_iter().flatten().enumerate() {
            match s(c, "type").as_deref() {
                Some("text") | Some("thinking") => {
                    let thinking = s(c, "type").as_deref() == Some("thinking");
                    let id = format!("{mid}-{i}");
                    let text = s(c, if thinking { "thinking" } else { "text" });
                    // Block still streaming: its content_block_stop will complete it.
                    if let Some(it) = self.open.get_mut(&id) {
                        it.text = text;
                        continue;
                    }
                    let mut it = item(&id, if thinking { ChatItemKind::Reasoning } else { ChatItemKind::Agent }, ItemStatus::Completed);
                    it.text = text;
                    it.thread = thread.clone();
                    out.event(AdapterEvent::Item(it));
                }
                Some("tool_use") | Some("server_tool_use") => {
                    let id = s(c, "id").unwrap_or_else(|| format!("{mid}-{i}"));
                    let name = s(c, "name").unwrap_or_default();
                    let input = c.get("input").cloned().unwrap_or(json!({}));
                    let mut it = tool_item(&id, &name, &input, ItemStatus::InProgress);
                    it.thread = thread.clone();
                    self.tools.entry(id.clone()).or_insert((name, now_ms()));
                    self.show_tool(it, out);
                }
                _ => {}
            }
        }
    }

    /// Shows a tool call that is running. Task calls become (or update) sub-agent cards, which
    /// keep the status they reached.
    fn show_tool(&mut self, mut it: ChatItem, out: &mut Out) {
        if it.kind == ChatItemKind::Subagent {
            // Seen before (streamed, then the full message): keep what is known.
            if let Some(prev) = self.cards.get(&it.id) {
                if let (Some(mut sub), Some(new)) = (prev.subagent.clone(), it.subagent.take()) {
                    sub.name = new.name.or(sub.name);
                    sub.role = new.role.or(sub.role);
                    sub.model = new.model.or(sub.model);
                    it.subagent = Some(sub);
                }
                it.status = prev.status;
                it.text = it.text.or_else(|| prev.text.clone());
                it.ts = prev.ts;
            }
            self.cards.insert(it.id.clone(), it.clone());
        }
        self.open.insert(it.id.clone(), it.clone());
        out.event(AdapterEvent::Item(it));
    }

    /// Changes the card of sub-agent `id` and sends it when it changed.
    fn update_card(&mut self, id: &str, out: &mut Out, f: impl FnOnce(&mut yonder_proto::app::Subagent)) {
        let Some(card) = self.cards.get_mut(id) else { return };
        let Some(sub) = card.subagent.as_mut() else { return };
        let before = (sub.clone(), card.status);
        f(sub);
        let status = sub.status;
        set_subagent_status(card, status);
        if card.subagent.as_ref() != Some(&before.0) || card.status != before.1 {
            if let Some(o) = self.open.get_mut(id) {
                *o = card.clone();
            }
            out.event(AdapterEvent::Item(card.clone()));
        }
    }

    /// `user` messages from Claude carry tool results.
    fn on_user(&mut self, v: &Value, out: &mut Out) {
        let thread = s(v, "parent_tool_use_id");
        let content = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()).cloned().unwrap_or_default();
        let tur = v.get("tool_use_result").cloned();
        // A sub-agent's task arrives as its first user message: the start of its thread.
        if let Some(t) = &thread {
            let text: Vec<String> = content.iter().filter(|c| s(c, "type").as_deref() == Some("text")).filter_map(|c| s(c, "text")).collect();
            if !text.is_empty() {
                let id = s(v, "uuid").unwrap_or_else(|| format!("user-{}", random_id()));
                let mut it = item(id, ChatItemKind::User, ItemStatus::Completed);
                it.text = Some(text.join("\n"));
                it.thread = Some(t.clone());
                out.event(AdapterEvent::Item(it));
            }
        }
        for c in content {
            if s(&c, "type").as_deref() != Some("tool_result") {
                continue;
            }
            let Some(tid) = s(&c, "tool_use_id") else { continue };
            let is_error = c.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let text = match c.get("content") {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Array(parts)) => parts.iter().filter_map(|p| s(p, "text")).collect::<Vec<_>>().join("\n"),
                _ => String::new(),
            };
            let (name, started) = self.tools.remove(&tid).unwrap_or_else(|| ("tool".into(), now_ms()));
            let rejected = is_error
                && (text.contains("doesn't want to proceed") || text.contains("was rejected") || text.contains("denied this action"));
            if self.cards.contains_key(&tid) {
                self.open.remove(&tid);
                self.task_result(&tid, &c, tur.as_ref(), is_error, rejected, out);
                continue;
            }
            let mut it = self.open.remove(&tid).unwrap_or_else(|| {
                let mut it = tool_item(&tid, &name, &json!({}), ItemStatus::InProgress);
                it.thread = thread.clone();
                it
            });
            it.status = if rejected {
                ItemStatus::Declined
            } else if is_error {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            };
            match it.kind {
                ChatItemKind::Command => {
                    let stdout = tur.as_ref().and_then(|t| s(t, "stdout"));
                    let stderr = tur.as_ref().and_then(|t| s(t, "stderr")).filter(|e| !e.is_empty());
                    let o = match (stdout, stderr) {
                        (Some(o), Some(e)) => format!("{o}{}{e}", if o.is_empty() || o.ends_with('\n') { "" } else { "\n" }),
                        (Some(o), None) => o,
                        (None, Some(e)) => e,
                        (None, None) => text.clone(),
                    };
                    it.output = Some(truncate_tail(&o, MAX_OUTPUT));
                    it.exit_code = if rejected {
                        None
                    } else if is_error {
                        parse_exit_code(&text).or(Some(1))
                    } else {
                        Some(0)
                    };
                }
                ChatItemKind::FileChange => {
                    if is_error {
                        it.output = Some(truncate_tail(&text, MAX_OUTPUT));
                    }
                    let path = it.paths.first().cloned().unwrap_or_default();
                    if let Some(patch) = tur.as_ref().and_then(|t| t.get("structuredPatch")).and_then(|p| p.as_array()) {
                        if let Some(d) = structured_patch_diff(&path, patch) {
                            it.diff = Some(truncate_tail(&d, MAX_OUTPUT * 2));
                        }
                    }
                }
                _ => it.output = Some(truncate_tail(&text, MAX_OUTPUT)),
            }
            it.duration_ms = Some(now_ms().saturating_sub(started));
            out.event(AdapterEvent::Item(it));
        }
    }

    /// The Task tool returned: the sub-agent's final reply, or that it went to the background.
    fn task_result(&mut self, id: &str, c: &Value, tur: Option<&Value>, is_error: bool, rejected: bool, out: &mut Out) {
        let launched = tur.and_then(|t| s(t, "status")).is_some_and(|st| st.ends_with("_launched"));
        if launched {
            self.background.insert(id.to_string());
            if let Some(a) = tur.and_then(|t| s(t, "agentId")) {
                self.tasks.insert(a, id.to_string());
            }
            return;
        }
        // The reply without Claude's trailer ("agentId: ... <usage>...</usage>").
        let parts = tur.and_then(|t| t.get("content")).and_then(|c| c.as_array()).cloned().or_else(|| c.get("content").and_then(|c| c.as_array()).cloned());
        let reply = match (parts, c.get("content")) {
            (Some(parts), _) => parts.iter().filter_map(|p| s(p, "text")).filter(|t| !t.starts_with("agentId:")).collect::<Vec<_>>().join("\n"),
            (None, Some(Value::String(t))) => t.clone(),
            _ => String::new(),
        };
        let status = if rejected {
            SubagentStatus::Interrupted
        } else if is_error {
            SubagentStatus::Failed
        } else {
            SubagentStatus::Done
        };
        // Claude does not stream the sub-agent's last message; it is this result. Close its
        // thread with it.
        if !reply.trim().is_empty() {
            let mut it = item(format!("{id}-reply"), ChatItemKind::Agent, ItemStatus::Completed);
            it.text = Some(reply.clone());
            it.thread = Some(id.to_string());
            out.event(AdapterEvent::Item(it));
        }
        self.update_card(id, out, |sub| {
            sub.status = status;
            if !reply.trim().is_empty() {
                sub.reply = Some(truncate_tail(&reply, MAX_OUTPUT));
            }
        });
    }

    fn on_control_request(&mut self, v: &Value, out: &mut Out) {
        let request_id = s(v, "request_id").unwrap_or_default();
        let req = v.get("request").cloned().unwrap_or(Value::Null);
        if s(&req, "subtype").as_deref() != Some("can_use_tool") {
            // Hook callbacks, MCP messages: not supported by this client.
            out.send(json!({"type": "control_response", "response": {"subtype": "error", "request_id": request_id, "error": "unsupported by yonder"}}));
            return;
        }
        let tool = s(&req, "tool_name").unwrap_or_default();
        let input = req.get("input").cloned().unwrap_or(json!({}));
        let path = s(&input, "file_path").or_else(|| s(&input, "notebook_path")).unwrap_or_default();
        let kind = match tool_kind(&tool) {
            ChatItemKind::Command => ApprovalKind::Command,
            ChatItemKind::FileChange => ApprovalKind::FileChange,
            _ if tool == "AskUserQuestion" || tool == "ExitPlanMode" => ApprovalKind::Question,
            _ => ApprovalKind::Tool,
        };
        let command = if kind == ApprovalKind::Command { s(&input, "command") } else { None };
        let title = match kind {
            ApprovalKind::Command => format!("运行命令: {}", short(command.as_deref().unwrap_or(""), 80)),
            ApprovalKind::FileChange => format!("修改文件: {path}"),
            _ => format!("使用工具: {}", tool_title(&tool, &input)),
        };
        let diff = (kind == ApprovalKind::FileChange).then(|| edit_diff(&tool, &path, &input));
        let reason = s(&req, "decision_reason").or_else(|| s(&req, "description")).map(|r| strip_ansi(&r));
        let suggestions = req.get("permission_suggestions").filter(|p| p.as_array().map(|a| !a.is_empty()).unwrap_or(false)).cloned();
        let mut options = vec![ApprovalOption { id: "allow".into(), label: "允许".into(), kind: OptionKind::Allow }];
        let suppress = req.get("suppress_always_allow_rule").and_then(|b| b.as_bool()).unwrap_or(false);
        if suggestions.is_some() && !suppress {
            options.push(ApprovalOption { id: "allow_always".into(), label: "本会话总是允许".into(), kind: OptionKind::AllowAlways });
        }
        options.push(ApprovalOption { id: "deny".into(), label: "拒绝".into(), kind: OptionKind::Deny });
        options.push(ApprovalOption { id: "abort".into(), label: "拒绝并停止".into(), kind: OptionKind::Abort });
        let detail = match kind {
            ApprovalKind::Command | ApprovalKind::FileChange => s(&input, "description"),
            _ => Some(serde_json::to_string_pretty(&input).unwrap_or_default()),
        };
        let approval_id = format!("claude-{request_id}");
        // Raised inside a sub-agent: its tool call is in that sub-agent's thread (or Claude names
        // the agent).
        let tool_use = s(&req, "tool_use_id");
        let thread = tool_use
            .as_ref()
            .and_then(|t| self.open.get(t))
            .and_then(|it| it.thread.clone())
            .or_else(|| s(&req, "agent_id").and_then(|a| self.tasks.get(&a).cloned()));
        let thread_name = thread.as_ref().and_then(|t| self.cards.get(t)).and_then(|c| c.subagent.as_ref()).and_then(|sub| sub.name.clone().or_else(|| sub.role.clone()));
        let pending = PendingApproval { request_id, input, suggestions, kind };
        // Switched to full access while Claude still asks (the switch raced this request).
        if self.auto_approves(kind) {
            out.send(Self::respond(&pending, "allow"));
            return;
        }
        let approval = Approval {
            id: approval_id.clone(),
            kind,
            title,
            command,
            cwd: Some(self.launch.cwd.to_string_lossy().into_owned()),
            diff,
            reason,
            detail,
            options,
            item: tool_use,
            ts: now_ms(),
            thread,
            thread_name,
        };
        self.approvals.insert(approval_id, pending);
        out.event(AdapterEvent::ApprovalRequested(approval));
        out.status(ChatStatus::AwaitingApproval, None);
    }

    /// Switching to full access approves the prompts Claude sent under the old mode: those
    /// pending at the switch and those racing it (sent before Claude applied bypassPermissions).
    /// Once Claude confirmed the switch, whatever it still asks (explicit `ask` rules, its own
    /// safety checks) goes to the user, as in a chat started in full access. Questions (plan
    /// approval, AskUserQuestion) always do.
    fn auto_approves(&self, kind: ApprovalKind) -> bool {
        self.mode == ApprovalMode::Yolo
            && kind != ApprovalKind::Question
            && self.mode_requests.values().any(|(_, to)| *to == ApprovalMode::Yolo)
    }

    /// `control_response` for a pending permission request and the chosen option.
    fn respond(p: &PendingApproval, option_id: &str) -> Value {
        let response = match option_id {
            "allow" => json!({"behavior": "allow", "updatedInput": p.input}),
            "allow_always" => {
                let mut r = json!({"behavior": "allow", "updatedInput": p.input});
                if let Some(sug) = &p.suggestions {
                    r["updatedPermissions"] = sug.clone();
                }
                r
            }
            "abort" => json!({"behavior": "deny", "message": "The user denied this action from yonder and stopped the turn.", "interrupt": true}),
            _ => json!({"behavior": "deny", "message": "The user denied this action from yonder."}),
        };
        json!({"type": "control_response", "response": {"subtype": "success", "request_id": p.request_id, "response": response}})
    }

    fn on_result(&mut self, v: &Value, out: &mut Out) {
        if let Some(sid) = s(v, "session_id") {
            self.set_session(sid, out);
        }
        let subtype = s(v, "subtype").unwrap_or_default();
        let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
        // An interrupt ends the turn with error_during_execution while a tool was pending.
        let interrupted = subtype == "error_during_execution" && s(v, "stop_reason").as_deref() == Some("tool_use");
        if is_error && !interrupted {
            let errors = v.get("errors").and_then(|e| e.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("\n"));
            let msg = s(v, "result").or(errors).filter(|m| !m.is_empty()).unwrap_or_else(|| subtype.clone());
            let mut it = item(format!("err-{}", random_id()), ChatItemKind::Error, ItemStatus::Failed);
            it.text = Some(msg);
            out.event(AdapterEvent::Item(it));
        }
        self.finish_turn(out, interrupted);
    }
}

impl Protocol for ClaudeState {
    fn start(&mut self) -> Out {
        let mut out = Out::default();
        out.send(json!({"type": "control_request", "request_id": self.init_id, "request": {"subtype": "initialize", "hooks": null}}));
        out
    }

    fn on_message(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        match s(v, "type").as_deref() {
            Some("control_response") => {
                let resp = v.get("response").cloned().unwrap_or(Value::Null);
                if !self.initialized && s(&resp, "request_id").as_deref() == Some(self.init_id.as_str()) {
                    self.initialized = true;
                    if s(&resp, "subtype").as_deref() == Some("error") {
                        let msg = s(&resp, "error").unwrap_or_else(|| "initialize failed".into());
                        self.exit_error = Some(msg.clone());
                        out.status(ChatStatus::Error, Some(msg));
                        return out;
                    }
                    if let Some(sid) = self.launch.resume.clone() {
                        self.set_session(sid, &mut out);
                    }
                    let queued = std::mem::take(&mut self.queued);
                    out.status(if queued.is_empty() { ChatStatus::Idle } else { ChatStatus::Working }, None);
                    for m in queued {
                        self.busy = true;
                        out.send(m);
                    }
                } else if let Some((prev, to)) = s(&resp, "request_id").and_then(|r| self.mode_requests.remove(&r)) {
                    if s(&resp, "subtype").as_deref() == Some("error") {
                        // Claude kept the old mode: say so and report the mode that still applies
                        // (unless a later switch already replaced this one).
                        let msg = s(&resp, "error").unwrap_or_else(|| "set_permission_mode failed".into());
                        let mut it = item(format!("err-mode-{}", random_id()), ChatItemKind::Error, ItemStatus::Failed);
                        it.text = Some(format!("切换审批模式失败：{msg}"));
                        out.event(AdapterEvent::Item(it));
                        if self.mode == to {
                            self.mode = prev;
                            out.event(AdapterEvent::ApprovalMode(prev));
                        }
                    }
                } else if let Some((prev, to)) = s(&resp, "request_id").and_then(|r| self.model_requests.remove(&r)) {
                    if s(&resp, "subtype").as_deref() == Some("error") {
                        // Claude kept the old model: say so and report it back (unless a later
                        // switch already replaced this one).
                        let msg = s(&resp, "error").unwrap_or_else(|| "set_model failed".into());
                        let mut it = item(format!("sys-model-{}", random_id()), ChatItemKind::System, ItemStatus::Completed);
                        it.text = Some(format!("切换模型失败：{msg}"));
                        out.event(AdapterEvent::Item(it));
                        if self.model.as_deref() == Some(to.as_str()) {
                            self.model = prev.clone();
                            if let Some(p) = prev {
                                out.event(AdapterEvent::Model(p));
                            }
                        }
                    }
                }
            }
            Some("control_request") => self.on_control_request(v, &mut out),
            Some("control_cancel_request") => {
                if let Some(rid) = s(v, "request_id") {
                    let id = format!("claude-{rid}");
                    if self.approvals.remove(&id).is_some() {
                        out.event(AdapterEvent::ApprovalResolved { approval: id, option: "cancelled".into() });
                    }
                }
            }
            Some("system") => match s(v, "subtype").as_deref() {
                Some("init") => {
                    if let Some(sid) = s(v, "session_id") {
                        self.set_session(sid, &mut out);
                    }
                    if let Some(m) = s(v, "model") {
                        self.set_model(m, &mut out);
                    }
                }
                Some("task_started") => {
                    if let (Some(task), Some(tool)) = (s(v, "task_id"), s(v, "tool_use_id")) {
                        self.tasks.insert(task, tool);
                    }
                }
                Some("task_notification") => {
                    let tool = s(v, "tool_use_id").or_else(|| s(v, "task_id").and_then(|t| self.tasks.get(&t).cloned()));
                    if let Some(id) = tool {
                        let status = match s(v, "status").as_deref() {
                            Some("failed") => SubagentStatus::Failed,
                            Some("stopped" | "killed") => SubagentStatus::Interrupted,
                            _ => SubagentStatus::Done,
                        };
                        let summary = s(v, "summary").filter(|t| !t.trim().is_empty());
                        self.background.remove(&id);
                        self.update_card(&id, &mut out, |sub| {
                            sub.status = status;
                            if sub.reply.is_none() {
                                sub.reply = summary;
                            }
                        });
                    }
                }
                Some("compact_boundary") => {
                    let mut it = item(format!("sys-{}", random_id()), ChatItemKind::System, ItemStatus::Completed);
                    it.text = Some("上下文已压缩".into());
                    out.event(AdapterEvent::Item(it));
                }
                _ => {}
            },
            Some("stream_event") => {
                if v.get("parent_tool_use_id").map(|p| p.is_null()).unwrap_or(true) {
                    if let Some(ev) = v.get("event") {
                        self.on_stream_event(ev, &mut out);
                    }
                }
            }
            Some("assistant") => self.on_assistant(v, &mut out),
            Some("user") => self.on_user(v, &mut out),
            Some("result") => self.on_result(v, &mut out),
            _ => {}
        }
        out
    }

    fn on_cmd(&mut self, cmd: AdapterCmd) -> Out {
        let mut out = Out::default();
        match cmd {
            AdapterCmd::Send { text, attachments } => {
                let (msg, it) = self.user_message(&text, &attachments);
                out.event(AdapterEvent::Item(it));
                if !self.initialized {
                    self.queued.push(msg);
                } else {
                    // Claude queues user messages sent mid-turn and handles them next.
                    self.busy = true;
                    out.send(msg);
                    out.status(ChatStatus::Working, None);
                }
            }
            AdapterCmd::Interrupt => {
                if self.busy {
                    out.send(json!({"type": "control_request", "request_id": format!("req_int_{}", random_id()), "request": {"subtype": "interrupt"}}));
                }
            }
            AdapterCmd::Approve { approval_id, option_id } => {
                if let Some(p) = self.approvals.remove(&approval_id) {
                    out.send(Self::respond(&p, &option_id));
                    out.event(AdapterEvent::ApprovalResolved { approval: approval_id, option: option_id });
                    if self.approvals.is_empty() && self.busy {
                        out.status(ChatStatus::Working, None);
                    }
                }
            }
            AdapterCmd::SetApprovalMode(mode) => {
                let prev = std::mem::replace(&mut self.mode, mode);
                // Claude handles control requests in order, so this also works before initialize answers.
                let request_id = format!("req_mode_{}", random_id());
                self.mode_requests.insert(request_id.clone(), (prev, mode));
                out.send(json!({
                    "type": "control_request",
                    "request_id": request_id,
                    "request": {"subtype": "set_permission_mode", "mode": permission_mode(mode)},
                }));
                out.event(AdapterEvent::ApprovalMode(mode));
                let ids: Vec<String> = self.approvals.iter().filter(|(_, p)| self.auto_approves(p.kind)).map(|(k, _)| k.clone()).collect();
                for id in ids {
                    if let Some(p) = self.approvals.remove(&id) {
                        out.send(Self::respond(&p, "allow"));
                        out.event(AdapterEvent::ApprovalResolved { approval: id, option: "allow".into() });
                    }
                }
                if self.approvals.is_empty() && self.busy {
                    out.status(ChatStatus::Working, None);
                }
            }
            AdapterCmd::SetModel(model) => {
                let prev = self.model.replace(model.clone());
                // Like set_permission_mode: Claude handles control requests in order.
                let request_id = format!("req_model_{}", random_id());
                self.model_requests.insert(request_id.clone(), (prev, model.clone()));
                out.send(json!({
                    "type": "control_request",
                    "request_id": request_id,
                    "request": {"subtype": "set_model", "model": model},
                }));
                out.event(AdapterEvent::Model(model));
            }
            AdapterCmd::Shutdown => {}
        }
        out
    }

    fn on_exit(&mut self) -> Out {
        let mut out = Out::default();
        if self.busy || !self.open.is_empty() {
            self.finish_turn(&mut out, true);
        }
        out
    }

    fn exit_error(&self) -> Option<String> {
        self.exit_error.clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn launch() -> AgentLaunch {
        AgentLaunch {
            agent: AgentKind::Claude,
            cwd: "/tmp".into(),
            model: None,
            approval: ApprovalMode::Ask,
            resume: None,
            env: Default::default(),
            program: None,
            login_shell: false,
        }
    }

    /// Replay recorded agent output; returns the state and emitted events.
    fn replay(name: &str) -> (ClaudeState, Vec<AdapterEvent>) {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let mut st = ClaudeState::new(launch());
        let mut evs = Vec::new();
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let o: Value = serde_json::from_str(line).unwrap();
            if o.get("dir").and_then(|d| d.as_str()) == Some("out") {
                if let Some(rid) = o["msg"]["request_id"].as_str() {
                    if rid.starts_with("req_init_") {
                        st.init_id = rid.to_string();
                    }
                }
                if o["msg"]["type"] == "user" {
                    st.busy = true;
                }
                continue;
            }
            let Some(raw) = o.get("raw").and_then(|r| r.as_str()) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(raw) else { continue };
            evs.extend(st.on_message(&v).events);
        }
        (st, evs)
    }

    pub(crate) fn final_items(evs: &[AdapterEvent]) -> Vec<ChatItem> {
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
                AdapterEvent::Delta { item, field, delta, .. } => {
                    if let Some(i) = map.get_mut(item) {
                        match field {
                            DeltaField::Text => i.text.get_or_insert_with(String::new).push_str(delta),
                            DeltaField::Output => i.output.get_or_insert_with(String::new).push_str(delta),
                        }
                    }
                }
                _ => {}
            }
        }
        order.into_iter().filter_map(|id| map.remove(&id)).collect()
    }

    #[test]
    fn pong() {
        let (st, evs) = replay("claude_pong.jsonl");
        assert!(st.session.is_some());
        let items = final_items(&evs);
        assert!(
            items.iter().any(|i| i.kind == ChatItemKind::Agent && i.text.as_deref() == Some("PONG") && i.status == ItemStatus::Completed),
            "{items:?}"
        );
        assert!(matches!(evs.last(), Some(AdapterEvent::Status { status: ChatStatus::Idle, detail: None })));
    }

    #[test]
    fn ask_touch_approval() {
        let (mut st, evs) = replay("claude_ask_touch.jsonl");
        let a = evs
            .iter()
            .find_map(|e| match e {
                AdapterEvent::ApprovalRequested(a) => Some(a.clone()),
                _ => None,
            })
            .expect("approval");
        assert_eq!(a.kind, ApprovalKind::Command);
        assert_eq!(a.command.as_deref(), Some("touch approval_test.txt"));
        let items = final_items(&evs);
        let cmd = items.iter().find(|i| i.kind == ChatItemKind::Command).expect("command item");
        assert_eq!(cmd.title.as_deref(), Some("touch approval_test.txt"));
        assert_eq!(cmd.status, ItemStatus::Completed);
        assert_eq!(cmd.exit_code, Some(0));
        // The turn finished and cleared approvals: answering now is a no-op.
        let out = st.on_cmd(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "allow".into() });
        assert!(out.to_agent.is_empty());
    }

    #[test]
    fn approve_and_deny_shapes() {
        let mut st = ClaudeState::new(launch());
        st.initialized = true;
        st.busy = true;
        let req = json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"t1","permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"ls"}],"behavior":"allow","destination":"session"}]}});
        let out = st.on_message(&req);
        let AdapterEvent::ApprovalRequested(a) = &out.events[0] else { panic!() };
        assert!(a.options.iter().any(|o| o.id == "allow_always"));
        let out = st.on_cmd(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "allow_always".into() });
        let r = &out.to_agent[0]["response"];
        assert_eq!(r["request_id"], "r1");
        assert_eq!(r["response"]["behavior"], "allow");
        assert_eq!(r["response"]["updatedInput"]["command"], "ls");
        assert_eq!(r["response"]["updatedPermissions"][0]["destination"], "session");

        let req = json!({"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/tmp/a.txt","content":"hi\n"}}});
        let out = st.on_message(&req);
        let AdapterEvent::ApprovalRequested(a) = &out.events[0] else { panic!() };
        assert_eq!(a.kind, ApprovalKind::FileChange);
        assert!(a.diff.as_deref().unwrap().contains("+hi"));
        let out = st.on_cmd(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "deny".into() });
        assert_eq!(out.to_agent[0]["response"]["response"]["behavior"], "deny");
        assert!(out.to_agent[0]["response"]["response"]["message"].is_string());
    }

    #[test]
    fn interrupt_keeps_process_usable() {
        let (st, evs) = replay("claude_interrupt.jsonl");
        assert!(!st.busy);
        let idles: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::Status { status: ChatStatus::Idle, detail } => Some(detail.clone()),
                _ => None,
            })
            .collect();
        assert!(idles.contains(&Some("interrupted".to_string())), "{idles:?}");
        let items = final_items(&evs);
        assert!(items.iter().any(|i| i.kind == ChatItemKind::Agent && i.text.as_deref() == Some("AFTER")));
        let cmd = items.iter().find(|i| i.kind == ChatItemKind::Command).unwrap();
        assert_eq!(cmd.status, ItemStatus::Declined);
        assert!(!items.iter().any(|i| i.kind == ChatItemKind::Error));
    }

    #[test]
    fn argv_modes() {
        let mut l = launch();
        l.approval = ApprovalMode::Yolo;
        l.resume = Some("abc".into());
        l.model = Some("sonnet".into());
        let a = argv(&l);
        assert!(a.windows(2).any(|w| w[0] == "--permission-mode" && w[1] == "bypassPermissions"));
        assert!(a.windows(2).any(|w| w[0] == "--resume" && w[1] == "abc"));
        assert!(a.windows(2).any(|w| w[0] == "--model" && w[1] == "sonnet"));
        assert_eq!(parse_exit_code("Error: Exit code 2\nboom"), Some(2));
        // Needed so a session started in ask mode can later switch to bypassPermissions.
        assert!(argv(&launch()).iter().any(|a| a == "--allow-dangerously-skip-permissions"));
    }

    /// Full access mid-session: set_permission_mode is sent, pending tool prompts are allowed,
    /// questions stay; a refused switch is reported and rolled back.
    #[test]
    fn switch_mode_mid_session() {
        let mut st = ClaudeState::new(launch());
        st.initialized = true;
        st.busy = true;
        let bash = json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"t1"}});
        let ask = json!({"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[]},"tool_use_id":"t2"}});
        st.on_message(&bash);
        st.on_message(&ask);
        assert_eq!(st.approvals.len(), 2);

        let out = st.on_cmd(AdapterCmd::SetApprovalMode(ApprovalMode::Yolo));
        let req = out.to_agent.iter().find(|m| m["type"] == "control_request").expect("set_permission_mode");
        assert_eq!(req["request"], json!({"subtype": "set_permission_mode", "mode": "bypassPermissions"}));
        let allowed = out.to_agent.iter().find(|m| m["type"] == "control_response").expect("pending bash allowed");
        assert_eq!(allowed["response"]["request_id"], "r1");
        assert_eq!(allowed["response"]["response"]["behavior"], "allow");
        assert_eq!(st.approvals.len(), 1, "AskUserQuestion stays with the user");

        // Claude refuses the switch: an error line, and the previous mode is reported back.
        let rid = req["request_id"].as_str().unwrap().to_string();
        let out = st.on_message(&json!({"type":"control_response","response":{"subtype":"error","request_id":rid,"error":"not allowed"}}));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.kind == ChatItemKind::Error)));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::ApprovalMode(ApprovalMode::Ask))));
        assert_eq!(st.mode, ApprovalMode::Ask);

        // In ask mode a new prompt is shown again.
        let out = st.on_message(&json!({"type":"control_request","request_id":"r3","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"pwd"},"tool_use_id":"t3"}}));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::ApprovalRequested(_))));
    }

    /// Full access approves prompts that race the switch; once Claude confirmed bypassPermissions,
    /// what it still asks (an explicit `ask` rule, a safety check) goes to the user.
    #[test]
    fn yolo_auto_allow_only_until_confirmed() {
        let bash = |rid: &str| json!({"type":"control_request","request_id":rid,"request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf build"},"tool_use_id":format!("t-{rid}")}});
        let mut st = ClaudeState::new(launch());
        st.initialized = true;
        st.busy = true;
        let out = st.on_cmd(AdapterCmd::SetApprovalMode(ApprovalMode::Yolo));
        let rid = out.to_agent[0]["request_id"].as_str().unwrap().to_string();
        // Sent by Claude before it applied the switch: allowed without asking.
        let out = st.on_message(&bash("r1"));
        assert!(out.events.is_empty(), "{:?}", out.events);
        assert_eq!(out.to_agent[0]["response"]["response"]["behavior"], "allow");
        // Claude confirms; a prompt after that is its own decision and goes to the user.
        let out = st.on_message(&json!({"type":"control_response","response":{"subtype":"success","request_id":rid}}));
        assert!(out.events.is_empty() && out.to_agent.is_empty());
        let out = st.on_message(&bash("r2"));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::ApprovalRequested(_))));
        assert!(out.to_agent.is_empty());
    }

    /// Ask -> Yolo -> Ask quickly, the first switch refused: the later mode stays in effect.
    #[test]
    fn refused_switch_does_not_undo_a_later_one() {
        let mut st = ClaudeState::new(launch());
        st.initialized = true;
        let first = st.on_cmd(AdapterCmd::SetApprovalMode(ApprovalMode::Yolo)).to_agent[0]["request_id"].as_str().unwrap().to_string();
        st.on_cmd(AdapterCmd::SetApprovalMode(ApprovalMode::Auto));
        let out = st.on_message(&json!({"type":"control_response","response":{"subtype":"error","request_id":first,"error":"denied"}}));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.kind == ChatItemKind::Error)));
        assert!(!out.events.iter().any(|e| matches!(e, AdapterEvent::ApprovalMode(_))));
        assert_eq!(st.mode, ApprovalMode::Auto);
    }

    /// set_model goes out as a control request and is reported at once; a refused switch
    /// leaves a note and reports the previous model again.
    #[test]
    fn switch_model() {
        let mut st = ClaudeState::new(launch());
        st.initialized = true;
        st.on_message(&json!({"type":"system","subtype":"init","session_id":"s1","model":"claude-sonnet-4-5"}));
        let out = st.on_cmd(AdapterCmd::SetModel("opus".into()));
        let req = &out.to_agent[0];
        assert_eq!(req["type"], "control_request");
        assert!(req["request_id"].as_str().unwrap().starts_with("req_model_"));
        assert_eq!(req["request"], json!({"subtype": "set_model", "model": "opus"}));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Model(m) if m == "opus")));

        // Accepted: nothing more to report.
        let rid = req["request_id"].as_str().unwrap().to_string();
        let out = st.on_message(&json!({"type":"control_response","response":{"subtype":"success","request_id":rid}}));
        assert!(out.events.is_empty() && out.to_agent.is_empty());
        assert_eq!(st.model.as_deref(), Some("opus"));

        // Refused: a system line and the model that still applies.
        let out = st.on_cmd(AdapterCmd::SetModel("nope".into()));
        let rid = out.to_agent[0]["request_id"].as_str().unwrap().to_string();
        let out = st.on_message(&json!({"type":"control_response","response":{"subtype":"error","request_id":rid,"error":"unknown model"}}));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.kind == ChatItemKind::System && i.text.as_deref().unwrap_or("").contains("unknown model"))));
        assert!(out.events.iter().any(|e| matches!(e, AdapterEvent::Model(m) if m == "opus")));
        assert_eq!(st.model.as_deref(), Some("opus"));
    }

    /// Real run (Claude Code 2.1.63): the Agent tool launches one sub-agent; its Bash call asks
    /// for permission.
    #[test]
    fn subagent_card_thread_and_approval_from_recording() {
        let (st, evs) = replay("claude_subagent_approval.jsonl");
        let task = "toolu_01E2kwUwr4aBP5rWHFNnC2FP";
        let items = final_items(&evs);
        let cards: Vec<&ChatItem> = items.iter().filter(|i| i.kind == ChatItemKind::Subagent).collect();
        assert_eq!(cards.len(), 1, "{items:?}");
        let card = cards[0];
        assert_eq!(card.id, task);
        assert!(card.thread.is_none());
        let sub = card.subagent.as_ref().unwrap();
        assert_eq!(sub.id, task);
        assert_eq!(sub.name.as_deref(), Some("Run touch command"));
        assert_eq!(sub.role.as_deref(), Some("general-purpose"));
        assert_eq!(sub.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(sub.status, SubagentStatus::Done);
        assert_eq!(sub.reply.as_deref(), Some("DONE"));
        assert_eq!(card.status, ItemStatus::Completed);
        assert!(card.text.as_deref().unwrap().starts_with("Run the Bash command: touch"));
        assert!(evs.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.id == task && i.status == ItemStatus::InProgress)));

        // Its task, its Bash call and the result are in its thread.
        let thread: Vec<&ChatItem> = items.iter().filter(|i| i.thread.as_deref() == Some(task)).collect();
        assert!(thread.iter().any(|i| i.kind == ChatItemKind::User && i.text.as_deref().unwrap().contains("touch")), "{thread:?}");
        let bash = thread.iter().find(|i| i.kind == ChatItemKind::Command).expect("sub-agent Bash");
        assert_eq!(bash.status, ItemStatus::Completed);
        assert_eq!(bash.exit_code, Some(0));
        // Claude does not stream the sub-agent's last message: the Task result closes its thread.
        assert_eq!(thread.last().unwrap().kind, ChatItemKind::Agent);
        assert_eq!(thread.last().unwrap().text.as_deref(), Some("DONE"));
        // The parent's own answer stays in the chat.
        assert!(items.iter().any(|i| i.thread.is_none() && i.kind == ChatItemKind::Agent && i.text.as_deref() == Some("DONE")));

        let approval = evs
            .iter()
            .find_map(|e| match e {
                AdapterEvent::ApprovalRequested(a) => Some(a.clone()),
                _ => None,
            })
            .expect("sub-agent approval");
        assert_eq!(approval.thread.as_deref(), Some(task));
        assert_eq!(approval.thread_name.as_deref(), Some("Run touch command"));
        assert_eq!(approval.command.as_deref(), Some("touch /tmp/work/cl1/sub_approval.txt"));
        assert!(st.cards.values().all(|c| !subagent_active(c)));
    }

    /// Answering the sub-agent's request uses the request id Claude sent.
    #[test]
    fn subagent_approval_answer() {
        let path = format!("{}/tests/fixtures/claude_subagent_approval.jsonl", env!("CARGO_MANIFEST_DIR"));
        let mut st = ClaudeState::new(launch());
        st.initialized = true;
        st.busy = true;
        let mut approval = None;
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let o: Value = serde_json::from_str(line).unwrap();
            let Some(raw) = o.get("raw").and_then(|r| r.as_str()) else { continue };
            let v: Value = serde_json::from_str(raw).unwrap();
            let out = st.on_message(&v);
            if let Some(a) = out.events.iter().find_map(|e| match e {
                AdapterEvent::ApprovalRequested(a) => Some(a.clone()),
                _ => None,
            }) {
                approval = Some(a);
                break;
            }
        }
        let a = approval.unwrap();
        let out = st.on_cmd(AdapterCmd::Approve { approval_id: a.id.clone(), option_id: "allow".into() });
        assert_eq!(out.to_agent[0]["response"]["request_id"], "d68e2338-63c8-4f93-8e4f-fcb7b21e9b95");
        assert_eq!(out.to_agent[0]["response"]["response"]["behavior"], "allow");
    }

    /// A background sub-agent outlives the turn; Claude's task notification ends its card.
    #[test]
    fn background_subagent_until_notification() {
        let mut st = ClaudeState::new(launch());
        st.initialized = true;
        st.busy = true;
        let task = "toolu_bg";
        st.on_message(&json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m1", "model": "x", "content": [
            {"type": "tool_use", "id": task, "name": "Task", "input": {"description": "scan", "subagent_type": "Explore", "prompt": "scan repo", "run_in_background": true}}]}}));
        st.on_message(&json!({"type": "system", "subtype": "task_started", "task_id": "a1", "tool_use_id": task}));
        st.on_message(&json!({"type": "user", "parent_tool_use_id": null, "message": {"content": [{"type": "tool_result", "tool_use_id": task, "content": "launched"}]},
            "tool_use_result": {"status": "async_launched", "agentId": "a1", "prompt": "scan repo"}}));
        let out = st.on_message(&json!({"type": "result", "subtype": "success", "is_error": false, "session_id": "s"}));
        assert!(!out.events.iter().any(|e| matches!(e, AdapterEvent::Item(i) if i.id == task)), "still running after the turn");
        assert!(subagent_active(&st.cards[task]));
        // Its approval is attributed by agent id.
        let out = st.on_message(&json!({"type": "control_request", "request_id": "r1", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}, "tool_use_id": "toolu_x", "agent_id": "a1"}}));
        let a = out.events.iter().find_map(|e| match e { AdapterEvent::ApprovalRequested(a) => Some(a.clone()), _ => None }).unwrap();
        assert_eq!(a.thread.as_deref(), Some(task));
        assert_eq!(a.thread_name.as_deref(), Some("scan"));
        let out = st.on_message(&json!({"type": "system", "subtype": "task_notification", "task_id": "a1", "tool_use_id": task, "status": "completed", "summary": "found 3 files"}));
        let card = out.events.iter().find_map(|e| match e { AdapterEvent::Item(i) => Some(i.clone()), _ => None }).unwrap();
        let sub = card.subagent.unwrap();
        assert_eq!(sub.status, SubagentStatus::Done);
        assert_eq!(sub.reply.as_deref(), Some("found 3 files"));
    }

    /// Interrupting the turn interrupts its foreground sub-agents.
    #[test]
    fn interrupted_turn_interrupts_subagent() {
        let mut st = ClaudeState::new(launch());
        st.initialized = true;
        st.busy = true;
        st.on_message(&json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m1", "content": [
            {"type": "tool_use", "id": "toolu_t", "name": "Agent", "input": {"description": "d", "prompt": "p"}}]}}));
        let out = st.on_message(&json!({"type": "result", "subtype": "error_during_execution", "stop_reason": "tool_use", "is_error": true}));
        let card = out.events.iter().find_map(|e| match e { AdapterEvent::Item(i) if i.kind == ChatItemKind::Subagent => Some(i.clone()), _ => None }).unwrap();
        assert_eq!(card.subagent.unwrap().status, SubagentStatus::Interrupted);
        assert_eq!(card.status, ItemStatus::Declined);
    }
}
