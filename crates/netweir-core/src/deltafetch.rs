//! The record of requests whose pages gave items, so a later crawl skips
//! them (as scrapy-deltafetch does). See docs/design/deltafetch.md.
//!
//! SQLite in WAL mode with a busy timeout, like the cache. The keys are
//! read into memory on open, so the check at submit never waits on the
//! file; the caller adds keys in batches, once their items are on disk,
//! with how long each output file was then, in the same transaction.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use crate::canonical::{Fingerprint, fingerprint_with};
use crate::checkpoint::retrying;

/// The layout this build writes, as the file's `user_version`. Newer files
/// are refused.
pub const FORMAT: i64 = 1;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS pages (key BLOB PRIMARY KEY) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS files (
        path TEXT PRIMARY KEY,
        size INTEGER NOT NULL,
        tail TEXT,
        clean INTEGER NOT NULL DEFAULT 0
    );
";

/// How a request is known to the record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DeltaKey {
    /// By the duplicate filter's fingerprint: method, canonical URL, body.
    #[default]
    Fingerprint,
    /// By the caller's own key (scrapy-deltafetch's `meta["deltafetch_key"]`).
    Custom(Vec<u8>),
    /// Not at all: never skipped (`meta["deltafetch_enabled"] = False`).
    Off,
}

/// A request's key in the record: `custom` when it has one, else the
/// duplicate filter's fingerprint. None for a URL that can't be crawled.
pub fn key(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    custom: Option<&[u8]>,
) -> Option<Fingerprint> {
    let Some(custom) = custom else {
        return fingerprint_with(method, url, body);
    };
    // The prefix keeps a custom key from ever equalling a fingerprint,
    // whose digest starts with a method.
    let digest = Sha256::new()
        .chain_update(b"\0deltafetch_key\0")
        .chain_update(custom)
        .finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    Some(out)
}

pub struct Record {
    path: PathBuf,
    /// None once closed.
    conn: Mutex<Option<Connection>>,
    keys: Mutex<HashSet<Fingerprint>>,
    /// Each output file's length when its items were last recorded, and a
    /// digest of what came just before it, as the record was opened.
    files: HashMap<String, Recorded>,
    /// Whether the record had no pages when it was opened.
    empty: bool,
}

/// An output file as last recorded: its length; a digest of its end (the
/// caller's), to tell the same file from another one; and whether the run
/// that wrote it closed it, so that it ends at that length rather than in
/// a tail torn by a crash.
pub type Recorded = (u64, Option<String>, bool);

/// An output file to record: its path, then as `Recorded`.
pub type Output = (String, u64, Option<String>, bool);

impl Record {
    /// Opens or creates the record at `path`; with `reset`, empties it.
    pub fn open(path: &Path, reset: bool) -> Result<Record, String> {
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        let unusable = |e: rusqlite::Error| {
            format!(
                "the deltafetch record at {} can't be used: {e}; delete it to start afresh",
                path.display()
            )
        };
        // The format is read before anything is written, so a file this
        // build can't read is left as it was.
        let (conn, format) = retrying(|| {
            let conn = Connection::open(path)?;
            // Another crawl using the same record: wait for it, don't fail.
            conn.busy_timeout(Duration::from_secs(10))?;
            let format: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            Ok((conn, format))
        })
        .map_err(unusable)?;
        if format > FORMAT {
            return Err(format!(
                "{} was written by a newer netweir (format {format}; this one reads up to {FORMAT})",
                path.display()
            ));
        }
        retrying(|| {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.execute_batch(SCHEMA)?;
            if reset {
                conn.execute_batch("DELETE FROM pages; DELETE FROM files;")?;
            }
            conn.pragma_update(None, "user_version", FORMAT)
        })
        .map_err(unusable)?;
        let (keys, files) = read(&conn).map_err(unusable)?;
        Ok(Record {
            path: path.to_path_buf(),
            conn: Mutex::new(Some(conn)),
            empty: keys.is_empty(),
            keys: Mutex::new(keys),
            files,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether `key` is recorded, from memory: the file isn't touched.
    pub fn has(&self, key: &Fingerprint) -> bool {
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(key)
    }

    /// Each output file as the last crawl recorded it, by the path the
    /// caller gave.
    pub fn files(&self) -> &HashMap<String, Recorded> {
        &self.files
    }

    /// Whether the record had no pages when it was opened: new, or reset.
    pub fn was_empty(&self) -> bool {
        self.empty
    }

    /// Records `keys`, and how long each output file is with their items
    /// in it, in one transaction. The keys count as recorded for this
    /// crawl even if the write fails; a failed write only means a later
    /// crawl fetches those pages again.
    pub fn add(&self, keys: &[Fingerprint], files: &[Output]) -> Result<(), String> {
        if keys.is_empty() && files.is_empty() {
            return Ok(());
        }
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(keys.iter().copied());
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = conn.as_mut() else {
            return Ok(());
        };
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let written = (|| {
            let mut page = tx.prepare_cached("INSERT OR IGNORE INTO pages (key) VALUES (?1)")?;
            for key in keys {
                page.execute(params![key.as_slice()])?;
            }
            let mut file = tx.prepare_cached(
                "INSERT INTO files (path, size, tail, clean) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (path) DO UPDATE
                 SET size = excluded.size, tail = excluded.tail, clean = excluded.clean",
            )?;
            for (path, size, tail, clean) in files {
                file.execute(params![path, *size as i64, tail, clean])?;
            }
            Ok::<_, rusqlite::Error>(())
        })();
        written
            .and_then(|()| tx.commit())
            .map_err(|e| e.to_string())
    }

    /// Lets go of the file. Keys added after this are kept in memory only.
    pub fn close(&self) {
        drop(self.conn.lock().unwrap_or_else(|e| e.into_inner()).take());
    }
}

type Contents = (HashSet<Fingerprint>, HashMap<String, Recorded>);

fn read(conn: &Connection) -> rusqlite::Result<Contents> {
    let mut keys = HashSet::new();
    let mut stmt = conn.prepare("SELECT key FROM pages")?;
    for key in stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))? {
        if let Ok(key) = Fingerprint::try_from(key?.as_slice()) {
            keys.insert(key);
        }
    }
    let mut files = HashMap::new();
    let mut stmt = conn.prepare("SELECT path, size, tail, clean FROM files")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, bool>(3)?,
        ))
    })?;
    for row in rows {
        let (path, size, tail, clean) = row?;
        files.insert(path, (size.max(0) as u64, tail, clean));
    }
    Ok((keys, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_custom_key_is_never_a_fingerprint() {
        let url = "https://example.com/p/1";
        let plain = key("GET", url, None, None).unwrap();
        assert_eq!(Some(plain), fingerprint_with("GET", url, None));
        assert_ne!(key("GET", url, None, Some(url.as_bytes())), Some(plain));
        assert_eq!(
            key("GET", "https://a.example/", None, Some(b"sku-1")),
            key("POST", "https://b.example/", Some(b"x"), Some(b"sku-1")),
            "a custom key stands for the request whatever its URL"
        );
        assert_eq!(key("GET", "not a url", None, None), None);
    }

    #[test]
    fn keys_and_lengths_outlive_the_record_and_reset_empties_it() {
        let dir = std::env::temp_dir().join(format!("netweir-delta-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("deltafetch.sqlite3");
        let k = key("GET", "https://example.com/", None, None).unwrap();
        let record = Record::open(&path, false).unwrap();
        assert!(!record.has(&k));
        assert!(record.was_empty());
        record
            .add(
                &[k, k],
                &[("out.jsonl".into(), 10, Some("a".into()), false)],
            )
            .unwrap();
        record
            .add(&[], &[("out.jsonl".into(), 25, Some("b".into()), true)])
            .unwrap();
        assert!(record.has(&k));
        assert!(record.files().is_empty(), "as it was opened");
        record.close();
        drop(record);
        let again = Record::open(&path, false).unwrap();
        assert!(again.has(&k) && !again.was_empty());
        assert_eq!(
            again.files().get("out.jsonl"),
            Some(&(25, Some("b".to_string()), true))
        );
        drop(again);
        let reset = Record::open(&path, true).unwrap();
        assert!(!reset.has(&k) && reset.files().is_empty() && reset.was_empty());
        drop(reset);
        assert!(
            !Record::open(&path, false).unwrap().has(&k),
            "reset is for good"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_from_a_newer_netweir_is_refused_and_left_alone() {
        let dir = std::env::temp_dir().join(format!("netweir-delta-newer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("deltafetch.sqlite3");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", FORMAT + 1)
            .unwrap();
        let err = Record::open(&path, true).err().unwrap();
        assert!(err.contains("newer netweir"), "{err}");
        let conn = Connection::open(&path).unwrap();
        let tables: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tables, 0);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
