//! Incremental scan cache.
//!
//! A cold scan of the Codex corpus is gigabytes of JSONL. Rescanning it on
//! every launch would make the TUI unusable, so each file gets a watermark:
//! the byte offset we have already parsed, plus the parser state at that
//! point and the events we found. A warm scan seeks past the watermark and
//! reads only what was appended.
//!
//! The watermark is only trustworthy if the file is the same file. Size
//! shrinking, or the (device, inode) pair changing, means truncation or
//! replacement — we drop the entry and reparse from zero.
//!
//! The cache lives in our own state directory. Nothing is ever written into
//! `~/.claude` or `~/.codex`.

use crate::model::UsageEvent;
use crate::parse::ParseCursor;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Bumped whenever the parsers or the on-disk shape change meaning. A stale
/// cache is discarded silently and rebuilt rather than migrated.
pub const SCHEMA_VERSION: u32 = 1;

/// Identity of a file, used to detect replacement under a stable path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl FileId {
    #[cfg(unix)]
    pub fn of(meta: &std::fs::Metadata) -> FileId {
        use std::os::unix::fs::MetadataExt;
        FileId { dev: meta.dev(), ino: meta.ino() }
    }

    /// Windows exposes a volume serial and file index, but only behind the
    /// unstable `windows_by_handle` feature, so a stable build cannot read
    /// them. Creation time is the next best identity: it survives appends and
    /// changes when a path is replaced by a new file, which is the exact event
    /// this guards against.
    #[cfg(windows)]
    pub fn of(meta: &std::fs::Metadata) -> FileId {
        let created = meta
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        FileId { dev: 0, ino: created }
    }

    /// Everywhere else, identity is unavailable and the size/mtime watermark
    /// carries the whole burden of detecting a changed file.
    #[cfg(not(any(unix, windows)))]
    pub fn of(_meta: &std::fs::Metadata) -> FileId {
        FileId::default()
    }
}

/// What we remember about one transcript file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: FileId,
    /// Bytes parsed so far. Always a newline boundary.
    pub offset: u64,
    /// File size when we last looked, for a cheap unchanged check.
    pub size: u64,
    pub mtime_ns: i128,
    pub cursor: ParseCursor,
    pub events: Vec<UsageEvent>,
    pub skipped_synthetic: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    pub files: HashMap<String, FileEntry>,
}

impl Default for Cache {
    fn default() -> Self {
        Cache { version: SCHEMA_VERSION, files: HashMap::new() }
    }
}

impl Cache {
    /// Load the cache, or start fresh if it is missing, unreadable, or from a
    /// different schema version. A broken cache is never fatal — it only costs
    /// one cold scan.
    pub fn load(path: &Path) -> Cache {
        let Ok(text) = std::fs::read_to_string(path) else { return Cache::default() };
        match serde_json::from_str::<Cache>(&text) {
            Ok(c) if c.version == SCHEMA_VERSION => c,
            _ => Cache::default(),
        }
    }

    /// Write atomically: a partial cache file left by a kill would otherwise
    /// be indistinguishable from a valid one on the next run.
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string(self).context("serializing cache")?;
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Forget files that no longer exist, so a long-lived cache does not grow
    /// without bound as sessions are archived or deleted.
    pub fn retain_existing(&mut self, seen: &std::collections::HashSet<String>) {
        self.files.retain(|k, _| seen.contains(k));
    }
}

/// How a file should be handled this scan.
///
/// `Append` is far larger than the two unit variants because it carries a
/// parse cursor. Boxing it to even them out would add an allocation per file
/// on every scan to save stack bytes on a value that is constructed once per
/// file and immediately matched.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Nothing appended since last time; reuse the cached events.
    Unchanged,
    /// Parse from `from_offset`, carrying `cursor`.
    Append { from_offset: u64, cursor: ParseCursor },
    /// Parse the whole file; discard anything cached for it.
    Full,
}

/// Decide what to do with `path` given its metadata and any cache entry.
pub fn plan(entry: Option<&FileEntry>, meta: &std::fs::Metadata) -> Plan {
    let size = meta.len();
    let Some(entry) = entry else { return Plan::Full };

    // Replaced under the same path, or truncated / rotated: the offset is
    // meaningless now.
    if entry.id != FileId::of(meta) || size < entry.offset {
        return Plan::Full;
    }
    if size == entry.offset && mtime_ns(meta) == entry.mtime_ns {
        return Plan::Unchanged;
    }
    if size == entry.offset {
        // Same length, touched: could be an in-place rewrite. Cheap enough to
        // be safe rather than clever.
        return Plan::Full;
    }
    Plan::Append { from_offset: entry.offset, cursor: entry.cursor.clone() }
}

pub fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Read `path` from `offset` to EOF.
pub fn read_from(path: &Path, offset: u64) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if offset > 0 {
        f.seek(SeekFrom::Start(offset))?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn default_path() -> Result<PathBuf> {
    crate::paths::cache_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("readout-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry_for(path: &Path, offset: u64) -> FileEntry {
        let meta = std::fs::metadata(path).unwrap();
        FileEntry {
            id: FileId::of(&meta),
            offset,
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
            cursor: ParseCursor::default(),
            events: vec![],
            skipped_synthetic: 0,
        }
    }

    #[test]
    fn an_untouched_file_is_skipped_and_an_appended_one_resumes() {
        let dir = temp_dir();
        let p = dir.join("a.jsonl");
        std::fs::write(&p, b"line1\n").unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        let e = entry_for(&p, meta.len());
        assert_eq!(plan(Some(&e), &meta), Plan::Unchanged);

        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"line2\n").unwrap();
        drop(f);
        let meta2 = std::fs::metadata(&p).unwrap();
        assert_eq!(
            plan(Some(&e), &meta2),
            Plan::Append { from_offset: 6, cursor: ParseCursor::default() }
        );
        assert_eq!(read_from(&p, 6).unwrap(), b"line2\n");
    }

    #[test]
    fn truncation_forces_a_full_reparse() {
        let dir = temp_dir();
        let p = dir.join("b.jsonl");
        std::fs::write(&p, b"aaaaaaaaaa\n").unwrap();
        let e = entry_for(&p, 11);
        std::fs::write(&p, b"bb\n").unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(plan(Some(&e), &meta), Plan::Full);
    }

    #[test]
    fn a_replaced_file_forces_a_full_reparse() {
        let dir = temp_dir();
        let p = dir.join("c.jsonl");
        std::fs::write(&p, b"x\n").unwrap();
        let mut e = entry_for(&p, 2);
        e.id = FileId { dev: e.id.dev, ino: e.id.ino.wrapping_add(1) };
        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(plan(Some(&e), &meta), Plan::Full);
    }

    #[test]
    fn an_unknown_file_is_parsed_whole() {
        let dir = temp_dir();
        let p = dir.join("d.jsonl");
        std::fs::write(&p, b"x\n").unwrap();
        assert_eq!(plan(None, &std::fs::metadata(&p).unwrap()), Plan::Full);
    }

    #[test]
    fn a_foreign_schema_version_is_discarded_not_migrated() {
        let dir = temp_dir();
        let p = dir.join("cache.json");
        std::fs::write(&p, r#"{"version":999999,"files":{"x":{}}}"#).unwrap();
        assert!(Cache::load(&p).files.is_empty());
        std::fs::write(&p, "not json at all").unwrap();
        assert!(Cache::load(&p).files.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp_dir();
        let p = dir.join("rt.json");
        let mut c = Cache::default();
        c.files.insert(
            "k".into(),
            FileEntry {
                id: FileId { dev: 1, ino: 2 },
                offset: 3,
                size: 3,
                mtime_ns: 4,
                cursor: ParseCursor::default(),
                events: vec![],
                skipped_synthetic: 1,
            },
        );
        c.save(&p).unwrap();
        let back = Cache::load(&p);
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files["k"].offset, 3);
    }
}
