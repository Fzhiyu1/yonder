//! Append-only PTY output log with global byte offsets and bounded size.
//!
//! `pty.log` holds bytes `[base, base + len)`; `pty.base` stores `base` as decimal text.
//! When the file grows past `max_len`, the newest `keep_len` bytes are copied into a fresh
//! file and `base` advances. Readers ask for a range by global offset.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::ipc::ExitInfo;
use crate::{EXIT_FILE, LOG_BASE_FILE, LOG_FILE};

pub const DEFAULT_MAX_LEN: u64 = 8 * 1024 * 1024;
pub const DEFAULT_KEEP_LEN: u64 = 4 * 1024 * 1024;

pub struct PtyLog {
    dir: PathBuf,
    file: File,
    base: u64,
    len: u64,
    max_len: u64,
    keep_len: u64,
}

impl PtyLog {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        Self::open_with_limits(dir, DEFAULT_MAX_LEN, DEFAULT_KEEP_LEN)
    }

    pub fn open_with_limits(dir: &Path, max_len: u64, keep_len: u64) -> std::io::Result<Self> {
        let base = read_base(dir);
        let file = OpenOptions::new().create(true).read(true).append(true).open(dir.join(LOG_FILE))?;
        let len = file.metadata()?.len();
        Ok(Self { dir: dir.to_path_buf(), file, base, len, max_len, keep_len })
    }

    /// Global offset right after the last byte.
    pub fn end(&self) -> u64 {
        self.base + self.len
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    /// Append bytes; returns the global offset of the first appended byte.
    pub fn append(&mut self, data: &[u8]) -> std::io::Result<u64> {
        let off = self.end();
        self.file.write_all(data)?;
        self.len += data.len() as u64;
        if self.len > self.max_len {
            self.rotate()?;
        }
        Ok(off)
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let keep = self.keep_len.min(self.len);
        let mut tail = vec![0u8; keep as usize];
        let mut rf = File::open(self.dir.join(LOG_FILE))?;
        rf.seek(SeekFrom::Start(self.len - keep))?;
        rf.read_exact(&mut tail)?;
        let new_base = self.base + (self.len - keep);
        let tmp = self.dir.join(format!("{LOG_FILE}.tmp"));
        {
            let mut t = File::create(&tmp)?;
            t.write_all(&tail)?;
            t.sync_all()?;
        }
        // Write base first: a crash between the two leaves base ahead of the old file,
        // which only makes readers see less history, never wrong offsets for new data.
        write_base(&self.dir, new_base)?;
        std::fs::rename(&tmp, self.dir.join(LOG_FILE))?;
        self.file = OpenOptions::new().read(true).append(true).open(self.dir.join(LOG_FILE))?;
        self.base = new_base;
        self.len = keep;
        Ok(())
    }

    /// Read `[from, end)` if `from` is still retained. Returns None when the range was
    /// rotated away (caller should fall back to a snapshot).
    pub fn read_from(&self, from: u64, max: usize) -> std::io::Result<Option<Vec<u8>>> {
        read_range(&self.dir, from, max)
    }
}

/// Read retained bytes starting at global offset `from` (up to `max` bytes).
pub fn read_range(dir: &Path, from: u64, max: usize) -> std::io::Result<Option<Vec<u8>>> {
    let base = read_base(dir);
    if from < base {
        return Ok(None);
    }
    let mut f = match File::open(dir.join(LOG_FILE)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some(Vec::new())),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();
    let rel = from - base;
    if rel > len {
        return Ok(Some(Vec::new()));
    }
    f.seek(SeekFrom::Start(rel))?;
    let n = ((len - rel) as usize).min(max);
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(Some(buf))
}

fn read_base(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join(LOG_BASE_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_base(dir: &Path, base: u64) -> std::io::Result<()> {
    let tmp = dir.join(format!("{LOG_BASE_FILE}.tmp"));
    std::fs::write(&tmp, base.to_string())?;
    std::fs::rename(tmp, dir.join(LOG_BASE_FILE))
}

pub fn write_exit(dir: &Path, exit: &ExitInfo) -> std::io::Result<()> {
    let tmp = dir.join(format!("{EXIT_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(exit)?)?;
    std::fs::rename(tmp, dir.join(EXIT_FILE))
}

pub fn read_exit(dir: &Path) -> Option<ExitInfo> {
    let s = std::fs::read(dir.join(EXIT_FILE)).ok()?;
    serde_json::from_slice(&s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_offsets() {
        let d = tempfile::tempdir().unwrap();
        let mut log = PtyLog::open_with_limits(d.path(), 100, 40).unwrap();
        let mut all = Vec::new();
        for i in 0..30u8 {
            let chunk = vec![b'a' + (i % 26); 7];
            let off = log.append(&chunk).unwrap();
            assert_eq!(off as usize, all.len());
            all.extend_from_slice(&chunk);
        }
        assert_eq!(log.end() as usize, all.len());
        assert!(log.base() > 0);
        assert!(log.read_from(0, 1 << 20).unwrap().is_none());
        let from = log.base() + 3;
        let got = log.read_from(from, 1 << 20).unwrap().unwrap();
        assert_eq!(got, all[from as usize..]);
        let reopened = PtyLog::open(d.path()).unwrap();
        assert_eq!(reopened.end(), log.end());
    }
}
