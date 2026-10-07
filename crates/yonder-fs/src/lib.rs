//! Sandboxed file operations for the yonder host.
//!
//! Every path is canonicalized and must lie inside one of the configured roots and
//! outside every deny path (symlinks are resolved first, so they cannot escape).
//! Deletes go to the OS trash (fallback: a private trash dir); nothing is unlinked.
//! Uploads are written chunk by chunk to a hidden sibling and renamed atomically.
//! The API is synchronous; the daemon calls it from `spawn_blocking`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use yonder_proto::app::{now_ms, ApiError, DirListing, FileEntry, FileKind, FS_READ_MAX, UPLOAD_TEMP_MAX};

/// Max entries returned by one listing.
pub const MAX_LIST: usize = 5000;
const PART_SUFFIX: &str = ".yonder-part";

pub struct FsConfig {
    /// Allowed roots. Empty = the user's home directory.
    pub roots: Vec<PathBuf>,
    /// Denied paths (and everything below them).
    pub deny: Vec<PathBuf>,
    /// Where `upload_temp` stores files (always allowed).
    pub upload_dir: PathBuf,
    /// Where deletes go when the OS trash fails.
    pub fallback_trash: PathBuf,
    /// JSON-lines audit log.
    pub audit_log: Option<PathBuf>,
}

pub struct ReadChunk {
    pub data: Vec<u8>,
    pub eof: bool,
    pub size: u64,
}

type TrashFn = Box<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

pub struct FsService {
    roots: Vec<PathBuf>,
    deny: Vec<PathBuf>,
    upload_dir: PathBuf,
    fallback_trash: PathBuf,
    audit_log: Option<PathBuf>,
    audit_lock: Mutex<()>,
    trash: TrashFn,
}

fn io_err(e: std::io::Error, what: &str) -> ApiError {
    use std::io::ErrorKind::*;
    let msg = format!("{what}: {e}");
    match e.kind() {
        NotFound => ApiError::not_found(msg),
        PermissionDenied => ApiError::forbidden(msg),
        AlreadyExists => ApiError::exists(msg),
        _ => ApiError::internal(msg),
    }
}

fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    dunce::canonicalize(p)
}

/// Component-wise prefix test; case-insensitive on Windows and macOS.
fn is_within(path: &Path, root: &Path) -> bool {
    let norm = |c: Component| -> String {
        let s = c.as_os_str().to_string_lossy().into_owned();
        if cfg!(any(windows, target_os = "macos")) {
            s.to_lowercase()
        } else {
            s
        }
    };
    let mut p = path.components();
    for rc in root.components() {
        match p.next() {
            Some(pc) if norm(pc) == norm(rc) => {}
            _ => return false,
        }
    }
    true
}

fn system_time_ms(t: std::io::Result<std::time::SystemTime>) -> Option<u64> {
    t.ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_millis() as u64)
}

fn is_hidden(name: &str, _meta: Option<&fs::Metadata>) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        if let Some(m) = _meta {
            return m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0;
        }
    }
    false
}

fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned.chars().take(120).collect()
    }
}

fn rand4() -> String {
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    format!("{:02x}{:02x}", b[0], b[1])
}

/// `yyyymmdd-hhmmss` (UTC) without a date crate.
fn timestamp() -> String {
    let secs = now_ms() / 1000;
    let days = secs / 86400;
    let rem = secs % 86400;
    // Civil-from-days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

#[cfg(target_os = "macos")]
fn os_trash(p: &Path) -> Result<(), String> {
    use trash::macos::{DeleteMethod, TrashContextExtMacos};
    let mut ctx = trash::TrashContext::default();
    // NSFileManager works headless (launchd); the Finder method needs AppleScript access.
    ctx.set_delete_method(DeleteMethod::NsFileManager);
    ctx.delete(p).map_err(|e| e.to_string())
}

#[cfg(not(target_os = "macos"))]
fn os_trash(p: &Path) -> Result<(), String> {
    trash::delete(p).map_err(|e| e.to_string())
}

impl FsService {
    pub fn new(cfg: FsConfig) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let roots_in = if cfg.roots.is_empty() { vec![home] } else { cfg.roots };
        let mut roots: Vec<PathBuf> = roots_in.iter().filter_map(|r| canonical(&expand_home(r)).ok()).collect();
        let _ = fs::create_dir_all(&cfg.upload_dir);
        if let Ok(u) = canonical(&cfg.upload_dir) {
            if !roots.iter().any(|r| is_within(&u, r)) {
                roots.push(u);
            }
        }
        roots.dedup();
        let deny = cfg.deny.iter().map(|d| canonical(&expand_home(d)).unwrap_or_else(|_| expand_home(d))).collect();
        Self {
            roots,
            deny,
            upload_dir: cfg.upload_dir,
            fallback_trash: cfg.fallback_trash,
            audit_log: cfg.audit_log,
            audit_lock: Mutex::new(()),
            trash: Box::new(os_trash),
        }
    }

    /// Replace the trash implementation (tests).
    pub fn with_trash(mut self, f: impl Fn(&Path) -> Result<(), String> + Send + Sync + 'static) -> Self {
        self.trash = Box::new(f);
        self
    }

    pub fn roots(&self) -> Vec<String> {
        self.roots.iter().map(|r| r.to_string_lossy().into_owned()).collect()
    }

    pub fn home(&self) -> String {
        dirs::home_dir()
            .and_then(|h| canonical(&h).ok())
            .filter(|h| self.allowed(h))
            .or_else(|| self.roots.first().cloned())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn allowed(&self, p: &Path) -> bool {
        self.roots.iter().any(|r| is_within(p, r)) && !self.deny.iter().any(|d| is_within(p, d))
    }

    fn check(&self, p: PathBuf) -> Result<PathBuf, ApiError> {
        if self.allowed(&p) {
            Ok(p)
        } else {
            Err(ApiError::forbidden(format!("{} is outside the allowed folders", p.display())))
        }
    }

    fn parse(path: &str) -> Result<PathBuf, ApiError> {
        let p = expand_home(Path::new(path.trim()));
        if !p.is_absolute() {
            return Err(ApiError::invalid(format!("path must be absolute: {path}")));
        }
        Ok(p)
    }

    /// Resolve an existing path.
    fn resolve(&self, path: &str) -> Result<PathBuf, ApiError> {
        let p = Self::parse(path)?;
        let c = canonical(&p).map_err(|e| io_err(e, path))?;
        self.check(c)
    }

    /// Resolve a path whose last component may not exist yet (parent must exist).
    fn resolve_new(&self, path: &str) -> Result<PathBuf, ApiError> {
        let p = Self::parse(path)?;
        let name = p.file_name().ok_or_else(|| ApiError::invalid(format!("no file name: {path}")))?.to_os_string();
        let n = name.to_string_lossy();
        if n == "." || n == ".." || n.contains(['/', '\\']) {
            return Err(ApiError::invalid(format!("bad file name: {n}")));
        }
        let parent = p.parent().ok_or_else(|| ApiError::invalid(format!("no parent: {path}")))?;
        let parent = canonical(parent).map_err(|e| io_err(e, "parent folder"))?;
        let full = parent.join(&name);
        // An existing symlink at the target is resolved (and must stay inside).
        if let Ok(meta) = fs::symlink_metadata(&full) {
            if meta.file_type().is_symlink() {
                let c = canonical(&full).map_err(|e| io_err(e, path))?;
                self.check(c)?;
            }
        }
        self.check(full)
    }

    fn entry(&self, path: &Path, name: String) -> Result<FileEntry, ApiError> {
        let lmeta = fs::symlink_metadata(path).map_err(|e| io_err(e, &name))?;
        let meta = if lmeta.file_type().is_symlink() { fs::metadata(path).unwrap_or(lmeta.clone()) } else { lmeta.clone() };
        let kind = if lmeta.file_type().is_symlink() {
            if meta.is_dir() {
                FileKind::Dir
            } else {
                FileKind::Symlink
            }
        } else if meta.is_dir() {
            FileKind::Dir
        } else if meta.is_file() {
            FileKind::File
        } else {
            FileKind::Other
        };
        Ok(FileEntry {
            hidden: is_hidden(&name, Some(&lmeta)),
            name,
            path: path.to_string_lossy().into_owned(),
            kind,
            size: if meta.is_file() { meta.len() } else { 0 },
            mtime: system_time_ms(meta.modified()),
            readonly: meta.permissions().readonly(),
        })
    }

    pub fn list(&self, path: &str, hidden: bool) -> Result<DirListing, ApiError> {
        let dir = self.resolve(path)?;
        let rd = fs::read_dir(&dir).map_err(|e| io_err(e, path))?;
        let mut entries = Vec::new();
        let mut truncated = false;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(PART_SUFFIX) {
                continue;
            }
            let Ok(entry) = self.entry(&e.path(), name) else { continue };
            if entry.hidden && !hidden {
                continue;
            }
            if entries.len() >= MAX_LIST {
                truncated = true;
                break;
            }
            entries.push(entry);
        }
        entries.sort_by(|a, b| {
            let da = a.kind == FileKind::Dir;
            let db = b.kind == FileKind::Dir;
            db.cmp(&da).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let parent = dir.parent().map(Path::to_path_buf).filter(|p| self.allowed(p) && !self.roots.iter().any(|r| r == &dir));
        Ok(DirListing { path: dir.to_string_lossy().into_owned(), parent: parent.map(|p| p.to_string_lossy().into_owned()), entries, truncated })
    }

    pub fn stat(&self, path: &str) -> Result<FileEntry, ApiError> {
        let p = self.resolve(path)?;
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned());
        self.entry(&p, name)
    }

    pub fn read(&self, path: &str, offset: u64, len: u32) -> Result<ReadChunk, ApiError> {
        let p = self.resolve(path)?;
        let mut f = File::open(&p).map_err(|e| io_err(e, path))?;
        let meta = f.metadata().map_err(|e| io_err(e, path))?;
        if meta.is_dir() {
            return Err(ApiError::invalid(format!("{path} is a folder")));
        }
        let size = meta.len();
        let len = len.min(FS_READ_MAX) as u64;
        let offset = offset.min(size);
        f.seek(SeekFrom::Start(offset)).map_err(|e| io_err(e, path))?;
        let mut data = Vec::with_capacity(len.min(size - offset) as usize);
        f.take(len).read_to_end(&mut data).map_err(|e| io_err(e, path))?;
        let eof = offset + data.len() as u64 >= size;
        Ok(ReadChunk { data, eof, size })
    }

    fn part_path(target: &Path) -> PathBuf {
        let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        target.with_file_name(format!(".{name}{PART_SUFFIX}"))
    }

    pub fn write(&self, path: &str, offset: u64, data: &[u8], finish: bool, overwrite: bool) -> Result<(), ApiError> {
        let target = self.resolve_new(path)?;
        if target.is_dir() {
            return Err(ApiError::invalid(format!("{path} is a folder")));
        }
        let part = Self::part_path(&target);
        if offset == 0 {
            if target.exists() && !overwrite {
                return Err(ApiError::exists(format!("{path} already exists")));
            }
            let mut f = File::create(&part).map_err(|e| io_err(e, path))?;
            f.write_all(data).map_err(|e| io_err(e, path))?;
        } else {
            let cur = fs::metadata(&part).map(|m| m.len()).map_err(|_| ApiError::invalid("upload not started (offset must be 0)"))?;
            if cur != offset {
                return Err(ApiError::invalid(format!("offset {offset} does not match uploaded size {cur}")));
            }
            let mut f = OpenOptions::new().append(true).open(&part).map_err(|e| io_err(e, path))?;
            f.write_all(data).map_err(|e| io_err(e, path))?;
        }
        if finish {
            if target.exists() && !overwrite {
                let _ = fs::remove_file(&part);
                return Err(ApiError::exists(format!("{path} already exists")));
            }
            if target.exists() {
                // Windows rename does not replace; keep the old file in the trash.
                #[cfg(windows)]
                {
                    self.delete_resolved(&target)?;
                }
            }
            fs::rename(&part, &target).map_err(|e| io_err(e, path))?;
        }
        Ok(())
    }

    pub fn mkdir(&self, path: &str) -> Result<(), ApiError> {
        let target = self.resolve_new(path)?;
        fs::create_dir(&target).map_err(|e| io_err(e, path))
    }

    pub fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<(), ApiError> {
        let src = Self::parse(from)?;
        // Do not follow a symlink being renamed: resolve its parent only.
        let src = match fs::symlink_metadata(&src) {
            Ok(m) if m.file_type().is_symlink() => self.resolve_new(from)?,
            Ok(_) => self.resolve(from)?,
            Err(e) => return Err(io_err(e, from)),
        };
        if self.roots.iter().any(|r| r == &src) {
            return Err(ApiError::forbidden("cannot rename a root folder"));
        }
        let dst = self.resolve_new(to)?;
        if dst == src {
            return Ok(());
        }
        if fs::symlink_metadata(&dst).is_ok() {
            if !overwrite {
                return Err(ApiError::exists(format!("{to} already exists")));
            }
            self.delete_resolved(&dst)?;
        }
        fs::rename(&src, &dst).map_err(|e| io_err(e, from))
    }

    pub fn delete(&self, path: &str) -> Result<(), ApiError> {
        let p = Self::parse(path)?;
        let target = match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => self.resolve_new(path)?,
            Ok(_) => self.resolve(path)?,
            Err(e) => return Err(io_err(e, path)),
        };
        if self.roots.iter().any(|r| r == &target) {
            return Err(ApiError::forbidden("cannot delete a root folder"));
        }
        self.delete_resolved(&target)
    }

    fn delete_resolved(&self, target: &Path) -> Result<(), ApiError> {
        match (self.trash)(target) {
            Ok(()) if fs::symlink_metadata(target).is_err() => Ok(()),
            _ => {
                fs::create_dir_all(&self.fallback_trash).map_err(|e| io_err(e, "trash folder"))?;
                let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "item".into());
                let dest = self.fallback_trash.join(format!("{}-{}-{name}", now_ms(), rand4()));
                fs::rename(target, &dest).map_err(|e| io_err(e, "move to trash"))
            }
        }
    }

    pub fn upload_temp(&self, name: &str, data: &[u8]) -> Result<String, ApiError> {
        if data.len() > UPLOAD_TEMP_MAX {
            return Err(ApiError::invalid(format!("file too large (max {} MiB)", UPLOAD_TEMP_MAX / 1024 / 1024)));
        }
        fs::create_dir_all(&self.upload_dir).map_err(|e| io_err(e, "upload folder"))?;
        let dir = canonical(&self.upload_dir).map_err(|e| io_err(e, "upload folder"))?;
        let path = dir.join(format!("{}-{}-{}", timestamp(), rand4(), sanitize_name(name)));
        let mut f = OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| io_err(e, "upload"))?;
        f.write_all(data).map_err(|e| io_err(e, "upload"))?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// Append one JSON line to the audit log.
    pub fn audit(&self, device: &str, op: &str, path: &str, result: &Result<(), ApiError>) {
        let Some(log) = &self.audit_log else { return };
        let line = serde_json::json!({
            "ts": now_ms(),
            "device": device,
            "op": op,
            "path": path,
            "ok": result.is_ok(),
            "error": result.as_ref().err().map(|e| e.to_string()),
        });
        let _g = self.audit_lock.lock();
        if let Some(parent) = log.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log) {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// `~` / `~/x` -> home.
pub fn expand_home(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| p.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
        svc: FsService,
    }

    fn env_with(trash_ok: bool) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let base = canonical(tmp.path()).unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join("secret")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("x.txt"), "outside").unwrap();
        let svc = FsService::new(FsConfig {
            roots: vec![root.clone()],
            deny: vec![root.join("secret")],
            upload_dir: base.join("uploads"),
            fallback_trash: base.join("trash"),
            audit_log: Some(base.join("audit.log")),
        })
        .with_trash(move |p| if trash_ok { fs::remove_file(p).or_else(|_| fs::remove_dir_all(p)).map_err(|e| e.to_string()) } else { Err("no trash".into()) });
        Env { _tmp: tmp, root, outside, svc }
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn traversal_and_deny() {
        let e = env_with(true);
        let sneaky = format!("{}/sub/../../outside/x.txt", s(&e.root));
        assert_eq!(e.svc.read(&sneaky, 0, 10).err().unwrap().code, "forbidden");
        assert_eq!(e.svc.list(&s(&e.root.join("secret")), false).err().unwrap().code, "forbidden");
        assert_eq!(e.svc.list("relative/path", false).err().unwrap().code, "invalid");
        assert_eq!(e.svc.stat(&s(&e.root.join("nope"))).err().unwrap().code, "not_found");
        assert_eq!(e.svc.mkdir(&s(&e.outside.join("new"))).err().unwrap().code, "forbidden");
        assert_eq!(e.svc.delete(&s(&e.root)).err().unwrap().code, "forbidden");
        let l = e.svc.list(&s(&e.root), false).unwrap();
        assert!(l.parent.is_none(), "no parent at a root");
        let l = e.svc.list(&s(&e.root.join("sub")), false).unwrap();
        assert_eq!(l.parent.as_deref(), Some(s(&e.root).as_str()));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape() {
        let e = env_with(true);
        std::os::unix::fs::symlink(&e.outside, e.root.join("link")).unwrap();
        assert_eq!(e.svc.list(&s(&e.root.join("link")), false).err().unwrap().code, "forbidden");
        assert_eq!(e.svc.read(&s(&e.root.join("link/x.txt")), 0, 10).err().unwrap().code, "forbidden");
        // Writing through a symlinked parent is refused too.
        assert_eq!(e.svc.write(&s(&e.root.join("link/new.txt")), 0, b"x", true, false).err().unwrap().code, "forbidden");
        // A symlink to a file outside: overwrite target is resolved and refused.
        std::os::unix::fs::symlink(e.outside.join("x.txt"), e.root.join("flink")).unwrap();
        assert_eq!(e.svc.write(&s(&e.root.join("flink")), 0, b"x", true, true).err().unwrap().code, "forbidden");
    }

    #[test]
    fn chunked_write_and_read() {
        let e = env_with(true);
        let p = s(&e.root.join("sub/a.bin"));
        e.svc.write(&p, 0, b"hello ", false, false).unwrap();
        assert_eq!(e.svc.write(&p, 3, b"x", false, false).err().unwrap().code, "invalid");
        e.svc.write(&p, 6, b"world", true, false).unwrap();
        assert_eq!(fs::read(e.root.join("sub/a.bin")).unwrap(), b"hello world");
        assert!(!e.root.join("sub/.a.bin.yonder-part").exists());
        assert_eq!(e.svc.write(&p, 0, b"new", true, false).err().unwrap().code, "exists");
        e.svc.write(&p, 0, b"new", true, true).unwrap();
        assert_eq!(fs::read(e.root.join("sub/a.bin")).unwrap(), b"new");

        let c = e.svc.read(&p, 0, 2).unwrap();
        assert_eq!(c.data, b"ne");
        assert!(!c.eof);
        assert_eq!(c.size, 3);
        let c = e.svc.read(&p, 2, 100).unwrap();
        assert_eq!(c.data, b"w");
        assert!(c.eof);
        let c = e.svc.read(&p, 99, 100).unwrap();
        assert!(c.data.is_empty() && c.eof);
        assert_eq!(e.svc.read(&s(&e.root), 0, 1).err().unwrap().code, "invalid");
    }

    #[test]
    fn rename_mkdir_delete() {
        let e = env_with(true);
        e.svc.mkdir(&s(&e.root.join("d"))).unwrap();
        assert_eq!(e.svc.mkdir(&s(&e.root.join("d"))).err().unwrap().code, "exists");
        fs::write(e.root.join("f.txt"), "1").unwrap();
        fs::write(e.root.join("g.txt"), "2").unwrap();
        assert_eq!(e.svc.rename(&s(&e.root.join("f.txt")), &s(&e.root.join("g.txt")), false).err().unwrap().code, "exists");
        e.svc.rename(&s(&e.root.join("f.txt")), &s(&e.root.join("d/f2.txt")), false).unwrap();
        assert!(e.root.join("d/f2.txt").exists());
        assert_eq!(e.svc.rename(&s(&e.root.join("g.txt")), &s(&e.outside.join("g.txt")), false).err().unwrap().code, "forbidden");
        e.svc.delete(&s(&e.root.join("d/f2.txt"))).unwrap();
        assert!(!e.root.join("d/f2.txt").exists());
    }

    #[test]
    fn delete_falls_back_when_trash_fails() {
        let e = env_with(false);
        fs::write(e.root.join("keep.txt"), "data").unwrap();
        e.svc.delete(&s(&e.root.join("keep.txt"))).unwrap();
        assert!(!e.root.join("keep.txt").exists());
        let trash = e.root.parent().unwrap().join("trash");
        let moved: Vec<_> = fs::read_dir(&trash).unwrap().flatten().collect();
        assert_eq!(moved.len(), 1);
        assert!(moved[0].file_name().to_string_lossy().ends_with("keep.txt"));
        assert_eq!(fs::read_to_string(moved[0].path()).unwrap(), "data");
    }

    #[test]
    fn hidden_sort_and_truncation() {
        let e = env_with(true);
        fs::write(e.root.join(".hidden"), "").unwrap();
        fs::write(e.root.join("B.txt"), "").unwrap();
        fs::write(e.root.join("a.txt"), "").unwrap();
        fs::write(e.root.join(".x.yonder-part"), "").unwrap();
        let l = e.svc.list(&s(&e.root), false).unwrap();
        let names: Vec<_> = l.entries.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["secret", "sub", "a.txt", "B.txt"]);
        let l = e.svc.list(&s(&e.root), true).unwrap();
        assert!(l.entries.iter().any(|x| x.name == ".hidden"));
        assert!(!l.entries.iter().any(|x| x.name.ends_with(".yonder-part")));

        let many = e.root.join("many");
        fs::create_dir(&many).unwrap();
        for i in 0..(MAX_LIST + 3) {
            File::create(many.join(format!("f{i}"))).unwrap();
        }
        let l = e.svc.list(&s(&many), false).unwrap();
        assert!(l.truncated);
        assert_eq!(l.entries.len(), MAX_LIST);
    }

    #[test]
    fn upload_temp_and_audit() {
        let e = env_with(true);
        let p = e.svc.upload_temp("../../evil name?.png", b"img").unwrap();
        let pb = PathBuf::from(&p);
        assert!(pb.starts_with(e.root.parent().unwrap().join("uploads")));
        assert!(p.ends_with("evil name_.png"), "{p}");
        assert_eq!(fs::read(&pb).unwrap(), b"img");
        // Upload dir is readable through the service.
        assert_eq!(e.svc.read(&p, 0, 10).unwrap().data, b"img");
        let big = vec![0u8; UPLOAD_TEMP_MAX + 1];
        assert_eq!(e.svc.upload_temp("big", &big).err().unwrap().code, "invalid");
        e.svc.audit("dev1", "fs_delete", "/x", &Ok(()));
        e.svc.audit("dev1", "fs_write", "/y", &Err(ApiError::forbidden("no")));
        let log = fs::read_to_string(e.root.parent().unwrap().join("audit.log")).unwrap();
        assert_eq!(log.lines().count(), 2);
        assert!(log.contains("\"ok\":false"));
        assert_eq!(timestamp().len(), 15);
    }
}
