//! Structured "chat mode" adapters for coding agents.
//!
//! Each adapter runs one agent process in its machine-readable mode and normalizes its
//! protocol into yonder's chat model ([`ChatItem`], [`Approval`], status):
//! - Codex: `codex app-server` (JSON-RPC 2.0 over stdio).
//! - Claude Code: `claude -p --input-format stream-json --output-format stream-json
//!   --permission-prompt-tool stdio` (SDK control protocol for approvals).
//! - pi: `pi --mode rpc` (JSONL commands and events; pi does not gate tools).

mod claude;
mod codex;
pub mod common;
pub mod detect;
mod driver;
mod history;
mod pi;
pub mod proc;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tokio::sync::mpsc;
use yonder_proto::app::{AgentKind, Approval, ApprovalMode, ChatItem, ChatStatus, DeltaField};

pub use detect::{detect_agents, program_for, resolve_program, user_path};
pub use history::{
    agent_preview, claude_default_mode, codex_config_mode, codex_models, invalidate_history, is_temp_dir, list_agent_history, list_history, pi_models,
    CodexInfo, HistoryPage, HistoryQuery,
};
pub use proc::kill_tree;

/// Everything needed to start one agent in chat mode.
#[derive(Debug, Clone)]
pub struct AgentLaunch {
    pub agent: AgentKind,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub approval: ApprovalMode,
    /// Agent session id to resume.
    pub resume: Option<String>,
    /// Extra environment variables for the agent process.
    pub env: BTreeMap<String, String>,
    /// Program (plus leading arguments) to run instead of the default executable.
    pub program: Option<Vec<String>>,
    /// Unix: run through `$SHELL -lc` so PATH matches an interactive login.
    pub login_shell: bool,
}

/// Commands from the supervisor to a running adapter.
#[derive(Debug, Clone)]
pub enum AdapterCmd {
    Send { text: String, attachments: Vec<PathBuf> },
    Interrupt,
    Approve { approval_id: String, option_id: String },
    /// Change the approval mode in place (Codex, Claude). Pending approvals that the new
    /// mode would not ask about are approved.
    SetApprovalMode(ApprovalMode),
    /// Switch the model (Codex: from the next turn; Claude, pi: right away).
    SetModel(String),
    Shutdown,
}

/// Normalized events from a running adapter.
#[derive(Debug, Clone)]
pub enum AdapterEvent {
    /// Insert or replace an item (by id).
    Item(ChatItem),
    /// Append text to an item field. `thread`: the item belongs to that sub-agent's thread.
    Delta { item: String, field: DeltaField, delta: String, thread: Option<String> },
    Status { status: ChatStatus, detail: Option<String> },
    ApprovalRequested(Approval),
    ApprovalResolved { approval: String, option: String },
    /// The agent's own session/thread id (for resume).
    AgentSession(String),
    Model(String),
    /// The approval mode now in effect.
    ApprovalMode(ApprovalMode),
    /// The agent process ended; the adapter is finished.
    Exited { code: Option<i32>, error: Option<String> },
}

/// Handle to a running adapter.
pub struct AdapterHandle {
    pub agent: AgentKind,
    pub pid: Option<u32>,
    pub cmds: mpsc::Sender<AdapterCmd>,
    pub events: mpsc::Receiver<AdapterEvent>,
}

/// Start an agent in chat mode. Must be called inside a tokio runtime.
pub fn spawn_adapter(launch: AgentLaunch) -> anyhow::Result<AdapterHandle> {
    match launch.agent {
        AgentKind::Codex => codex::spawn(launch),
        AgentKind::Claude => claude::spawn(launch),
        AgentKind::Pi => pi::spawn(launch),
        other => anyhow::bail!("{} has no chat mode; use a terminal session", other.as_str()),
    }
}

/// CLI argv for a terminal (TUI) session of an agent, optionally resuming its session.
pub fn terminal_argv(agent: AgentKind, resume: Option<&str>, model: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = match agent {
        AgentKind::Codex => vec!["codex".into()],
        AgentKind::Claude => vec!["claude".into()],
        AgentKind::Pi => vec!["pi".into()],
        AgentKind::Shell | AgentKind::Custom => return Vec::new(),
    };
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        match agent {
            AgentKind::Codex => v.extend(["-m".into(), m.into()]),
            _ => v.extend(["--model".into(), m.into()]),
        }
    }
    if let Some(id) = resume.filter(|s| !s.is_empty()) {
        match agent {
            AgentKind::Codex => v.extend(["resume".into(), id.into()]),
            AgentKind::Claude => v.extend(["--resume".into(), id.into()]),
            AgentKind::Pi => v.extend(["--session".into(), id.into()]),
            _ => {}
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_argv_resume() {
        assert_eq!(terminal_argv(AgentKind::Codex, Some("t1"), None), vec!["codex", "resume", "t1"]);
        assert_eq!(terminal_argv(AgentKind::Claude, Some("s1"), Some("opus")), vec!["claude", "--model", "opus", "--resume", "s1"]);
        assert_eq!(terminal_argv(AgentKind::Pi, None, None), vec!["pi"]);
        assert!(terminal_argv(AgentKind::Shell, None, None).is_empty());
    }
}
