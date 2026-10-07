//! The supervisor process: owns one PTY + child, logs output, serves local clients.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use interprocess::local_socket::tokio::{prelude::*, Stream};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::{broadcast, mpsc};

use crate::ipc::{read_frame, write_frame, ExitInfo, SupEvent, SupRequest, SupervisorInfo};
use crate::launch::{resolve_command, SupervisorArgs};
use crate::log::{write_exit, PtyLog};
use crate::now_ms;

/// Max bytes replayed per subscribe from the log before switching to a snapshot.
const MAX_REPLAY: usize = 2 * 1024 * 1024;
const SCROLLBACK: usize = 1000;

struct Shared {
    args: SupervisorArgs,
    started_at: u64,
    child_pid: Option<u32>,
    state: Mutex<State>,
    live: broadcast::Sender<SupEvent>,
    input: mpsc::UnboundedSender<Vec<u8>>,
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    shutdown: tokio::sync::Notify,
}

struct State {
    log: PtyLog,
    parser: vt100::Parser<TitleCb>,
    cols: u16,
    rows: u16,
    exited: Option<ExitInfo>,
}

#[derive(Default)]
struct TitleCb {
    title: Option<String>,
    changed: bool,
}

impl vt100::Callbacks for TitleCb {
    fn set_window_title(&mut self, _screen: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
        self.changed = true;
    }
}

/// Entry point for the supervisor process. Blocks until the supervisor exits.
pub fn supervisor_main(args: SupervisorArgs) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(run(args))
}

async fn run(args: SupervisorArgs) -> Result<()> {
    std::fs::create_dir_all(&args.dir)?;
    eprintln!("[{}] supervisor {} starting: {:?}", now_ms(), std::process::id(), args.argv);

    // Bind the socket first so a second supervisor for the same session fails fast.
    let listener = crate::local::bind(&args.dir, &args.id)?;

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize { rows: args.rows.max(1), cols: args.cols.max(1), pixel_width: 0, pixel_height: 0 })
        .context("openpty")?;

    let env_path = args.env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok());
    let argv = resolve_command(&args.argv, args.login_shell, env_path.as_deref())?;
    let mut cmd = CommandBuilder::from_argv(argv.iter().map(Into::into).collect());
    cmd.cwd(&args.cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("YONDER_SESSION", &args.id);
    for (k, v) in &args.env {
        cmd.env(k, v);
    }
    let mut child = pair.slave.spawn_command(cmd).with_context(|| format!("spawn {argv:?}"))?;
    drop(pair.slave);
    let child_pid = child.process_id();
    eprintln!("[{}] child pid {:?}", now_ms(), child_pid);

    #[cfg(windows)]
    let _job = child_pid.and_then(|pid| win_job::assign(pid).map_err(|e| eprintln!("job object: {e}")).ok());

    let reader = pair.master.try_clone_reader().context("pty reader")?;
    let writer = pair.master.take_writer().context("pty writer")?;
    let killer = child.clone_killer();

    let log = PtyLog::open(&args.dir)?;
    let (live, _) = broadcast::channel(1024);
    let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let shared = Arc::new(Shared {
        started_at: now_ms(),
        child_pid,
        state: Mutex::new(State {
            log,
            parser: vt100::Parser::new_with_callbacks(args.rows.max(1), args.cols.max(1), SCROLLBACK, TitleCb::default()),
            cols: args.cols.max(1),
            rows: args.rows.max(1),
            exited: None,
        }),
        live,
        input: input_tx,
        master: Mutex::new(pair.master),
        killer: Mutex::new(killer),
        shutdown: tokio::sync::Notify::new(),
        args,
    });

    spawn_writer(writer, input_rx);
    let reader_done = spawn_reader(shared.clone(), reader);

    // Child waiter: blocking wait on a thread, then record exit once output is drained.
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let status = child.wait();
        let _ = exit_tx.send(status);
    });
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            let status = exit_rx.await.ok().and_then(|r| r.ok());
            // Give the reader a moment to drain the last output (EOF usually follows).
            let _ = tokio::time::timeout(Duration::from_secs(2), reader_done).await;
            let exit = ExitInfo {
                code: status.as_ref().map(|s| s.exit_code() as i32),
                signal: status.as_ref().and_then(|s| s.signal().map(str::to_string)),
                ended_at: now_ms(),
            };
            eprintln!("[{}] child exited: {:?}", now_ms(), exit);
            let _ = write_exit(&shared.args.dir, &exit);
            shared.state.lock().unwrap().exited = Some(exit.clone());
            let _ = shared.live.send(SupEvent::Exited { exit });
            let linger = shared.args.linger_secs;
            let sh = shared.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(linger)).await;
                sh.shutdown.notify_one();
            });
        });
    }

    loop {
        tokio::select! {
            conn = listener.accept() => match conn {
                Ok(conn) => {
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_conn(shared, conn).await {
                            eprintln!("[{}] client error: {e}", now_ms());
                        }
                    });
                }
                Err(e) => {
                    eprintln!("[{}] accept error: {e}", now_ms());
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            },
            _ = shared.shutdown.notified() => break,
        }
    }
    eprintln!("[{}] supervisor exiting", now_ms());
    crate::local::cleanup(&shared.args.dir);
    Ok(())
}

fn spawn_writer(mut writer: Box<dyn Write + Send>, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    std::thread::spawn(move || {
        while let Some(buf) = rx.blocking_recv() {
            if writer.write_all(&buf).and_then(|_| writer.flush()).is_err() {
                break;
            }
        }
    });
}

fn spawn_reader(shared: Arc<Shared>, mut reader: Box<dyn Read + Send>) -> tokio::sync::oneshot::Receiver<()> {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let data = &buf[..n];
            // Answer cursor position requests (DSR 6) ourselves: ConPTY sends one on
            // startup and blocks output until it is answered, and agents like to probe too.
            // Clients never answer (they would race each other).
            let dsr = count_dsr(data);
            let (offset, title) = {
                let mut st = shared.state.lock().unwrap();
                let off = match st.log.append(data) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("log append: {e}");
                        st.log.end()
                    }
                };
                st.parser.process(data);
                let cb = st.parser.callbacks_mut();
                let title = if cb.changed {
                    cb.changed = false;
                    cb.title.clone()
                } else {
                    None
                };
                if dsr > 0 {
                    let (row, col) = st.parser.screen().cursor_position();
                    let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                    for _ in 0..dsr {
                        let _ = shared.input.send(reply.clone().into_bytes());
                    }
                }
                (off, title)
            };
            let _ = shared.live.send(SupEvent::Output { offset, data: B64.encode(data) });
            if let Some(title) = title {
                let _ = shared.live.send(SupEvent::Title { title });
            }
        }
        let _ = done_tx.send(());
    });
    done_rx
}

/// Count `ESC [ 6 n` (device status report: cursor position) sequences.
fn count_dsr(data: &[u8]) -> usize {
    data.windows(4).filter(|w| w == b"\x1b[6n").count()
}

fn info(shared: &Shared) -> SupervisorInfo {
    let st = shared.state.lock().unwrap();
    let screen = st.parser.screen();
    let (_, cols) = screen.size();
    let preview = screen
        .rows(0, cols)
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .last()
        .map(|r| r.chars().take(160).collect::<String>());
    SupervisorInfo {
        id: shared.args.id.clone(),
        supervisor_pid: std::process::id(),
        child_pid: shared.child_pid,
        cols: st.cols,
        rows: st.rows,
        started_at: shared.started_at,
        offset: st.log.end(),
        exited: st.exited.clone(),
        title: st.parser.callbacks().title.clone(),
        preview,
    }
}

fn snapshot(shared: &Shared) -> SupEvent {
    let st = shared.state.lock().unwrap();
    let screen = st.parser.screen();
    let mut data = Vec::new();
    // Reset attributes, clear, home; then repaint and restore cursor + input modes.
    data.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
    data.extend_from_slice(&screen.state_formatted());
    let (row, col) = screen.cursor_position();
    data.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
    data.extend_from_slice(&screen.cursor_state_formatted());
    SupEvent::Snapshot { data: B64.encode(&data), offset: st.log.end(), cols: st.cols, rows: st.rows }
}

async fn serve_conn(shared: Arc<Shared>, conn: Stream) -> Result<()> {
    let (mut rx, mut tx) = conn.split();
    let (out_tx, mut out_rx) = mpsc::channel::<SupEvent>(256);

    let writer = tokio::spawn(async move {
        while let Some(ev) = out_rx.recv().await {
            if write_frame(&mut tx, &ev).await.is_err() {
                break;
            }
        }
    });

    let mut sub_task: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(req) = read_frame::<_, SupRequest>(&mut rx).await? {
        match req {
            SupRequest::Hello => {
                let _ = out_tx.send(SupEvent::Info { info: info(&shared) }).await;
            }
            SupRequest::Input { data } => {
                if let Ok(bytes) = B64.decode(data) {
                    let _ = shared.input.send(bytes);
                }
            }
            SupRequest::Resize { cols, rows } => {
                let (cols, rows) = (cols.clamp(2, 1000), rows.clamp(1, 500));
                let changed = {
                    let mut st = shared.state.lock().unwrap();
                    if st.cols != cols || st.rows != rows {
                        st.cols = cols;
                        st.rows = rows;
                        st.parser.screen_mut().set_size(rows, cols);
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    let r = shared
                        .master
                        .lock()
                        .unwrap()
                        .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                    if let Err(e) = r {
                        eprintln!("resize: {e}");
                    }
                    let _ = shared.live.send(SupEvent::Resized { cols, rows });
                }
            }
            SupRequest::Kill => kill_tree(&shared),
            SupRequest::Snapshot => {
                let _ = out_tx.send(snapshot(&shared)).await;
            }
            SupRequest::Subscribe { from_offset } => {
                if let Some(t) = sub_task.take() {
                    t.abort();
                }
                // Subscribe to live before reading history so nothing falls in between.
                let mut live = shared.live.subscribe();
                let (replay, mut next) = {
                    let st = shared.state.lock().unwrap();
                    let end = st.log.end();
                    match from_offset {
                        Some(from) if from <= end && end - from <= MAX_REPLAY as u64 => {
                            match st.log.read_from(from, MAX_REPLAY) {
                                Ok(Some(bytes)) => (Some((from, bytes)), end),
                                _ => (None, end),
                            }
                        }
                        _ => (None, end),
                    }
                };
                match replay {
                    Some((from, bytes)) => {
                        // Always answer with exactly one initial event (possibly empty) so
                        // clients can tell replay from snapshot.
                        let _ = out_tx.send(SupEvent::Output { offset: from, data: B64.encode(&bytes) }).await;
                    }
                    None => {
                        let snap = snapshot(&shared);
                        if let SupEvent::Snapshot { offset, .. } = &snap {
                            next = *offset;
                        }
                        let _ = out_tx.send(snap).await;
                    }
                }
                let out = out_tx.clone();
                sub_task = Some(tokio::spawn(async move {
                    loop {
                        match live.recv().await {
                            Ok(SupEvent::Output { offset, data }) => {
                                // Drop bytes already delivered by replay/snapshot.
                                let Ok(bytes) = B64.decode(&data) else { continue };
                                let end = offset + bytes.len() as u64;
                                if end <= next {
                                    continue;
                                }
                                let skip = next.saturating_sub(offset) as usize;
                                let ev = if skip > 0 {
                                    SupEvent::Output { offset: next, data: B64.encode(&bytes[skip..]) }
                                } else {
                                    SupEvent::Output { offset, data }
                                };
                                next = end;
                                if out.send(ev).await.is_err() {
                                    break;
                                }
                            }
                            Ok(ev) => {
                                if out.send(ev).await.is_err() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                // Too slow: resync with a fresh snapshot.
                                let _ = out.send(SupEvent::Error { message: "lagged".into() }).await;
                                break;
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }));
            }
            SupRequest::Shutdown => {
                if shared.state.lock().unwrap().exited.is_none() {
                    kill_tree(&shared);
                }
                shared.shutdown.notify_one();
            }
        }
    }
    if let Some(t) = sub_task {
        t.abort();
    }
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

fn kill_tree(shared: &Arc<Shared>) {
    #[cfg(unix)]
    if let Some(pid) = shared.child_pid {
        let pgid = pid as libc::pid_t;
        unsafe {
            libc::kill(-pgid, libc::SIGHUP);
            libc::kill(-pgid, libc::SIGTERM);
        }
        let sh = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            if sh.state.lock().unwrap().exited.is_none() {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
                let _ = sh.killer.lock().unwrap().kill();
            }
        });
        return;
    }
    #[cfg(windows)]
    {
        win_job::terminate();
    }
    let _ = shared.killer.lock().unwrap().kill();
}

#[cfg(windows)]
mod win_job {
    //! Job object so killing a session kills the whole ConPTY process tree.
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    struct Job(HANDLE);
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    static JOB: OnceLock<Job> = OnceLock::new();

    pub fn assign(pid: u32) -> Result<(), String> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err("CreateJobObjectW failed".into());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if proc.is_null() {
                CloseHandle(job);
                return Err("OpenProcess failed".into());
            }
            let ok = AssignProcessToJobObject(job, proc);
            CloseHandle(proc);
            if ok == 0 {
                CloseHandle(job);
                return Err("AssignProcessToJobObject failed".into());
            }
            let _ = JOB.set(Job(job));
            Ok(())
        }
    }

    pub fn terminate() {
        if let Some(job) = JOB.get() {
            unsafe {
                TerminateJobObject(job.0, 1);
            }
        }
    }
}
