//! Helpers shared by adapters.

use yonder_proto::app::{ChatItem, ChatItemKind, ItemStatus, Subagent, SubagentStatus};

pub use yonder_proto::app::now_ms;

/// Keep at most `max` bytes, preferring the tail (most recent output).
pub fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("[... {} bytes omitted ...]\n{}", start, &s[start..])
}

pub const MAX_OUTPUT: usize = 64 * 1024;

pub fn item(id: impl Into<String>, kind: ChatItemKind, status: ItemStatus) -> ChatItem {
    ChatItem::new(id, kind, status, now_ms())
}

/// A new sub-agent card (`kind == subagent`) for the agent whose thread is `thread` (empty
/// until the agent exists).
pub fn subagent_card(id: impl Into<String>, thread: impl Into<String>, status: SubagentStatus) -> ChatItem {
    let mut it = item(id, ChatItemKind::Subagent, ItemStatus::InProgress);
    it.subagent = Some(Subagent { id: thread.into(), name: None, role: None, model: None, status, reply: None });
    set_subagent_status(&mut it, status);
    it
}

/// Sets a card's sub-agent status and the item status that goes with it. Returns whether
/// anything changed.
pub fn set_subagent_status(it: &mut ChatItem, status: SubagentStatus) -> bool {
    let item_status = match status {
        SubagentStatus::Running => ItemStatus::InProgress,
        SubagentStatus::Done | SubagentStatus::Closed => ItemStatus::Completed,
        SubagentStatus::Interrupted => ItemStatus::Declined,
        SubagentStatus::Failed => ItemStatus::Failed,
    };
    let Some(sub) = it.subagent.as_mut() else { return false };
    let changed = sub.status != status || it.status != item_status;
    sub.status = status;
    it.status = item_status;
    changed
}

/// The card's agent is still running.
pub fn subagent_active(it: &ChatItem) -> bool {
    it.subagent.as_ref().is_some_and(|s| s.status == SubagentStatus::Running)
}

pub fn random_id() -> String {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Strip ANSI escape sequences (pi status texts, some tool output).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('@'..='~').contains(&c2) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                for c2 in chars.by_ref() {
                    if c2 == '\u{7}' {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(truncate_tail("abc", 10), "abc");
        let t = truncate_tail("héllo world", 5);
        assert!(t.ends_with("world"));
        assert_eq!(strip_ansi("\u{1b}[38;5;242m24h\u{1b}[39m ok"), "24h ok");
    }
}
