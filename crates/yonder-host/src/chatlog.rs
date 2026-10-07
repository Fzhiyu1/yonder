//! Chat session state and its append-only event log (`events.jsonl`).
//!
//! The chat supervisor owns the authoritative [`ChatState`]; every client-visible change is
//! one [`ChatEv`] with a per-session sequence number (starting at 1, no gaps). Metadata
//! (agent session id, model, exit) is logged without a sequence number. The daemon rebuilds
//! the state from the log for sessions whose supervisor is gone.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use yonder_proto::app::{
    Approval, ApprovalMode, ChatItem, ChatItemKind, ChatSnapshot, ChatStatus, DeltaField, Event, ItemStatus,
};

pub const EVENTS_FILE: &str = "events.jsonl";
/// Items kept in memory (oldest dropped beyond this).
const MAX_ITEMS: usize = 1000;
/// Items sent in one snapshot (supervisor -> daemon).
const SNAPSHOT_ITEMS: usize = 300;
/// Items in the first page a client gets; older ones are fetched with `chat_older`.
pub const PAGE_ITEMS: usize = 40;
/// Cap for command output and reasoning text in pages sent to clients.
const PAGE_OUTPUT: usize = 8 * 1024;
/// Cap for accumulated command output (bytes); the tail is kept.
const MAX_ITEM_OUTPUT: usize = 256 * 1024;
/// Cap for text / diff inside snapshots.
const SNAPSHOT_OUTPUT: usize = 32 * 1024;

/// One client-visible chat change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum ChatEv {
    Item { item: ChatItem },
    Delta { item: String, field: DeltaField, delta: String },
    Status {
        status: ChatStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    ApprovalRequested { approval: Approval },
    ApprovalResolved { approval: String, option: String },
}

impl ChatEv {
    /// The chat change carried by an app event: `(session, seq, change)`.
    pub fn from_event(ev: &Event) -> Option<(String, u64, ChatEv)> {
        Some(match ev.clone() {
            Event::ChatItem { session, seq, item } => (session, seq, ChatEv::Item { item }),
            Event::ChatDelta { session, seq, item, field, delta } => (session, seq, ChatEv::Delta { item, field, delta }),
            Event::ChatStatus { session, seq, status, detail } => (session, seq, ChatEv::Status { status, detail }),
            Event::ApprovalRequested { session, seq, approval } => (session, seq, ChatEv::ApprovalRequested { approval }),
            Event::ApprovalResolved { session, seq, approval, option } => {
                (session, seq, ChatEv::ApprovalResolved { approval, option })
            }
            _ => return None,
        })
    }

    pub fn to_event(&self, session: &str, seq: u64) -> Event {
        let session = session.to_string();
        match self.clone() {
            ChatEv::Item { item } => Event::ChatItem { session, seq, item },
            ChatEv::Delta { item, field, delta } => Event::ChatDelta { session, seq, item, field, delta },
            ChatEv::Status { status, detail } => Event::ChatStatus { session, seq, status, detail },
            ChatEv::ApprovalRequested { approval } => Event::ApprovalRequested { session, seq, approval },
            ChatEv::ApprovalResolved { approval, option } => {
                Event::ApprovalResolved { session, seq, approval, option }
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatExit {
    pub code: Option<i32>,
    pub error: Option<String>,
    pub ended_at: u64,
}

/// Not sequence-numbered metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum ChatMeta {
    AgentSession { id: String },
    Model { model: String },
    /// The approval mode now in effect (set at runtime).
    Approval { mode: ApprovalMode },
    Exited { exit: ChatExit },
}

/// One line of `events.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ev: Option<ChatEv>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<ChatMeta>,
}

#[derive(Debug, Clone)]
pub struct ChatState {
    items: Vec<ChatItem>,
    index: HashMap<String, usize>,
    pub approvals: Vec<Approval>,
    pub status: ChatStatus,
    pub detail: Option<String>,
    pub seq: u64,
    pub truncated: bool,
    pub agent_session: Option<String>,
    pub model: Option<String>,
    /// Approval mode changed at runtime (None: the launch mode still applies).
    pub approval: Option<ApprovalMode>,
    pub exited: Option<ChatExit>,
    /// Last agent message (for session lists and notifications).
    pub last_agent_text: Option<String>,
    /// Last user message.
    pub last_user_text: Option<String>,
}

impl Default for ChatState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            index: HashMap::new(),
            approvals: Vec::new(),
            status: ChatStatus::Starting,
            detail: None,
            seq: 0,
            truncated: false,
            agent_session: None,
            model: None,
            approval: None,
            exited: None,
            last_agent_text: None,
            last_user_text: None,
        }
    }
}

fn truncate_tail(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut start = s.len() - max / 2;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    let tail = s[start..].to_string();
    *s = format!("[... {start} bytes omitted ...]\n{tail}");
}

fn clip_tail(s: &str, max: usize) -> String {
    let mut v = s.to_string();
    truncate_tail(&mut v, max);
    v
}

/// A copy of `it` with long output, reasoning and diffs cut to their tail.
fn clip_item(it: &ChatItem, output: usize) -> ChatItem {
    let mut it = it.clone();
    if let Some(o) = &it.output {
        it.output = Some(clip_tail(o, output));
    }
    if it.kind == ChatItemKind::Reasoning {
        if let Some(t) = &it.text {
            it.text = Some(clip_tail(t, output));
        }
    }
    if let Some(d) = &it.diff {
        if d.len() > 4 * output {
            it.diff = Some(clip_tail(d, 4 * output));
        }
    }
    it
}

impl ChatState {
    pub fn items(&self) -> &[ChatItem] {
        &self.items
    }

    /// State equivalent to a snapshot (what a client holds after `attached`).
    pub fn from_snapshot(snap: &ChatSnapshot) -> Self {
        let mut st = ChatState::default();
        for item in &snap.items {
            st.upsert(item.clone());
        }
        st.approvals = snap.approvals.clone();
        st.status = snap.status;
        st.seq = snap.seq;
        st.truncated = snap.truncated;
        st
    }

    fn reindex(&mut self) {
        self.index = self.items.iter().enumerate().map(|(i, it)| (it.id.clone(), i)).collect();
    }

    fn upsert(&mut self, item: ChatItem) {
        if item.kind == ChatItemKind::Agent {
            if let Some(t) = item.text.as_ref().filter(|t| !t.trim().is_empty()) {
                self.last_agent_text = Some(t.clone());
            }
        }
        if item.kind == ChatItemKind::User {
            if let Some(t) = item.text.as_ref().filter(|t| !t.trim().is_empty()) {
                self.last_user_text = Some(t.clone());
            }
        }
        match self.index.get(&item.id) {
            Some(&i) => self.items[i] = item,
            None => {
                self.index.insert(item.id.clone(), self.items.len());
                self.items.push(item);
                if self.items.len() > MAX_ITEMS {
                    let drop = self.items.len() - MAX_ITEMS + MAX_ITEMS / 5;
                    self.items.drain(..drop);
                    self.truncated = true;
                    self.reindex();
                }
            }
        }
    }

    /// Apply one sequenced event.
    pub fn apply(&mut self, seq: u64, ev: &ChatEv) {
        self.seq = seq;
        match ev {
            ChatEv::Item { item } => self.upsert(item.clone()),
            ChatEv::Delta { item, field, delta } => {
                let idx = match self.index.get(item) {
                    Some(&i) => i,
                    None => {
                        let kind = if *field == DeltaField::Output { ChatItemKind::Command } else { ChatItemKind::Agent };
                        self.upsert(ChatItem::new(item.clone(), kind, ItemStatus::InProgress, yonder_proto::app::now_ms()));
                        self.index[item]
                    }
                };
                let it = &mut self.items[idx];
                match field {
                    DeltaField::Text => {
                        let t = it.text.get_or_insert_with(String::new);
                        t.push_str(delta);
                        if it.kind == ChatItemKind::Agent {
                            self.last_agent_text = Some(t.clone());
                        }
                    }
                    DeltaField::Output => {
                        let o = it.output.get_or_insert_with(String::new);
                        o.push_str(delta);
                        truncate_tail(o, MAX_ITEM_OUTPUT);
                    }
                }
            }
            ChatEv::Status { status, detail } => {
                self.status = *status;
                self.detail = detail.clone();
            }
            ChatEv::ApprovalRequested { approval } => {
                self.approvals.retain(|a| a.id != approval.id);
                self.approvals.push(approval.clone());
            }
            ChatEv::ApprovalResolved { approval, .. } => {
                self.approvals.retain(|a| &a.id != approval);
            }
        }
    }

    pub fn apply_meta(&mut self, meta: &ChatMeta) {
        match meta {
            ChatMeta::AgentSession { id } => self.agent_session = Some(id.clone()),
            ChatMeta::Model { model } => self.model = Some(model.clone()),
            ChatMeta::Approval { mode } => self.approval = Some(*mode),
            ChatMeta::Exited { exit } => {
                self.exited = Some(exit.clone());
                self.status = ChatStatus::Exited;
                self.approvals.clear();
            }
        }
    }

    pub fn snapshot(&self) -> ChatSnapshot {
        self.tail(SNAPSHOT_ITEMS, SNAPSHOT_OUTPUT)
    }

    /// The newest items for a client (older ones via [`ChatState::older`]).
    pub fn page(&self) -> ChatSnapshot {
        self.tail(PAGE_ITEMS, PAGE_OUTPUT)
    }

    fn tail(&self, n: usize, output: usize) -> ChatSnapshot {
        let start = self.items.len().saturating_sub(n);
        ChatSnapshot {
            items: self.items[start..].iter().map(|it| clip_item(it, output)).collect(),
            approvals: self.approvals.clone(),
            status: self.status,
            seq: self.seq,
            truncated: self.truncated || start > 0,
        }
    }

    /// Up to `limit` items right before the item `before`, oldest first, and whether even
    /// older ones exist. None when `before` is not in memory.
    pub fn older(&self, before: &str, limit: usize) -> Option<(Vec<ChatItem>, bool)> {
        let end = *self.index.get(before)?;
        if end == 0 && self.truncated {
            // Older items exist but not here (a snapshot copy): the caller reads the log.
            return None;
        }
        let start = end.saturating_sub(limit);
        let items: Vec<ChatItem> = self.items[start..end].iter().map(|it| clip_item(it, PAGE_OUTPUT)).collect();
        // Items dropped from memory cannot be served: stop at the oldest one kept.
        let more = start > 0 || (self.truncated && !items.is_empty());
        Some((items, more))
    }

    /// Short text for session lists.
    pub fn preview(&self) -> Option<String> {
        if let Some(a) = self.approvals.last() {
            return Some(crate::util::one_line(&a.title, 140));
        }
        self.last_agent_text
            .as_deref()
            .or(self.last_user_text.as_deref())
            .map(|t| crate::util::one_line(t, 140))
    }

    /// Rebuild from a session's `events.jsonl` (missing file = empty state).
    pub fn load(dir: &Path) -> Self {
        let mut st = ChatState::default();
        let Ok(f) = std::fs::File::open(dir.join(EVENTS_FILE)) else { return st };
        for line in BufReader::new(f).lines() {
            let Ok(line) = line else { break };
            let Ok(l) = serde_json::from_str::<LogLine>(&line) else { continue };
            if let (Some(seq), Some(ev)) = (l.seq, &l.ev) {
                st.apply(seq, ev);
            }
            if let Some(m) = &l.meta {
                st.apply_meta(m);
            }
        }
        st
    }
}

/// Buffered appender for `events.jsonl`.
pub struct EventLog {
    w: std::io::BufWriter<std::fs::File>,
}

impl EventLog {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(EVENTS_FILE))?;
        Ok(Self { w: std::io::BufWriter::new(f) })
    }

    pub fn append(&mut self, line: &LogLine) {
        if let Ok(s) = serde_json::to_string(line) {
            let _ = self.w.write_all(s.as_bytes());
            let _ = self.w.write_all(b"\n");
        }
    }

    pub fn flush(&mut self) {
        let _ = self.w.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yonder_proto::app::{ApprovalKind, ApprovalOption, OptionKind};

    fn agent_item(id: &str, text: &str) -> ChatItem {
        let mut it = ChatItem::new(id, ChatItemKind::Agent, ItemStatus::Completed, 1);
        it.text = Some(text.into());
        it
    }

    #[test]
    fn apply_snapshot_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = EventLog::open(dir.path()).unwrap();
        let mut st = ChatState::default();
        let approval = Approval {
            id: "a1".into(),
            kind: ApprovalKind::Command,
            title: "run ls".into(),
            command: Some("ls".into()),
            cwd: None,
            diff: None,
            reason: None,
            detail: None,
            options: vec![ApprovalOption { id: "allow".into(), label: "Allow".into(), kind: OptionKind::Allow }],
            item: None,
            ts: 1,
        };
        let evs = [
            ChatEv::Status { status: ChatStatus::Working, detail: None },
            ChatEv::Delta { item: "m1".into(), field: DeltaField::Text, delta: "hel".into() },
            ChatEv::Delta { item: "m1".into(), field: DeltaField::Text, delta: "lo".into() },
            ChatEv::ApprovalRequested { approval: approval.clone() },
            ChatEv::ApprovalResolved { approval: "a1".into(), option: "allow".into() },
            ChatEv::ApprovalRequested { approval },
            ChatEv::Item { item: agent_item("m1", "hello!") },
            ChatEv::Status { status: ChatStatus::Idle, detail: None },
        ];
        for (i, ev) in evs.iter().enumerate() {
            let seq = i as u64 + 1;
            st.apply(seq, ev);
            log.append(&LogLine { seq: Some(seq), ts: 1, ev: Some(ev.clone()), meta: None });
        }
        let meta = ChatMeta::AgentSession { id: "thread-1".into() };
        st.apply_meta(&meta);
        log.append(&LogLine { seq: None, ts: 1, ev: None, meta: Some(meta) });
        log.flush();

        let snap = st.snapshot();
        assert_eq!(snap.seq, 8);
        assert_eq!(snap.items.len(), 1);
        assert_eq!(snap.items[0].text.as_deref(), Some("hello!"));
        assert_eq!(snap.approvals.len(), 1);
        assert_eq!(snap.status, ChatStatus::Idle);
        assert_eq!(st.preview().as_deref(), Some("run ls"));

        let back = ChatState::load(dir.path());
        assert_eq!(back.seq, 8);
        assert_eq!(back.agent_session.as_deref(), Some("thread-1"));
        assert_eq!(back.snapshot().items[0].text.as_deref(), Some("hello!"));

        let mut st2 = back;
        st2.apply_meta(&ChatMeta::Exited { exit: ChatExit { code: Some(0), error: None, ended_at: 2 } });
        assert_eq!(st2.status, ChatStatus::Exited);
        assert!(st2.approvals.is_empty());
    }

    #[test]
    fn caps_items_and_output() {
        let mut st = ChatState::default();
        for i in 0..(MAX_ITEMS + 10) {
            st.apply(i as u64 + 1, &ChatEv::Item { item: agent_item(&format!("i{i}"), "x") });
        }
        assert!(st.items().len() <= MAX_ITEMS);
        assert!(st.truncated);
        assert_eq!(st.snapshot().items.len(), SNAPSHOT_ITEMS);
        let big = "y".repeat(MAX_ITEM_OUTPUT);
        st.apply(5000, &ChatEv::Delta { item: "c".into(), field: DeltaField::Output, delta: big.clone() });
        st.apply(5001, &ChatEv::Delta { item: "c".into(), field: DeltaField::Output, delta: big });
        let c = st.items().iter().find(|i| i.id == "c").unwrap();
        assert!(c.output.as_ref().unwrap().len() <= MAX_ITEM_OUTPUT + 64);
        assert_eq!(c.kind, ChatItemKind::Command);
    }

    #[test]
    fn pages_and_older() {
        let mut st = ChatState::default();
        for i in 0..100 {
            st.apply(i as u64 + 1, &ChatEv::Item { item: agent_item(&format!("i{i}"), "x") });
        }
        let page = st.page();
        assert_eq!(page.items.len(), PAGE_ITEMS);
        assert_eq!(page.items[0].id, format!("i{}", 100 - PAGE_ITEMS));
        assert!(page.truncated);
        let (older, more) = st.older(&page.items[0].id, 50).unwrap();
        assert_eq!(older.len(), 50);
        assert_eq!(older.last().unwrap().id, format!("i{}", 100 - PAGE_ITEMS - 1));
        assert!(more);
        let (rest, more) = st.older(&older[0].id, 50).unwrap();
        assert_eq!(rest.len(), 10);
        assert_eq!(rest[0].id, "i0");
        assert!(!more);
        assert!(st.older("nope", 10).is_none());
        // A copy that starts mid-chat sends callers to the log for what it lacks.
        let copy = ChatState::from_snapshot(&page);
        assert!(copy.older(&page.items[0].id, 10).is_none());
    }
}
