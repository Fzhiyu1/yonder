//! Generic process loop shared by all adapters.
//!
//! An adapter is a pure state machine ([`Protocol`]) that turns agent stdout messages and
//! [`AdapterCmd`]s into messages for the agent's stdin plus normalized [`AdapterEvent`]s.
//! Keeping I/O out of the state machines lets recorded fixtures drive the tests.

use std::time::Duration;

use anyhow::Result;
use serde_json::Value;
use tokio::process::ChildStdin;
use tokio::sync::mpsc;
use tokio::time::Instant;
use yonder_proto::app::{AgentKind, ChatStatus};

use crate::proc::{build_command, kill_tree, spawn_jsonl, stderr_summary, write_json, AgentProc};
use crate::{AdapterCmd, AdapterEvent, AdapterHandle, AgentLaunch};

/// Output of one state machine step.
#[derive(Default, Debug)]
pub struct Out {
    pub to_agent: Vec<Value>,
    pub events: Vec<AdapterEvent>,
}

impl Out {
    pub fn event(&mut self, ev: AdapterEvent) {
        self.events.push(ev);
    }
    pub fn send(&mut self, v: Value) {
        self.to_agent.push(v);
    }
    pub fn status(&mut self, status: ChatStatus, detail: Option<String>) {
        self.events.push(AdapterEvent::Status { status, detail });
    }
}

pub trait Protocol: Send + 'static {
    /// Messages to write right after the process started.
    fn start(&mut self) -> Out;
    fn on_message(&mut self, v: &Value) -> Out;
    fn on_cmd(&mut self, cmd: AdapterCmd) -> Out;
    /// The agent process is gone; close open items.
    fn on_exit(&mut self) -> Out {
        Out::default()
    }
    /// A protocol-level reason for the exit (e.g. failed handshake).
    fn exit_error(&self) -> Option<String> {
        None
    }
    /// Stop the agent process after this long with nothing going on, so it lets go of the
    /// session (Codex allows one writer per thread: the desktop app cannot continue a thread
    /// an idle Yonder chat keeps open). It starts again on the next command that needs it.
    fn idle_release(&self) -> Option<Duration> {
        None
    }
    /// Nothing is running or waiting: the process may be stopped now.
    fn can_release(&self) -> bool {
        false
    }
    /// How often the driver should call [`Protocol::on_tick`] while the process is alive.
    fn poll_interval(&self) -> Option<Duration> {
        None
    }
    /// Periodic protocol work, such as recovering state that may have been lost upstream.
    fn on_tick(&mut self) -> Out {
        Out::default()
    }
    /// The process was stopped for being idle.
    fn on_release(&mut self) -> Out {
        Out::default()
    }
    /// While released: this command needs the process (others only update state).
    fn needs_agent(&self, _cmd: &AdapterCmd) -> bool {
        true
    }
    /// Messages to write after the process was started again.
    fn restart(&mut self) -> Out {
        self.start()
    }
}

async fn flush(stdin: &mut ChildStdin, ev_tx: &mpsc::Sender<AdapterEvent>, out: Out) -> bool {
    for ev in out.events {
        let _ = ev_tx.send(ev).await;
    }
    for msg in out.to_agent {
        if let Err(e) = write_json(stdin, &msg).await {
            tracing::debug!("write to agent failed: {e}");
            return false;
        }
    }
    true
}

/// Close stdin, give the agent a moment to finish, then kill the whole tree. Returns the exit
/// status, whatever it printed before exiting, and its stderr tail.
async fn stop(p: AgentProc, pid: Option<u32>) -> (Option<std::process::ExitStatus>, Vec<Value>, StderrTail) {
    let AgentProc { mut child, stdin, mut lines, stderr_tail } = p;
    drop(stdin);
    let status = match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(s) => s.ok(),
        Err(_) => {
            if let Some(pid) = pid {
                kill_tree(pid);
            }
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    };
    let mut rest = Vec::new();
    while let Ok(v) = lines.try_recv() {
        rest.push(v);
    }
    (status, rest, stderr_tail)
}

type StderrTail = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

async fn emit(ev_tx: &mpsc::Sender<AdapterEvent>, out: Out) {
    for ev in out.events {
        let _ = ev_tx.send(ev).await;
    }
}

/// Spawn `argv` and drive it with `proto` until it exits or is shut down. A protocol with
/// `idle_release` has its process stopped while idle and started again on demand.
pub fn run<P: Protocol>(agent: AgentKind, launch: &AgentLaunch, argv: Vec<String>, mut proto: P) -> Result<AdapterHandle> {
    let launch = launch.clone();
    let first = spawn_jsonl(build_command(&argv, &launch.cwd, &launch.env, launch.login_shell)?)?;
    let first_pid = first.child.id();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<AdapterCmd>(64);
    let (ev_tx, ev_rx) = mpsc::channel::<AdapterEvent>(4096);
    tokio::spawn(async move {
        let _ = ev_tx.send(AdapterEvent::Status { status: ChatStatus::Starting, detail: None }).await;
        let mut p = first;
        let mut pid = first_pid;
        let mut alive = flush(&mut p.stdin, &ev_tx, proto.start()).await;
        let mut last = Instant::now();
        let mut last_poll = Instant::now();
        'run: while alive {
            let deadline = proto.idle_release().filter(|_| proto.can_release()).map(|d| last + d);
            let poll_deadline = proto.poll_interval().map(|d| last_poll + d);
            tokio::select! {
                line = p.lines.recv() => match line {
                    Some(v) => {
                        last = Instant::now();
                        alive = flush(&mut p.stdin, &ev_tx, proto.on_message(&v)).await;
                    }
                    None => break,
                },
                cmd = cmd_rx.recv() => match cmd {
                    None | Some(AdapterCmd::Shutdown) => break,
                    Some(c) => {
                        last = Instant::now();
                        alive = flush(&mut p.stdin, &ev_tx, proto.on_cmd(c)).await;
                    }
                },
                _ = sleep_until(poll_deadline), if poll_deadline.is_some() => {
                    last_poll = Instant::now();
                    alive = flush(&mut p.stdin, &ev_tx, proto.on_tick()).await;
                },
                _ = sleep_until(deadline), if deadline.is_some() => {
                    if !proto.can_release() {
                        last = Instant::now();
                        continue;
                    }
                    let (_, rest, _) = stop(p, pid).await;
                    for v in rest {
                        emit(&ev_tx, proto.on_message(&v)).await;
                    }
                    emit(&ev_tx, proto.on_release()).await;
                    // Released: wait for a command that needs the agent again.
                    loop {
                        match cmd_rx.recv().await {
                            None | Some(AdapterCmd::Shutdown) => {
                                emit(&ev_tx, proto.on_exit()).await;
                                let _ = ev_tx.send(AdapterEvent::Status { status: ChatStatus::Exited, detail: None }).await;
                                let _ = ev_tx.send(AdapterEvent::Exited { code: Some(0), error: None }).await;
                                return;
                            }
                            Some(c) if proto.needs_agent(&c) => {
                                match build_command(&argv, &launch.cwd, &launch.env, launch.login_shell).and_then(spawn_jsonl) {
                                    Ok(np) => {
                                        p = np;
                                        pid = p.child.id();
                                        last = Instant::now();
                                        let restart = proto.restart();
                                        let cmd_out = proto.on_cmd(c);
                                        alive = flush(&mut p.stdin, &ev_tx, restart).await && flush(&mut p.stdin, &ev_tx, cmd_out).await;
                                        continue 'run;
                                    }
                                    Err(e) => {
                                        let msg = format!("{e:#}");
                                        emit(&ev_tx, proto.on_exit()).await;
                                        let _ = ev_tx.send(AdapterEvent::Status { status: ChatStatus::Exited, detail: None }).await;
                                        let _ = ev_tx.send(AdapterEvent::Exited { code: None, error: Some(msg) }).await;
                                        return;
                                    }
                                }
                            }
                            Some(c) => emit(&ev_tx, proto.on_cmd(c)).await,
                        }
                    }
                }
            }
        }
        let (status, rest, stderr_tail) = stop(p, pid).await;
        for v in rest {
            emit(&ev_tx, proto.on_message(&v)).await;
        }
        emit(&ev_tx, proto.on_exit()).await;
        let failed = status.map(|s| !s.success()).unwrap_or(true);
        let error = proto.exit_error().or_else(|| if failed { stderr_summary(&stderr_tail) } else { None });
        let _ = ev_tx.send(AdapterEvent::Status { status: ChatStatus::Exited, detail: None }).await;
        let _ = ev_tx.send(AdapterEvent::Exited { code: status.and_then(|s| s.code()), error }).await;
    });
    Ok(AdapterHandle { agent, pid: first_pid, cmds: cmd_tx, events: ev_rx })
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}
