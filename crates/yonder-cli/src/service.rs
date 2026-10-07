//! Install the daemon as a per-user service that starts at login.
//!
//! - macOS: `~/Library/LaunchAgents/dev.yonder.daemon.plist` (launchctl bootstrap).
//! - Linux: `~/.config/systemd/user/yonder.service` (+ `loginctl enable-linger` hint).
//! - Windows: a scheduled task "yonder" at logon (no admin needed), started now; falls back
//!   to an HKCU Run entry.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use anyhow::anyhow;
use anyhow::{bail, Context, Result};
use yonder_host::Paths;

#[cfg(target_os = "macos")]
const LABEL: &str = "dev.yonder.daemon";

fn exe() -> Result<PathBuf> {
    let p = std::env::current_exe().context("current exe")?;
    Ok(dunce_like(p))
}

fn dunce_like(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p)
        .map(|c| {
            let s = c.to_string_lossy().to_string();
            PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
        })
        .unwrap_or(p)
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("run {cmd:?}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        bail!("{cmd:?} failed: {}", text.trim());
    }
    Ok(text)
}

pub fn install(paths: &Paths) -> Result<()> {
    let exe = exe()?;
    install_for(paths, &exe)
}

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    Ok(dirs_home()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

#[cfg(unix)]
fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("no home directory"))
}

#[cfg(target_os = "macos")]
fn install_for(paths: &Paths, exe: &Path) -> Result<()> {
    let plist = plist_path()?;
    std::fs::create_dir_all(plist.parent().unwrap())?;
    let log = paths.daemon_log();
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let mut env = String::new();
    for key in ["YONDER_CONFIG_DIR", "YONDER_DATA_DIR"] {
        if let Ok(v) = std::env::var(key) {
            env.push_str(&format!("    <key>{key}</key><string>{}</string>\n", esc(&v)));
        }
    }
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>YONDER_LOG_STDERR</key><string>1</string>
{env}  </dict>
</dict>
</plist>
"#,
        exe = esc(&exe.to_string_lossy()),
        log = esc(&log.to_string_lossy()),
    );
    std::fs::write(&plist, body)?;
    let uid = run(Command::new("id").arg("-u"))?.trim().to_string();
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{LABEL}");
    let _ = Command::new("launchctl").args(["bootout", &target]).output();
    // bootout returns before the old instance is gone; bootstrap fails with EIO until then.
    let loaded = || Command::new("launchctl").args(["print", &target]).output().map(|o| o.status.success()).unwrap_or(false);
    for _ in 0..50 {
        if !loaded() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let mut last = None;
    for _ in 0..5 {
        match run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist)) {
            Ok(_) => {
                last = None;
                break;
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(600));
            }
        }
    }
    if let Some(e) = last {
        return Err(e);
    }
    let _ = Command::new("launchctl").args(["kickstart", "-k", &format!("{domain}/{LABEL}")]).output();
    println!("installed launchd agent {}", plist.display());
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall(_paths: &Paths) -> Result<()> {
    let plist = plist_path()?;
    let uid = run(Command::new("id").arg("-u"))?.trim().to_string();
    let _ = Command::new("launchctl").args(["bootout", &format!("gui/{uid}/{LABEL}")]).output();
    if plist.exists() {
        std::fs::remove_file(&plist)?;
    }
    println!("removed launchd agent");
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unit_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or(dirs_home()?.join(".config"));
    Ok(base.join("systemd/user/yonder.service"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn install_for(_paths: &Paths, exe: &Path) -> Result<()> {
    let unit = unit_path()?;
    std::fs::create_dir_all(unit.parent().unwrap())?;
    let mut env = String::new();
    for key in ["YONDER_CONFIG_DIR", "YONDER_DATA_DIR"] {
        if let Ok(v) = std::env::var(key) {
            env.push_str(&format!("Environment={key}={v}\n"));
        }
    }
    let body = format!(
        "[Unit]\nDescription=yonder host daemon\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nExecStart={exe} daemon\nRestart=always\nRestartSec=3\n\
         # Sessions run in detached supervisors and must survive daemon restarts.\nKillMode=process\n\
         Environment=YONDER_LOG_STDERR=1\n{env}\n[Install]\nWantedBy=default.target\n",
        exe = exe.display()
    );
    std::fs::write(&unit, body)?;
    run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    run(Command::new("systemctl").args(["--user", "enable", "yonder.service"]))?;
    run(Command::new("systemctl").args(["--user", "restart", "yonder.service"]))?;
    println!("installed systemd user unit {}", unit.display());
    let user = std::env::var("USER").unwrap_or_default();
    let linger = Command::new("loginctl").args(["show-user", &user, "--property=Linger"]).output();
    if let Ok(o) = linger {
        if !String::from_utf8_lossy(&o.stdout).contains("Linger=yes") {
            println!("note: run `sudo loginctl enable-linger {user}` so it also runs while you are logged out");
        }
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn uninstall(_paths: &Paths) -> Result<()> {
    let _ = Command::new("systemctl").args(["--user", "disable", "--now", "yonder.service"]).output();
    let unit = unit_path()?;
    if unit.exists() {
        std::fs::remove_file(&unit)?;
    }
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
    println!("removed systemd user unit");
    Ok(())
}

#[cfg(windows)]
/// Current user's SID from `whoami /user /fo csv /nh` (`"host\user","S-1-5-..."`).
fn current_sid() -> Result<String> {
    let out = run(Command::new("whoami").args(["/user", "/fo", "csv", "/nh"]))?;
    out.split(',')
        .map(|f| f.trim().trim_matches('"'))
        .find(|f| f.starts_with("S-1-"))
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no SID in whoami output: {}", out.trim()))
}

#[cfg(windows)]
/// Registers the logon task from XML with the user's SID: `/RU <domain\user>` fails for
/// local accounts whose USERDOMAIN is the workgroup name.
fn create_logon_task(dir: &Path, vbs: &Path) -> Result<()> {
    let sid = current_sid()?;
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>yonder host daemon</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author"><Exec><Command>wscript.exe</Command><Arguments>"{vbs}"</Arguments></Exec></Actions>
</Task>
"#,
        vbs = esc(&vbs.to_string_lossy())
    );
    // schtasks expects UTF-16 (with BOM) for an XML declared as UTF-16.
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let file = dir.join("yonder-task.xml");
    std::fs::write(&file, bytes)?;
    let r = run(Command::new("schtasks").args(["/Create", "/TN", "yonder", "/XML"]).arg(&file).arg("/F"));
    let _ = std::fs::remove_file(&file);
    r.map(|_| ())
}

#[cfg(windows)]
fn install_for(paths: &Paths, exe: &Path) -> Result<()> {
    // A logon task needs no admin rights. A tiny VBScript launcher keeps it windowless.
    let dir = paths.data_dir.clone();
    std::fs::create_dir_all(&dir)?;
    let vbs = dir.join("yonder-daemon.vbs");
    let mut env = String::new();
    for key in ["YONDER_CONFIG_DIR", "YONDER_DATA_DIR"] {
        if let Ok(v) = std::env::var(key) {
            env.push_str(&format!("sh.Environment(\"PROCESS\")(\"{key}\") = \"{}\"\r\n", v.replace('"', "\"\"")));
        }
    }
    let script = format!(
        "Set sh = CreateObject(\"WScript.Shell\")\r\n{env}sh.Run \"\"\"{}\"\" daemon\", 0, False\r\n",
        exe.display().to_string().replace('"', "\"\"")
    );
    std::fs::write(&vbs, script)?;
    let tr = format!("wscript.exe \"{}\"", vbs.display());
    let _ = Command::new("schtasks").args(["/Delete", "/TN", "yonder", "/F"]).output();
    // A daemon already started by an older Run entry would hold the control pipe.
    let _ = Command::new("reg")
        .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "yonder", "/f"])
        .output();
    match create_logon_task(&dir, &vbs) {
        Ok(_) => {
            let _ = run(Command::new("schtasks").args(["/Run", "/TN", "yonder"]));
            println!("installed scheduled task \"yonder\" (runs at logon)");
        }
        Err(e) => {
            // Fallback: per-user Run key.
            run(Command::new("reg").args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "yonder",
                "/t",
                "REG_SZ",
                "/d",
                &tr,
                "/f",
            ]))
            .with_context(|| format!("schtasks failed ({e:#}); registry fallback failed too"))?;
            let _ = Command::new("wscript.exe").arg(&vbs).spawn();
            println!("installed HKCU Run entry \"yonder\" (scheduled task failed: {e:#})");
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn uninstall(_paths: &Paths) -> Result<()> {
    let _ = Command::new("schtasks").args(["/Delete", "/TN", "yonder", "/F"]).output();
    let _ = Command::new("reg")
        .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "yonder", "/f"])
        .output();
    println!("removed scheduled task / Run entry");
    Ok(())
}
