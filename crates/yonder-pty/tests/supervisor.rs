use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use yonder_pty::{spawn_supervisor, SupEvent, SupervisorArgs, SupervisorClient};

fn supervisor_bin() -> PathBuf {
    std::env::var_os("YONDER_PTY_SUPERVISOR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_yonder-pty-supervisor")))
}

fn args(dir: &Path, id: &str, argv: &[&str]) -> SupervisorArgs {
    SupervisorArgs {
        id: id.into(),
        dir: dir.to_path_buf(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: std::env::temp_dir(),
        env: Default::default(),
        cols: 80,
        rows: 24,
        login_shell: false,
        linger_secs: 3,
    }
}

/// Read events until the accumulated output contains `needle`. Returns (text, last offset end).
async fn read_until(c: &SupervisorClient, needle: &str, timeout: Duration) -> (String, u64) {
    let mut text = String::new();
    let mut end = 0;
    let fut = async {
        loop {
            match c.recv().await.unwrap() {
                Some(SupEvent::Output { offset, data }) => {
                    let b = B64.decode(data).unwrap();
                    end = offset + b.len() as u64;
                    text.push_str(&String::from_utf8_lossy(&b));
                }
                Some(SupEvent::Snapshot { data, offset, .. }) => {
                    text.push_str(&String::from_utf8_lossy(&B64.decode(data).unwrap()));
                    end = offset;
                }
                Some(_) => {}
                None => panic!("supervisor closed; got so far: {text:?}"),
            }
            if text.contains(needle) {
                break;
            }
        }
    };
    tokio::time::timeout(timeout, fut).await.unwrap_or_else(|_| panic!("timeout waiting for {needle:?}; got {text:?}"));
    (text, end)
}

#[cfg(unix)]
const SCRIPT: &[&str] = &["/bin/sh", "-c", "echo hello-yonder; read x; echo got:$x; sleep 30"];
#[cfg(windows)]
const SCRIPT: &[&str] = &["cmd.exe", "/d", "/c", "echo hello-yonder& set /p x=& call echo got:%x%& ping -n 30 127.0.0.1 >nul"];

#[tokio::test(flavor = "multi_thread")]
async fn io_resume_snapshot_kill() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s1");
    let id = format!("t{}", std::process::id());
    let a = args(&dir, &id, SCRIPT);
    let pid = spawn_supervisor(&supervisor_bin(), &[], &a).unwrap();
    assert!(pid > 0);

    let c = SupervisorClient::connect_retry(&dir, &id, Duration::from_secs(10)).await.unwrap();
    let info = c.info().await.unwrap();
    assert_eq!(info.cols, 80);
    assert!(info.child_pid.is_some());
    c.subscribe(Some(0)).await.unwrap();
    read_until(&c, "hello-yonder", Duration::from_secs(10)).await;
    c.input(b"world\r").await.unwrap();
    let (_, end) = read_until(&c, "got:world", Duration::from_secs(10)).await;
    assert!(end > 0);

    // Drop the client (daemon restart); the child must keep running.
    drop(c);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(yonder_pty::supervisor_alive(&dir, &id).await);

    // Resume from offset 0 replays everything from the log.
    let c2 = SupervisorClient::connect(&dir, &id).await.unwrap();
    c2.subscribe(Some(0)).await.unwrap();
    read_until(&c2, "got:world", Duration::from_secs(10)).await;

    // Snapshot contains the screen text.
    let c3 = SupervisorClient::connect(&dir, &id).await.unwrap();
    c3.send(&yonder_pty::SupRequest::Snapshot).await.unwrap();
    let snap = loop {
        if let Some(SupEvent::Snapshot { data, cols, .. }) = c3.recv().await.unwrap() {
            assert_eq!(cols, 80);
            break String::from_utf8_lossy(&B64.decode(data).unwrap()).into_owned();
        }
    };
    assert!(snap.contains("hello-yonder"), "snapshot: {snap:?}");

    // Resize is acknowledged to subscribers.
    c3.resize(100, 30).await.unwrap();
    let got_resize = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(SupEvent::Resized { cols, rows }) = c2.recv().await.unwrap() {
                return (cols, rows);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(got_resize, (100, 30));
    assert_eq!(c3.info().await.unwrap().cols, 100);

    // Kill ends the child; exit.json is written.
    c3.kill().await.unwrap();
    let exited = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match c2.recv().await.unwrap() {
                Some(SupEvent::Exited { exit }) => return exit,
                Some(_) => {}
                None => panic!("closed before exit"),
            }
        }
    })
    .await
    .unwrap();
    assert!(exited.ended_at > 0);
    let from_disk = yonder_pty::read_exit(&dir).expect("exit.json");
    assert_eq!(from_disk, exited);

    // After linger the supervisor quits.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!yonder_pty::supervisor_alive(&dir, &id).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_code_and_late_attach() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s2");
    let id = format!("u{}", std::process::id());
    #[cfg(unix)]
    let argv: &[&str] = &["/bin/sh", "-c", "echo bye; exit 7"];
    #[cfg(windows)]
    let argv: &[&str] = &["cmd.exe", "/d", "/c", "echo bye& exit 7"];
    let mut a = args(&dir, &id, argv);
    a.linger_secs = 20;
    spawn_supervisor(&supervisor_bin(), &[], &a).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    // Attach after the child already exited: history + exit info are still served.
    let c = SupervisorClient::connect_retry(&dir, &id, Duration::from_secs(10)).await.unwrap();
    let info = c.info().await.unwrap();
    let exit = info.exited.expect("exited");
    assert_eq!(exit.code, Some(7));
    c.subscribe(Some(0)).await.unwrap();
    read_until(&c, "bye", Duration::from_secs(5)).await;
    c.send(&yonder_pty::SupRequest::Shutdown).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!yonder_pty::supervisor_alive(&dir, &id).await);
}
