//! Agent session history (history view, "resume" pickers), previews and model lists.
//!
//! - Codex: `thread/list` / `model/list` through a short-lived `codex app-server`; previews
//!   come from the thread's rollout file.
//! - Claude Code: `~/.claude/projects/<escaped cwd>/<session>.jsonl`.
//! - pi: `~/.pi/agent/sessions/--<escaped cwd>--/<ts>_<id>.jsonl` (or `$PI_CODING_AGENT_DIR`).
//!
//! Listing reads every session the agent knows about (all model providers), caches the result
//! briefly, and filters / pages it here, so search behaves the same on every agent version.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use yonder_proto::app::{AgentKind, AgentSessionSummary, ApprovalMode, ChatItem, ChatItemKind, HistoryFolder, ItemStatus};

use crate::proc::{build_command, spawn_jsonl, write_json};

/// Sessions scanned per agent (newest first).
const SCAN_MAX: usize = 2000;
/// Default page size of [`list_history`].
pub const PAGE_DEFAULT: usize = 50;
const PAGE_MAX: usize = 200;
/// A listing is reused for this long (the history view pages and searches on it).
const CACHE_TTL: Duration = Duration::from_secs(20);
/// Messages returned by [`agent_preview`].
const PREVIEW_ITEMS: usize = 12;
const PREVIEW_TEXT: usize = 4000;

/// One page of history.
#[derive(Debug, Clone, Default)]
pub struct HistoryPage {
    pub sessions: Vec<AgentSessionSummary>,
    pub next_cursor: Option<String>,
    pub folders: Vec<HistoryFolder>,
    pub errors: Vec<String>,
}

/// What to list.
#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    /// One agent, or Codex + Claude + pi.
    pub agent: Option<AgentKind>,
    /// Exact working directory.
    pub cwd: Option<String>,
    /// Case-insensitive substring of title, first message or folder (all words must match).
    pub query: Option<String>,
    /// Offset from a previous page.
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    /// Include temporary folders, empty sessions and non-interactive runs.
    pub all: bool,
}

/// Sessions of one agent, newest first (compatibility helper for resume lookups).
pub async fn list_agent_history(agent: AgentKind, cwd: Option<&str>) -> Result<Vec<AgentSessionSummary>> {
    let q = HistoryQuery { agent: Some(agent), cwd: cwd.filter(|c| !c.is_empty()).map(str::to_string), all: true, limit: Some(PAGE_MAX), ..Default::default() };
    let page = list_history(&q).await;
    match page.errors.first() {
        Some(e) if page.sessions.is_empty() => anyhow::bail!("{e}"),
        _ => Ok(page.sessions),
    }
}

/// One page of history for `q`, newest first. Agents that fail are reported in `errors`.
pub async fn list_history(q: &HistoryQuery) -> HistoryPage {
    let agents: Vec<AgentKind> = match q.agent {
        Some(a) => vec![a],
        None => vec![AgentKind::Codex, AgentKind::Claude, AgentKind::Pi],
    };
    let mut all = Vec::new();
    let mut errors = Vec::new();
    let lists = futures_join(agents.iter().map(|a| cached_scan(*a)).collect()).await;
    let lists: Vec<_> = lists.into_iter().map(|r| r.map(mark_active)).collect();
    for (a, r) in agents.iter().zip(lists) {
        match r {
            Ok(v) => all.extend(v),
            // A missing agent is not an error for the merged view.
            Err(e) if q.agent.is_some() || !is_not_installed(&e) => errors.push(format!("{}: {e:#}", a.as_str())),
            Err(_) => {}
        }
    }
    filter_page(all, q, errors)
}

/// A session whose file was written this recently is probably mid-turn somewhere.
const ACTIVE_WINDOW_MS: u64 = 60_000;

/// Fresh modification times (listings are cached) and the `active` flag.
fn mark_active(mut v: Vec<Scanned>) -> Vec<Scanned> {
    let now = now_ms();
    for s in &mut v {
        if let Some(m) = s.path.as_deref().and_then(mtime_ms) {
            s.summary.updated_at = Some(s.summary.updated_at.unwrap_or(0).max(m));
        }
        s.summary.active = s.summary.updated_at.is_some_and(|u| now.saturating_sub(u) < ACTIVE_WINDOW_MS);
    }
    v
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Await all futures concurrently (no extra dependency).
async fn futures_join<T: Send + 'static>(futs: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>>) -> Vec<T> {
    let handles: Vec<_> = futs.into_iter().map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.expect("history task panicked"));
    }
    out
}

fn is_not_installed(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}").to_lowercase();
    m.contains("spawn agent") || m.contains("not found") || m.contains("no such file")
}

/// Filter, sort and page a full listing (pure; unit-tested).
pub fn filter_page(mut all: Vec<Scanned>, q: &HistoryQuery, errors: Vec<String>) -> HistoryPage {
    all.sort_by(|a, b| b.summary.updated_at.unwrap_or(0).cmp(&a.summary.updated_at.unwrap_or(0)));
    if !q.all {
        all.retain(|s| !s.hidden);
    }
    // Folder facets: over everything this agent filter shows, before cwd / query narrow it.
    let mut counts: HashMap<String, (u32, u64)> = HashMap::new();
    for s in &all {
        if let Some(c) = &s.summary.cwd {
            let e = counts.entry(c.clone()).or_insert((0, 0));
            e.0 += 1;
            e.1 = e.1.max(s.summary.updated_at.unwrap_or(0));
        }
    }
    if let Some(c) = q.cwd.as_deref().filter(|c| !c.is_empty()) {
        let want = norm_path(c);
        all.retain(|s| s.summary.cwd.as_deref().map(norm_path).as_deref() == Some(want.as_str()));
    }
    let words: Vec<String> = q.query.as_deref().unwrap_or("").split_whitespace().map(|w| w.to_lowercase()).collect();
    if !words.is_empty() {
        all.retain(|s| {
            let hay = format!(
                "{}\n{}\n{}\n{}",
                s.summary.title,
                s.summary.preview.as_deref().unwrap_or(""),
                s.summary.cwd.as_deref().unwrap_or(""),
                s.summary.id
            )
            .to_lowercase();
            words.iter().all(|w| hay.contains(w))
        });
    }
    let offset: usize = q.cursor.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
    let limit = q.limit.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX);
    let total = all.len();
    let sessions: Vec<AgentSessionSummary> = all.into_iter().skip(offset).take(limit).map(|s| s.summary).collect();
    let next = offset + sessions.len();
    let mut folders: Vec<HistoryFolder> = Vec::new();
    if offset == 0 {
        let mut f: Vec<(String, (u32, u64))> = counts.into_iter().collect();
        f.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
        folders = f.into_iter().take(60).map(|(path, (count, _))| HistoryFolder { path, count }).collect();
    }
    HistoryPage { sessions, next_cursor: (next < total).then(|| next.to_string()), folders, errors }
}

fn norm_path(p: &str) -> String {
    let t = p.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        t.replace('/', "\\").to_lowercase()
    } else {
        t.to_string()
    }
}

/// A listed session plus whether the default view hides it.
#[derive(Debug, Clone)]
pub struct Scanned {
    pub summary: AgentSessionSummary,
    /// Temporary folder, test run or non-interactive (`codex exec`).
    pub hidden: bool,
    /// File holding the session (previews).
    pub path: Option<PathBuf>,
}

type ScanResult = Result<Vec<Scanned>, std::sync::Arc<anyhow::Error>>;

struct Cache {
    at: Instant,
    data: Vec<Scanned>,
}

fn cache() -> &'static Mutex<HashMap<AgentKind, Cache>> {
    static C: std::sync::OnceLock<Mutex<HashMap<AgentKind, Cache>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_scan(agent: AgentKind) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Scanned>>> + Send>> {
    Box::pin(async move {
        if let Some(c) = cache().lock().unwrap().get(&agent) {
            if c.at.elapsed() < CACHE_TTL {
                return Ok(c.data.clone());
            }
        }
        let r: ScanResult = match agent {
            AgentKind::Codex => codex_scan().await.map_err(std::sync::Arc::new),
            AgentKind::Claude => Ok(tokio::task::spawn_blocking(|| claude_scan(&claude_dir())).await.unwrap_or_default()),
            AgentKind::Pi => Ok(tokio::task::spawn_blocking(|| pi_scan(&pi_sessions_dir())).await.unwrap_or_default()),
            _ => Ok(Vec::new()),
        };
        match r {
            Ok(v) => {
                cache().lock().unwrap().insert(agent, Cache { at: Instant::now(), data: v.clone() });
                Ok(v)
            }
            Err(e) => Err(anyhow::anyhow!("{e:#}")),
        }
    })
}

/// Forget cached listings (after a session was created, resumed or renamed).
pub fn invalidate_history() {
    cache().lock().unwrap().clear();
}

/// Folders where test runs and throwaway sessions live.
pub fn is_temp_dir(cwd: &str) -> bool {
    let c = cwd.replace('\\', "/");
    let lc = c.to_lowercase();
    let tmp = std::env::temp_dir().to_string_lossy().replace('\\', "/").to_lowercase();
    let tmp = tmp.trim_end_matches('/');
    (!tmp.is_empty() && lc.starts_with(tmp))
        || lc.starts_with("/tmp/")
        || lc == "/tmp"
        || lc.starts_with("/private/tmp/")
        || lc.starts_with("/var/folders/")
        || lc.starts_with("/private/var/folders/")
        || lc.contains("/appdata/local/temp/")
        || lc.contains("/yonder-e2e-")
        || lc.contains("/yonder-it")
}

/// Several requests in one throwaway `codex app-server` run; one slot per request.
/// The first `required` requests are waited for; the rest are optional and get a short grace
/// period after those arrive (older app-servers may never answer a newer method), then `None`.
async fn codex_requests(reqs: Vec<(&str, Value)>, required: usize) -> Result<Vec<Option<Result<Value>>>> {
    let mut s = CodexSession::start().await?;
    let r = s.batch(reqs, required).await;
    s.close().await;
    r
}

/// A short-lived `codex app-server` for several sequential requests.
struct CodexSession {
    p: crate::proc::AgentProc,
    next: u64,
}

impl CodexSession {
    async fn start() -> Result<Self> {
        let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
        let cmd = build_command(&["codex".into(), "app-server".into()], &home, &Default::default(), false)?;
        let mut p = spawn_jsonl(cmd)?;
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"yonder","title":"yonder","version":env!("CARGO_PKG_VERSION")},"capabilities":null}});
        write_json(&mut p.stdin, &init).await?;
        let fut = async {
            loop {
                let v = p.lines.recv().await.context("codex app-server exited")?;
                if v.get("method").is_none() && v.get("id").and_then(|i| i.as_u64()) == Some(1) {
                    if let Some(e) = v.get("error") {
                        anyhow::bail!("initialize: {}", e.get("message").and_then(|m| m.as_str()).unwrap_or("error"));
                    }
                    return Ok(());
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), fut).await.context("codex app-server timed out")??;
        write_json(&mut p.stdin, &json!({"jsonrpc":"2.0","method":"initialized"})).await?;
        Ok(Self { p, next: 2 })
    }

    async fn batch(&mut self, reqs: Vec<(&str, Value)>, required: usize) -> Result<Vec<Option<Result<Value>>>> {
        let n = reqs.len();
        let required = required.min(n);
        let base = self.next;
        self.next += n as u64;
        for (i, (method, params)) in reqs.iter().enumerate() {
            write_json(&mut self.p.stdin, &json!({"jsonrpc":"2.0","id":base + i as u64,"method":method,"params":params})).await?;
        }
        let lines = &mut self.p.lines;
        let fut = async {
            let mut results: Vec<Option<Result<Value>>> = (0..n).map(|_| None).collect();
            if n == 0 {
                return Ok(results);
            }
            let mut grace: Option<tokio::time::Instant> = None;
            loop {
                let v = match grace {
                    Some(deadline) => match tokio::time::timeout_at(deadline, lines.recv()).await {
                        Ok(v) => v,
                        Err(_) => return Ok(results),
                    },
                    None => lines.recv().await,
                };
                let Some(v) = v else {
                    if grace.is_some() {
                        return Ok(results);
                    }
                    anyhow::bail!("codex app-server exited");
                };
                if v.get("method").is_some() {
                    continue;
                }
                let Some(id) = v.get("id").and_then(|i| i.as_u64()) else { continue };
                if id < base || id >= base + n as u64 {
                    continue;
                }
                let i = (id - base) as usize;
                results[i] = Some(match v.get("error") {
                    Some(e) => Err(anyhow::anyhow!("{}: {}", reqs[i].0, e.get("message").and_then(|m| m.as_str()).unwrap_or("error"))),
                    None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                });
                if results.iter().all(Option::is_some) {
                    return Ok(results);
                }
                if grace.is_none() && results[..required].iter().all(Option::is_some) {
                    grace = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), fut).await.context("codex app-server timed out")?
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.batch(vec![(method, params)], 1).await?.pop().flatten().context("no response")?
    }

    async fn close(mut self) {
        let _ = self.p.child.start_kill();
        let _ = self.p.child.wait().await;
    }
}

/// Every Codex thread the user started (all model providers, cli / IDE / desktop / exec),
/// newest first. Sub-agent threads are not listed.
async fn codex_scan() -> Result<Vec<Scanned>> {
    let mut s = CodexSession::start().await?;
    let r = codex_scan_with(&mut s).await;
    s.close().await;
    r
}

async fn codex_scan_with(s: &mut CodexSession) -> Result<Vec<Scanned>> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    // `modelProviders: []` lists every provider (omitted, Codex shows only the configured one);
    // the explicit source kinds are needed on 0.142, where other combinations return nothing.
    let base = json!({"limit": 100, "sortKey": "updated_at", "modelProviders": [], "sourceKinds": ["cli", "vscode", "exec", "appServer"], "useStateDbOnly": true});
    let mut fallback = false;
    loop {
        let mut params = base.clone();
        if fallback {
            params.as_object_mut().unwrap().remove("useStateDbOnly");
        }
        if let Some(c) = &cursor {
            params["cursor"] = json!(c);
        }
        let res = match s.request("thread/list", params).await {
            Ok(r) => r,
            // Older app-servers reject unknown fields.
            Err(e) if !fallback && cursor.is_none() && format!("{e:#}").contains("unknown field") => {
                fallback = true;
                continue;
            }
            Err(e) => return Err(e),
        };
        for t in res.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
            if let Some(sc) = codex_thread(t) {
                out.push(sc);
            }
        }
        cursor = res.get("nextCursor").and_then(|c| c.as_str()).map(str::to_string);
        if cursor.is_none() || out.len() >= SCAN_MAX {
            break;
        }
    }
    Ok(out)
}

/// `source` of a Codex thread: a string (`vscode`) or an object (`{"subagent": …}`).
fn codex_source(t: &Value) -> String {
    match t.get("source") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o.keys().next().cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

fn codex_thread(t: &Value) -> Option<Scanned> {
    if t.get("parentThreadId").is_some_and(|p| !p.is_null()) || t.get("ephemeral").and_then(|e| e.as_bool()).unwrap_or(false) {
        return None;
    }
    let id = t.get("id")?.as_str()?.to_string();
    let src = codex_source(t);
    if src.to_lowercase().starts_with("sub") {
        return None;
    }
    let originator = t.get("originator").and_then(|o| o.as_str()).unwrap_or("");
    let name = t.get("name").and_then(|n| n.as_str()).map(|n| one_line(n, 120)).filter(|n| !n.is_empty());
    let first = one_line(t.get("preview").and_then(|n| n.as_str()).unwrap_or(""), 160);
    let title = name.clone().unwrap_or_else(|| one_line(&first, 120));
    let cwd = t.get("cwd").and_then(|c| c.as_str()).map(str::to_string);
    let source = match (src.as_str(), originator) {
        (_, "yonder") => "yonder",
        (_, o) if o.contains("Desktop") => "desktop",
        ("cli", _) => "cli",
        ("exec", _) => "exec",
        ("vscode", "") => "ide",
        ("vscode", o) if o.contains("tui") => "cli",
        ("appServer", _) => "sdk",
        _ => "ide",
    };
    let hidden = src == "exec" || first.is_empty() || cwd.as_deref().is_some_and(is_temp_dir) || is_test_prompt(&first);
    Some(Scanned {
        summary: AgentSessionSummary {
            id,
            agent: AgentKind::Codex,
            title: if title.is_empty() { "(无标题)".into() } else { title.clone() },
            cwd,
            updated_at: t.get("updatedAt").and_then(|u| u.as_u64()).map(|s| s * 1000),
            preview: (name.is_some() && !first.is_empty() && first != title).then_some(first),
            source: Some(source.into()),
            model: t.get("model").and_then(|m| m.as_str()).map(str::to_string),
            active: false,
        },
        hidden,
        path: t.get("path").and_then(|p| p.as_str()).map(PathBuf::from),
    })
}

/// Automated acceptance prompts that pollute the history of test hosts.
fn is_test_prompt(first: &str) -> bool {
    first.starts_with("Run exactly this shell command and nothing else")
}

/// What `codex app-server` reports about models and the user's approval settings.
#[derive(Debug, Clone, Default)]
pub struct CodexInfo {
    pub models: Vec<String>,
    /// The model set in the user's Codex config when there is one (what Codex runs when none is
    /// given), else the catalog default (which can belong to another provider).
    pub default_model: Option<String>,
    /// The yonder mode matching the configured `approval_policy` / `sandbox_mode`.
    pub approval: Option<ApprovalMode>,
}

/// Yonder mode for Codex config values (`None` when nothing is configured).
pub fn codex_config_mode(approval_policy: Option<&str>, sandbox_mode: Option<&str>) -> Option<ApprovalMode> {
    match (approval_policy, sandbox_mode) {
        (Some("never"), Some("danger-full-access")) => Some(ApprovalMode::Yolo),
        (Some("never"), _) => Some(ApprovalMode::Auto),
        (Some(_), _) => Some(ApprovalMode::Ask),
        (None, Some("danger-full-access")) => Some(ApprovalMode::Yolo),
        (None, _) => None,
    }
}

/// Models offered by `codex app-server` (`model/list`) plus the defaults from `config/read`.
pub async fn codex_models() -> Result<CodexInfo> {
    let mut res = codex_requests(vec![("model/list", json!({"limit": 100})), ("config/read", json!({}))], 1).await?;
    let config = res.pop().flatten().and_then(|r| r.ok());
    let res = res.pop().flatten().context("no model/list response")??;
    let mut models = Vec::new();
    let mut default = None;
    for m in res.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
        if m.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false) {
            continue;
        }
        let Some(id) = m.get("model").or_else(|| m.get("id")).and_then(|i| i.as_str()) else { continue };
        if m.get("isDefault").and_then(|d| d.as_bool()).unwrap_or(false) {
            default = Some(id.to_string());
        }
        models.push(id.to_string());
    }
    let cfg = config.as_ref().and_then(|c| c.get("config"));
    let text = |k: &str| cfg.and_then(|c| c.get(k)).and_then(|m| m.as_str()).filter(|m| !m.is_empty());
    let configured = text("model");
    if let Some(m) = configured {
        if !models.iter().any(|x| x == m) {
            models.insert(0, m.to_string());
        }
        default = Some(m.to_string());
    }
    Ok(CodexInfo { models, default_model: default, approval: codex_config_mode(text("approval_policy"), text("sandbox_mode")) })
}

/// Yonder mode for Claude's `permissions.defaultMode` in the user's settings.
pub fn claude_default_mode() -> Option<ApprovalMode> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
    let v: Value = serde_json::from_slice(&std::fs::read(dir.join("settings.json")).ok()?).ok()?;
    match v.get("permissions")?.get("defaultMode")?.as_str()? {
        "bypassPermissions" => Some(ApprovalMode::Yolo),
        "acceptEdits" | "auto" | "dontAsk" => Some(ApprovalMode::Auto),
        _ => Some(ApprovalMode::Ask),
    }
}

/// `pi --list-models` table -> `provider/model` ids.
pub async fn pi_models() -> Result<Vec<String>> {
    let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    let mut cmd = build_command(&["pi".into(), "--list-models".into()], &home, &Default::default(), false)?;
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output()).await.context("pi --list-models timed out")??;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let provider = it.next()?;
            let model = it.next()?;
            Some(format!("{provider}/{model}"))
        })
        .collect())
}

fn one_line(s: &str, max: usize) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s
    }
}

fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
        .join("projects")
}

fn mtime_ms(p: &Path) -> Option<u64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    m.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_millis() as u64)
}

/// Newest `.jsonl` files directly in the given dirs.
fn newest_jsonl(dirs: &[PathBuf], limit: usize) -> Vec<(PathBuf, u64)> {
    let mut files: Vec<(PathBuf, u64)> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "jsonl").unwrap_or(false))
        .filter_map(|p| mtime_ms(&p).map(|t| (p, t)))
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.truncate(limit);
    files
}

fn subdirs(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default()
}

/// First `max` lines of a file (bounded read).
fn head_lines(p: &Path, max: usize) -> Vec<String> {
    let Ok(f) = std::fs::File::open(p) else { return Vec::new() };
    BufReader::new(f.take(4 * 1024 * 1024)).lines().map_while(|l| l.ok()).take(max).collect()
}

/// Last `bytes` of a file, split into lines (first partial line dropped). Also says whether
/// the file had more before that.
fn tail_lines(p: &Path, bytes: u64) -> (Vec<String>, bool) {
    let Ok(mut f) = std::fs::File::open(p) else { return (Vec::new(), false) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), false);
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let s = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = s.lines().map(str::to_string).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    (lines, start > 0)
}

/// Text parts of a message `content` (string or `[{type: text|input_text|output_text, text}]`).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| matches!(p.get("type").and_then(|t| t.as_str()), Some("text" | "input_text" | "output_text" | "Text")))
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What the user typed, or `None` for injected context (instructions, environment, commands).
fn user_text(msg: &Value) -> Option<String> {
    let text = content_text(msg.get("content")?);
    let t = text.trim();
    if t.is_empty() || t.starts_with('<') || t.starts_with("Caveat:") || t.starts_with("# AGENTS.md instructions") {
        return None;
    }
    Some(t.to_string())
}

/// Claude Code sessions, newest first. Sessions started by the SDK (yonder's own chats, other
/// tools) count as interactive; empty ones and temp folders are hidden.
pub fn claude_scan(root: &Path) -> Vec<Scanned> {
    let mut out = Vec::new();
    for (path, mtime) in newest_jsonl(&subdirs(root), SCAN_MAX) {
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
        let mut title: Option<String> = None;
        let mut first_user: Option<String> = None;
        let mut session_cwd: Option<String> = None;
        let mut entry: Option<String> = None;
        let mut model: Option<String> = None;
        let mut has_messages = false;
        for l in head_lines(&path, 200) {
            let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("user") => {
                    has_messages = true;
                    if session_cwd.is_none() {
                        session_cwd = v.get("cwd").and_then(|c| c.as_str()).map(str::to_string);
                    }
                    if entry.is_none() {
                        entry = v.get("entrypoint").and_then(|c| c.as_str()).map(str::to_string);
                    }
                    if first_user.is_none() && !v.get("isMeta").and_then(|m| m.as_bool()).unwrap_or(false) {
                        first_user = v.get("message").and_then(user_text);
                    }
                }
                Some("assistant") => {
                    has_messages = true;
                    if model.is_none() {
                        model = v.get("message").and_then(|m| m.get("model")).and_then(|m| m.as_str()).filter(|m| !m.starts_with('<')).map(str::to_string);
                    }
                }
                Some("summary") => {
                    if title.is_none() {
                        title = v.get("summary").and_then(|s| s.as_str()).map(str::to_string);
                    }
                }
                _ => {}
            }
        }
        let (tail, _) = tail_lines(&path, 64 * 1024);
        for l in tail.iter().rev() {
            let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("custom-title") => {
                    if let Some(t) = v.get("customTitle").and_then(|t| t.as_str()) {
                        title = Some(t.to_string());
                        break;
                    }
                }
                Some("ai-title") if title.is_none() => {
                    title = v.get("aiTitle").and_then(|t| t.as_str()).map(str::to_string);
                }
                _ => {}
            }
        }
        if !has_messages {
            continue;
        }
        let first = first_user.as_deref().map(|f| one_line(f, 160));
        let title = one_line(title.as_deref().or(first.as_deref()).unwrap_or("(无标题)"), 120);
        let hidden = first.is_none() || session_cwd.as_deref().is_some_and(is_temp_dir) || first.as_deref().is_some_and(is_test_prompt);
        let source = match entry.as_deref() {
            Some("cli") => "cli",
            Some(e) if e.starts_with("sdk") => "sdk",
            Some(e) if e.contains("vscode") || e.contains("ide") => "ide",
            Some(e) if e.contains("desktop") => "desktop",
            _ => "cli",
        };
        out.push(Scanned {
            summary: AgentSessionSummary {
                id,
                agent: AgentKind::Claude,
                preview: first.filter(|f| *f != title),
                title,
                cwd: session_cwd,
                updated_at: Some(mtime),
                source: Some(source.into()),
                model,
                active: false,
            },
            hidden,
            path: Some(path),
        });
    }
    out
}

fn pi_sessions_dir() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".pi").join("agent"))
        .join("sessions")
}

/// pi sessions, newest first. Only top-level session files count: sub-agent runs live in
/// nested folders.
pub fn pi_scan(root: &Path) -> Vec<Scanned> {
    let mut out = Vec::new();
    for (path, mtime) in newest_jsonl(&subdirs(root), SCAN_MAX) {
        let mut id: Option<String> = None;
        let mut session_cwd: Option<String> = None;
        let mut name: Option<String> = None;
        let mut first_user: Option<String> = None;
        let mut model: Option<String> = None;
        for l in head_lines(&path, 60) {
            let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("session") => {
                    id = v.get("id").and_then(|i| i.as_str()).map(str::to_string);
                    session_cwd = v.get("cwd").and_then(|c| c.as_str()).map(str::to_string);
                }
                Some("model_change") if model.is_none() => {
                    let p = v.get("provider").and_then(|x| x.as_str());
                    let m = v.get("modelId").and_then(|x| x.as_str());
                    model = m.map(|m| p.map(|p| format!("{p}/{m}")).unwrap_or_else(|| m.to_string()));
                }
                Some("session_info") => {
                    if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
                        name = Some(n.to_string());
                    }
                }
                Some("message") if first_user.is_none() => {
                    let m = v.get("message").cloned().unwrap_or(Value::Null);
                    if m.get("role").and_then(|r| r.as_str()) == Some("user") {
                        first_user = user_text(&m);
                    }
                }
                _ => {}
            }
        }
        let (tail, _) = tail_lines(&path, 32 * 1024);
        for l in tail.iter().rev() {
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                if v.get("type").and_then(|t| t.as_str()) == Some("session_info") {
                    if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
                        name = Some(n.to_string());
                    }
                    break;
                }
            }
        }
        let Some(id) = id else { continue };
        if first_user.is_none() && name.is_none() {
            continue; // never used
        }
        let first = first_user.as_deref().map(|f| one_line(f, 160));
        let title = one_line(name.as_deref().or(first.as_deref()).unwrap_or("(无标题)"), 120);
        let hidden = session_cwd.as_deref().is_some_and(is_temp_dir) || first.as_deref().is_some_and(is_test_prompt);
        out.push(Scanned {
            summary: AgentSessionSummary {
                id,
                agent: AgentKind::Pi,
                preview: first.filter(|f| *f != title),
                title,
                cwd: session_cwd,
                updated_at: Some(mtime),
                source: Some("cli".into()),
                model,
                active: false,
            },
            hidden,
            path: Some(path),
        });
    }
    out
}

// ------------------------------------------------------------------ previews

/// The last messages of an agent session, read from its own files (the agent is not started).
pub async fn agent_preview(agent: AgentKind, id: &str) -> Result<(Vec<ChatItem>, bool)> {
    let path = session_path(agent, id).await?;
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        // Long agent turns are mostly tool calls: read further back until enough messages.
        let mut window: u64 = 1024 * 1024;
        loop {
            let (lines, more) = tail_lines(&path, window);
            let msgs = match agent {
                AgentKind::Codex => codex_messages(&lines),
                AgentKind::Claude => claude_messages(&lines),
                AgentKind::Pi => pi_messages(&lines),
                _ => Vec::new(),
            };
            if !more || msgs.len() >= PREVIEW_ITEMS || window >= 32 * 1024 * 1024 {
                return Ok(preview_items(&id, msgs, more));
            }
            window *= 4;
        }
    })
    .await?
}

/// File of a listed session (listing first when it is not cached).
async fn session_path(agent: AgentKind, id: &str) -> Result<PathBuf> {
    let find = |v: &[Scanned]| v.iter().find(|s| s.summary.id == id).and_then(|s| s.path.clone());
    let cached = cache().lock().unwrap().get(&agent).and_then(|c| find(&c.data));
    if let Some(p) = cached {
        return Ok(p);
    }
    let v = cached_scan(agent).await?;
    find(&v).with_context(|| format!("{} session {id} not found", agent.as_str()))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    User(String),
    Agent(String),
}

/// Keep user messages and the last agent message before each next user message, then the
/// newest [`PREVIEW_ITEMS`].
fn preview_items(id: &str, msgs: Vec<Msg>, more_before: bool) -> (Vec<ChatItem>, bool) {
    let mut kept: Vec<Msg> = Vec::new();
    for m in msgs {
        match (&m, kept.last()) {
            (Msg::Agent(_), Some(Msg::Agent(_))) => {
                kept.pop();
                kept.push(m);
            }
            _ => kept.push(m),
        }
    }
    let truncated = more_before || kept.len() > PREVIEW_ITEMS;
    let start = kept.len().saturating_sub(PREVIEW_ITEMS);
    let items = kept[start..]
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (kind, text) = match m {
                Msg::User(t) => (ChatItemKind::User, t),
                Msg::Agent(t) => (ChatItemKind::Agent, t),
            };
            let mut it = ChatItem::new(format!("{id}-p{i}"), kind, ItemStatus::Completed, 0);
            it.text = Some(clip(text, PREVIEW_TEXT));
            it
        })
        .collect();
    (items, truncated)
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Codex rollout lines: `response_item` messages (user / assistant), ignoring injected context.
pub fn codex_messages(lines: &[String]) -> Vec<Msg> {
    let mut out = Vec::new();
    for l in lines {
        // Cheap check before parsing: most lines are tool calls and token counts.
        if !l.contains("\"message\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let Some(p) = v.get("payload") else { continue };
        if p.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        match p.get("role").and_then(|r| r.as_str()) {
            Some("user") => {
                if let Some(t) = user_text(p) {
                    out.push(Msg::User(t));
                }
            }
            Some("assistant") => {
                let t = content_text(p.get("content").unwrap_or(&Value::Null));
                if !t.trim().is_empty() {
                    out.push(Msg::Agent(t.trim().to_string()));
                }
            }
            _ => {}
        }
    }
    out
}

/// Claude Code session lines (`user` / `assistant` entries; tool results are not messages).
pub fn claude_messages(lines: &[String]) -> Vec<Msg> {
    let mut out = Vec::new();
    for l in lines {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        if v.get("isSidechain").and_then(|x| x.as_bool()).unwrap_or(false) || v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        let Some(m) = v.get("message") else { continue };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                if let Some(t) = user_text(m) {
                    out.push(Msg::User(t));
                }
            }
            Some("assistant") => {
                let t = content_text(m.get("content").unwrap_or(&Value::Null));
                if !t.trim().is_empty() {
                    out.push(Msg::Agent(t.trim().to_string()));
                }
            }
            _ => {}
        }
    }
    out
}

/// pi session lines (`message` entries with role user / assistant).
pub fn pi_messages(lines: &[String]) -> Vec<Msg> {
    let mut out = Vec::new();
    for l in lines {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let Some(m) = v.get("message") else { continue };
        match m.get("role").and_then(|r| r.as_str()) {
            Some("user") => {
                if let Some(t) = user_text(m) {
                    out.push(Msg::User(t));
                }
            }
            Some("assistant") => {
                let t = content_text(m.get("content").unwrap_or(&Value::Null));
                if !t.trim().is_empty() {
                    out.push(Msg::Agent(t.trim().to_string()));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(id: &str, title: &str, cwd: &str, t: u64, hidden: bool) -> Scanned {
        Scanned {
            summary: AgentSessionSummary {
                id: id.into(),
                agent: AgentKind::Codex,
                title: title.into(),
                cwd: Some(cwd.into()),
                updated_at: Some(t),
                preview: None,
                source: None,
                model: None,
                active: false,
            },
            hidden,
            path: None,
        }
    }

    #[test]
    fn filter_sort_page_and_folders() {
        let all = vec![
            sc("a", "修复中继重连", "/w/relay", 30, false),
            sc("b", "Run exactly this", "/tmp/x", 50, true),
            sc("c", "KTV 播放器", "/w/ktv", 40, false),
            sc("d", "relay docs", "/w/relay", 10, false),
        ];
        let q = HistoryQuery::default();
        let p = filter_page(all.clone(), &q, vec![]);
        assert_eq!(p.sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["c", "a", "d"]);
        assert_eq!(p.folders, vec![HistoryFolder { path: "/w/ktv".into(), count: 1 }, HistoryFolder { path: "/w/relay".into(), count: 2 }]);
        assert!(p.next_cursor.is_none());

        let p = filter_page(all.clone(), &HistoryQuery { all: true, ..Default::default() }, vec![]);
        assert_eq!(p.sessions[0].id, "b");

        // Search is case-insensitive, over title and folder, all words.
        let p = filter_page(all.clone(), &HistoryQuery { query: Some("RELAY".into()), ..Default::default() }, vec![]);
        assert_eq!(p.sessions.len(), 2);
        let p = filter_page(all.clone(), &HistoryQuery { query: Some("relay 修复".into()), ..Default::default() }, vec![]);
        assert_eq!(p.sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["a"]);

        let p = filter_page(all.clone(), &HistoryQuery { cwd: Some("/w/relay/".into()), ..Default::default() }, vec![]);
        assert_eq!(p.sessions.len(), 2);

        // Paging.
        let p1 = filter_page(all.clone(), &HistoryQuery { limit: Some(2), ..Default::default() }, vec![]);
        assert_eq!(p1.sessions.len(), 2);
        assert_eq!(p1.next_cursor.as_deref(), Some("2"));
        let p2 = filter_page(all, &HistoryQuery { limit: Some(2), cursor: p1.next_cursor, ..Default::default() }, vec![]);
        assert_eq!(p2.sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["d"]);
        assert!(p2.next_cursor.is_none());
        assert!(p2.folders.is_empty());
    }

    #[test]
    fn temp_dirs() {
        assert!(is_temp_dir("/private/var/folders/7r/x/T/.tmpRYX"));
        assert!(is_temp_dir("/tmp/yonder-e2e-abc/work"));
        assert!(is_temp_dir(r"C:\Users\a\AppData\Local\Temp\x"));
        assert!(is_temp_dir("/Users/a/yonder-it"));
        assert!(!is_temp_dir("/Users/a/run/tmp"));
        assert!(!is_temp_dir("/Users/a/run/yonder"));
    }

    #[test]
    fn codex_thread_mapping() {
        let t = json!({"id": "t1", "name": "同步配置", "preview": "把 codex 配置同步过去", "cwd": "/w", "updatedAt": 5, "source": "vscode", "originator": "Codex Desktop", "modelProvider": "fox", "path": "/x.jsonl"});
        let s = codex_thread(&t).unwrap();
        assert_eq!(s.summary.title, "同步配置");
        assert_eq!(s.summary.preview.as_deref(), Some("把 codex 配置同步过去"));
        assert_eq!(s.summary.source.as_deref(), Some("desktop"));
        assert_eq!(s.summary.updated_at, Some(5000));
        assert!(!s.hidden);
        assert!(codex_thread(&json!({"id": "s", "preview": "x", "source": {"subagent": {}}})).is_none());
        assert!(codex_thread(&json!({"id": "s", "preview": "x", "parentThreadId": "p"})).is_none());
        assert!(codex_thread(&json!({"id": "e", "preview": "x", "source": "exec", "cwd": "/w"})).unwrap().hidden);
        assert!(codex_thread(&json!({"id": "y", "preview": "hi", "source": "vscode", "originator": "yonder", "cwd": "/w"})).unwrap().summary.source.as_deref() == Some("yonder"));
    }

    #[test]
    fn previews_keep_user_and_final_agent_messages() {
        let lines: Vec<String> = [
            json!({"type": "response_item", "payload": {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<permissions>"}]}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>x</environment_context>"}]}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "修一下重连"}]}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "先看代码"}]}}),
            json!({"type": "response_item", "payload": {"type": "function_call", "name": "exec"}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "修好了"}]}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect();
        let (items, truncated) = preview_items("t", codex_messages(&lines), false);
        assert!(!truncated);
        assert_eq!(items.iter().map(|i| (i.kind, i.text.clone().unwrap())).collect::<Vec<_>>(), vec![(ChatItemKind::User, "修一下重连".into()), (ChatItemKind::Agent, "修好了".into())]);

        let claude: Vec<String> = [
            json!({"type": "user", "message": {"role": "user", "content": "hello"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use"}]}}),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "x"}]}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "hi there"}]}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect();
        assert_eq!(claude_messages(&claude), vec![Msg::User("hello".into()), Msg::Agent("hi there".into())]);

        let pi: Vec<String> = [
            json!({"type": "session", "id": "s"}),
            json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "q"}]}}),
            json!({"type": "message", "message": {"role": "assistant", "content": [{"type": "thinking"}, {"type": "text", "text": "a"}]}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect();
        assert_eq!(pi_messages(&pi), vec![Msg::User("q".into()), Msg::Agent("a".into())]);

        let many: Vec<Msg> = (0..30).map(|i| if i % 2 == 0 { Msg::User(format!("u{i}")) } else { Msg::Agent(format!("a{i}")) }).collect();
        let (items, truncated) = preview_items("t", many, false);
        assert!(truncated);
        assert_eq!(items.len(), PREVIEW_ITEMS);
        assert_eq!(items.last().unwrap().text.as_deref(), Some("a29"));
    }

    #[test]
    fn claude_and_pi_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cdir = tmp.path().join("claude");
        let proj = cdir.join("-w-p");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("abc.jsonl"),
            concat!(
                "{\"type\":\"queue-operation\"}\n",
                "{\"type\":\"user\",\"cwd\":\"/w/p\",\"entrypoint\":\"cli\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"fix the bug\"}]}}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"model\":\"opus\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n",
                "{\"type\":\"ai-title\",\"aiTitle\":\"Fix bug\",\"sessionId\":\"abc\"}\n"
            ),
        )
        .unwrap();
        std::fs::write(proj.join("empty.jsonl"), "{\"type\":\"queue-operation\"}\n").unwrap();
        let h = claude_scan(&cdir);
        assert_eq!(h.len(), 1);
        let s = &h[0].summary;
        assert_eq!((s.id.as_str(), s.title.as_str(), s.cwd.as_deref(), s.preview.as_deref()), ("abc", "Fix bug", Some("/w/p"), Some("fix the bug")));
        assert_eq!(s.model.as_deref(), Some("opus"));
        assert!(!h[0].hidden);

        let pdir = tmp.path().join("pi");
        let sd = pdir.join("--w-p--");
        std::fs::create_dir_all(sd.join("2026_x").join("sub")).unwrap();
        std::fs::write(
            sd.join("2026_x.jsonl"),
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"s-1\",\"cwd\":\"/w/p\"}\n",
                "{\"type\":\"model_change\",\"provider\":\"deepseek\",\"modelId\":\"v4\"}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello   pi\"}]}}\n"
            ),
        )
        .unwrap();
        // Sub-agent runs in nested folders are not sessions of their own.
        std::fs::write(sd.join("2026_x").join("sub").join("session.jsonl"), "{\"type\":\"session\",\"id\":\"sub\"}\n").unwrap();
        let h = pi_scan(&pdir);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].summary.id, "s-1");
        assert_eq!(h[0].summary.title, "hello pi");
        assert_eq!(h[0].summary.model.as_deref(), Some("deepseek/v4"));
    }
}
