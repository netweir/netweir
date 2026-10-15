//! A crawl's state on disk, so a crawl killed at any point resumes where it
//! stopped.
//!
//! SQLite in WAL mode does the crash safety. Requests are plain data; the
//! caller's own part of a request (callback names, meta) is an opaque JSON
//! string. Writes go through one thread that commits them in order, a batch
//! at a time, so after a crash the file holds everything up to some point
//! and nothing after it: a request is never marked done unless the requests
//! it led to were saved first. Delivery is at least once.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::canonical::Fingerprint;

/// The layout this build writes. Older files are upgraded on open; newer
/// ones are refused.
pub const FORMAT: i64 = 4;

/// A request that was saved and not finished. A shared queue keeps the
/// same fields, as JSON, without the row.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pending {
    #[serde(skip)]
    pub row: i64,
    pub url: String,
    pub priority: i32,
    pub headers: Vec<(String, String)>,
    pub dont_filter: bool,
    pub depth: u32,
    pub payload: String,
    /// The request's method, its kind ("navigate", "form", "fetch"), body,
    /// body type and the page it came from. Format 1 had only GETs.
    pub method: String,
    pub kind: String,
    #[serde(with = "hex_body")]
    pub body: Option<Vec<u8>>,
    pub content_type: Option<String>,
    pub referer: Option<String>,
    pub retry_post: bool,
    /// The proxy the request asked for, if any. Formats 1 and 2 had none.
    pub proxy: Option<String>,
    /// Its timeout, redirect limit and login. Formats 1 to 3, and queue
    /// members written before, had none: the fetcher's own.
    #[serde(default)]
    pub policy: crate::Policy,
}

/// A body as hex in JSON, which has no bytes.
mod hex_body {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(body: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match body {
            Some(b) => s.serialize_str(&b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let Some(hex) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        // By bytes: a string sliced by characters panics on one that isn't
        // ASCII, and a queue's members can be anything.
        let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        if hex.len() % 2 != 0 {
            return Err(D::Error::custom("odd-length hex body"));
        }
        hex.as_bytes()
            .chunks(2)
            .map(|pair| match (digit(pair[0]), digit(pair[1])) {
                (Some(high), Some(low)) => Ok(high << 4 | low),
                _ => Err(D::Error::custom("a body that isn't hex")),
            })
            .collect::<Result<Vec<u8>, _>>()
            .map(Some)
    }
}

/// What a checkpoint held when it was opened.
#[derive(Debug, Default)]
pub struct Saved {
    pub pending: Vec<Pending>,
    pub seen: HashSet<Fingerprint>,
    pub items: HashSet<String>,
    /// The caller's counters, as it last saved them (JSON).
    pub counters: Option<String>,
    /// The next free row number.
    pub next_row: i64,
}

enum Op {
    /// A new request and its URL's seen mark, which must land in the same
    /// commit: seen but not queued, a resume would never fetch it.
    Request(Fingerprint, Box<Pending>),
    Seen(Fingerprint),
    Done(i64),
    Item(String),
    Counters(String),
    /// Items and counters that must land in the same commit.
    Settle(Vec<String>, String),
    /// Commit what's queued and reply.
    Flush(Sender<Result<(), String>>),
}

/// The open checkpoint. Writes are queued and committed by its thread;
/// dropping it commits what's left.
pub struct Checkpoint {
    ops: Mutex<Option<Sender<Op>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
    /// The first write that failed since the last flush.
    failed: Arc<Mutex<Option<String>>>,
    /// How many commits have failed, and how many `commit` has reported.
    failures: Arc<AtomicU64>,
    reported: Mutex<u64>,
}

#[derive(Debug)]
pub struct CheckpointError(pub String);

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CheckpointError {}

fn sql(e: rusqlite::Error) -> CheckpointError {
    CheckpointError(format!("checkpoint: {e}"))
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS requests (
        row INTEGER PRIMARY KEY,
        url TEXT NOT NULL,
        priority INTEGER NOT NULL,
        headers TEXT NOT NULL,
        dont_filter INTEGER NOT NULL,
        depth INTEGER NOT NULL,
        payload TEXT NOT NULL,
        done INTEGER NOT NULL DEFAULT 0,
        method TEXT NOT NULL DEFAULT 'GET',
        kind TEXT NOT NULL DEFAULT 'navigate',
        body BLOB,
        content_type TEXT,
        referer TEXT,
        retry_post INTEGER NOT NULL DEFAULT 0,
        proxy TEXT,
        policy TEXT
    );
    CREATE INDEX IF NOT EXISTS pending ON requests (row) WHERE done = 0;
    CREATE TABLE IF NOT EXISTS seen (fingerprint BLOB PRIMARY KEY) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS items (id TEXT PRIMARY KEY) WITHOUT ROWID;
";

impl Checkpoint {
    /// Opens or creates the checkpoint at `path` and reads back what it
    /// holds.
    pub fn open(path: &Path) -> Result<(Checkpoint, Saved), CheckpointError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| CheckpointError(format!("can't create {}: {e}", dir.display())))?;
        }
        let conn = retrying(|| connect(path)).map_err(sql)?;
        let format: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'format'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(sql)?;
        match format.as_deref().map(str::parse::<i64>) {
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('format', ?1)",
                    params![FORMAT.to_string()],
                )
                .map_err(sql)?;
            }
            Some(Ok(1)) => {
                // Format 1 had only GETs: every request saved is one.
                conn.execute_batch(
                    "BEGIN;
                     ALTER TABLE requests ADD COLUMN method TEXT NOT NULL DEFAULT 'GET';
                     ALTER TABLE requests ADD COLUMN kind TEXT NOT NULL DEFAULT 'navigate';
                     ALTER TABLE requests ADD COLUMN body BLOB;
                     ALTER TABLE requests ADD COLUMN content_type TEXT;
                     ALTER TABLE requests ADD COLUMN referer TEXT;
                     ALTER TABLE requests ADD COLUMN retry_post INTEGER NOT NULL DEFAULT 0;
                     ALTER TABLE requests ADD COLUMN proxy TEXT;
                     ALTER TABLE requests ADD COLUMN policy TEXT;
                     UPDATE meta SET value = '4' WHERE key = 'format';
                     COMMIT;",
                )
                .map_err(sql)?;
            }
            Some(Ok(2)) => {
                conn.execute_batch(
                    "BEGIN;
                     ALTER TABLE requests ADD COLUMN proxy TEXT;
                     ALTER TABLE requests ADD COLUMN policy TEXT;
                     UPDATE meta SET value = '4' WHERE key = 'format';
                     COMMIT;",
                )
                .map_err(sql)?;
            }
            Some(Ok(3)) => {
                // Format 3 had no request policies: each was sent with the
                // crawl's own, which an empty one stands for.
                conn.execute_batch(
                    "BEGIN;
                     ALTER TABLE requests ADD COLUMN policy TEXT;
                     UPDATE meta SET value = '4' WHERE key = 'format';
                     COMMIT;",
                )
                .map_err(sql)?;
            }
            Some(Ok(v)) if v <= FORMAT => {}
            Some(Ok(v)) => {
                return Err(CheckpointError(format!(
                    "{} was written by a newer netweir (format {v}; this one reads up to {FORMAT})",
                    path.display()
                )));
            }
            Some(Err(_)) => {
                return Err(CheckpointError(format!(
                    "{} is not a netweir checkpoint",
                    path.display()
                )));
            }
        }
        let saved = read(&conn).map_err(sql)?;
        let (ops, inbox) = channel();
        let failed = Arc::new(Mutex::new(None));
        let writer_failed = failed.clone();
        let failures = Arc::new(AtomicU64::new(0));
        let writer_failures = failures.clone();
        let writer = std::thread::Builder::new()
            .name("netweir-checkpoint".into())
            .spawn(move || write_loop(conn, inbox, writer_failed, writer_failures))
            .map_err(|e| CheckpointError(format!("can't start the checkpoint writer: {e}")))?;
        Ok((
            Checkpoint {
                ops: Mutex::new(Some(ops)),
                writer: Mutex::new(Some(writer)),
                failed,
                failures,
                reported: Mutex::new(0),
            },
            saved,
        ))
    }

    fn send(&self, op: Op) {
        if let Some(ops) = &*self.ops.lock().unwrap_or_else(|e| e.into_inner()) {
            // The writer only stops when we drop the sender.
            let _ = ops.send(op);
        }
    }

    /// Records a new request as queued and its URL as seen, in one commit.
    pub fn request(&self, fp: Fingerprint, pending: Pending) {
        self.send(Op::Request(fp, Box::new(pending)));
    }

    pub fn seen(&self, fp: Fingerprint) {
        self.send(Op::Seen(fp));
    }

    pub fn done(&self, row: i64) {
        self.send(Op::Done(row));
    }

    pub fn item(&self, id: String) {
        self.send(Op::Item(id));
    }

    pub fn counters(&self, json: String) {
        self.send(Op::Counters(json));
    }

    /// Records items as delivered together with the counters that go with
    /// them (such as how long the output files were), in one commit: after
    /// a crash, either both are saved or neither is.
    pub fn settle(&self, items: Vec<String>, counters: String) {
        self.send(Op::Settle(items, counters));
    }

    /// Commits what's queued and closes the file. Writes after this are
    /// dropped.
    pub fn close(&self) {
        drop(self.ops.lock().unwrap_or_else(|e| e.into_inner()).take());
        let writer = self.writer.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(writer) = writer {
            let _ = writer.join();
        }
    }

    /// Waits until everything queued so far is committed. An error if a
    /// write has failed; that batch is not in the file, so a resume would
    /// do its work again. The error stays: every flush after it, and the
    /// one that closes the crawl, reports it too, so a caller that waits
    /// on a commit mid-crawl can't hide it from the end.
    pub fn flush(&self) -> Result<(), String> {
        let (tx, rx) = channel();
        self.send(Op::Flush(tx));
        let _ = rx.recv();
        match &*self.failed.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Waits until everything queued so far is committed, and says whether
    /// every commit since the last call worked: the writes queued before
    /// this call may have gone in an earlier commit than the one it waited
    /// on. Unlike `flush`, a failure reported once isn't reported again:
    /// the next settle holds the latest counters, so once a later round
    /// commits whole the file is up to date again, but for the failed
    /// batch's own writes, which a resume does again.
    pub fn commit(&self) -> Result<(), String> {
        let (tx, rx) = channel();
        self.send(Op::Flush(tx));
        // No reply: the writer has stopped, so nothing was committed.
        let result = rx
            .recv()
            .unwrap_or_else(|_| Err("the checkpoint writer has stopped".into()));
        let failures = self.failures.load(Ordering::SeqCst);
        let mut reported = self.reported.lock().unwrap_or_else(|e| e.into_inner());
        if failures != *reported {
            *reported = failures;
            let failed = self
                .failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            return Err(failed.unwrap_or_else(|| "a checkpoint write failed".into()));
        }
        result
    }
}

impl Drop for Checkpoint {
    fn drop(&mut self) {
        self.close();
    }
}

/// Runs `open` again for a few seconds while it fails the way a file
/// still held elsewhere does. Right after a crash, the dead process's hold
/// on the file can outlast it for a moment, as can a virus scanner's on a
/// new file, which SQLite reports as an I/O error rather than waiting (on
/// Windows especially).
pub(crate) fn retrying<T>(mut open: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match open() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::SystemIoFailure
                        | rusqlite::ErrorCode::DatabaseBusy
                        | rusqlite::ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            other => return other,
        }
    }
}

/// Opens the file in WAL mode, with the tables in place.
/// Makes `path` readable and writable by its owner alone, creating it
/// empty if it isn't there, as are a WAL and shared-memory file already
/// beside it. SQLite gives the ones it makes the database's own mode. A
/// checkpoint holds the logins requests were given.
#[cfg(unix)]
fn owners_only(path: &Path) -> rusqlite::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let failed = |e: std::io::Error| rusqlite::Error::ToSqlConversionFailure(Box::new(e));
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(failed)?;
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let file = Path::new(&name);
        if file.exists() {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
                .map_err(failed)?;
        }
    }
    Ok(())
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    #[cfg(unix)]
    owners_only(path)?;
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // WAL with NORMAL is crash-safe for the process dying; a power cut may
    // lose the last commits, which resuming fetches again.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

fn read(conn: &Connection) -> rusqlite::Result<Saved> {
    let mut saved = Saved::default();
    let mut stmt = conn.prepare(
        "SELECT row, url, priority, headers, dont_filter, depth, payload,
                method, kind, body, content_type, referer, retry_post, proxy, policy
         FROM requests WHERE done = 0 ORDER BY row",
    )?;
    let rows = stmt.query_map([], |r| {
        let headers: String = r.get(3)?;
        let policy: Option<String> = r.get(14)?;
        Ok(Pending {
            row: r.get(0)?,
            url: r.get(1)?,
            priority: r.get(2)?,
            headers: serde_json::from_str(&headers).unwrap_or_default(),
            dont_filter: r.get(4)?,
            depth: r.get(5)?,
            payload: r.get(6)?,
            method: r.get(7)?,
            kind: r.get(8)?,
            body: r.get(9)?,
            content_type: r.get(10)?,
            referer: r.get(11)?,
            retry_post: r.get(12)?,
            proxy: r.get(13)?,
            // Unreadable, the request would go out without its login or
            // limits: the resume stops and says which.
            policy: match policy {
                None => crate::Policy::default(),
                Some(p) => serde_json::from_str(&p).map_err(|e| {
                    let url: String = r.get(1).unwrap_or_default();
                    rusqlite::Error::FromSqlConversionFailure(
                        14,
                        rusqlite::types::Type::Text,
                        format!("the saved policy of {url} can't be read ({e})").into(),
                    )
                })?,
            },
        })
    })?;
    for row in rows {
        saved.pending.push(row?);
    }
    let mut stmt = conn.prepare("SELECT fingerprint FROM seen")?;
    for fp in stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))? {
        if let Ok(fp) = Fingerprint::try_from(fp?.as_slice()) {
            saved.seen.insert(fp);
        }
    }
    let mut stmt = conn.prepare("SELECT id FROM items")?;
    for id in stmt.query_map([], |r| r.get::<_, String>(0))? {
        saved.items.insert(id?);
    }
    saved.counters = conn
        .query_row("SELECT value FROM meta WHERE key = 'counters'", [], |r| {
            r.get(0)
        })
        .optional()?;
    saved.next_row = conn.query_row("SELECT COALESCE(MAX(row), 0) + 1 FROM requests", [], |r| {
        r.get(0)
    })?;
    Ok(saved)
}

/// Commits the queued writes in order, a batch per transaction.
fn write_loop(
    mut conn: Connection,
    inbox: Receiver<Op>,
    failed: Arc<Mutex<Option<String>>>,
    failures: Arc<AtomicU64>,
) {
    // Waits for the first write, then takes whatever else arrives within
    // this long, so a busy crawl commits in batches rather than per row.
    const GATHER: Duration = Duration::from_millis(50);
    while let Ok(first) = inbox.recv() {
        let waited_on = matches!(first, Op::Flush(_));
        let mut batch = vec![first];
        let until = std::time::Instant::now() + GATHER;
        while !waited_on && batch.len() < 10_000 {
            let left = until.saturating_duration_since(std::time::Instant::now());
            match inbox.recv_timeout(left) {
                // Someone is waiting on this commit: make it now.
                Ok(op @ Op::Flush(_)) => {
                    batch.push(op);
                    break;
                }
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }
        // Flushes are answered after the commit's result is known, even
        // when it fails part way.
        let (flushes, writes): (Vec<Op>, Vec<Op>) =
            batch.into_iter().partition(|op| matches!(op, Op::Flush(_)));
        let result = commit(&mut conn, writes).map_err(|e| format!("checkpoint write failed: {e}"));
        if let Err(e) = &result {
            // The crawl goes on; every flush from now on reports it, and a
            // resume would do this batch's work again.
            failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(|| e.clone());
            failures.fetch_add(1, Ordering::SeqCst);
        }
        for op in flushes {
            if let Op::Flush(reply) = op {
                let _ = reply.send(result.clone());
            }
        }
    }
}

fn commit(conn: &mut Connection, batch: Vec<Op>) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut request = tx.prepare_cached(
            "INSERT OR REPLACE INTO requests (row, url, priority, headers, dont_filter, depth, payload,
                 method, kind, body, content_type, referer, retry_post, proxy, policy)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?;
        let mut seen = tx.prepare_cached("INSERT OR IGNORE INTO seen (fingerprint) VALUES (?1)")?;
        let mut done = tx.prepare_cached("UPDATE requests SET done = 1 WHERE row = ?1")?;
        let mut item = tx.prepare_cached("INSERT OR IGNORE INTO items (id) VALUES (?1)")?;
        let mut counters = tx.prepare_cached(
            "INSERT INTO meta (key, value) VALUES ('counters', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )?;
        for op in batch {
            match op {
                Op::Request(fp, p) => {
                    seen.execute(params![fp.as_slice()])?;
                    let headers = serde_json::to_string(&p.headers).unwrap_or_else(|_| "[]".into());
                    let policy = (p.policy != crate::Policy::default())
                        .then(|| serde_json::to_string(&p.policy).ok())
                        .flatten();
                    request.execute(params![
                        p.row,
                        p.url,
                        p.priority,
                        headers,
                        p.dont_filter,
                        p.depth,
                        p.payload,
                        p.method,
                        p.kind,
                        p.body,
                        p.content_type,
                        p.referer,
                        p.retry_post,
                        p.proxy,
                        policy
                    ])?;
                }
                Op::Seen(fp) => {
                    seen.execute(params![fp.as_slice()])?;
                }
                Op::Done(row) => {
                    done.execute(params![row])?;
                }
                Op::Item(id) => {
                    item.execute(params![id])?;
                }
                Op::Counters(json) => {
                    counters.execute(params![json])?;
                }
                Op::Settle(ids, json) => {
                    for id in ids {
                        item.execute(params![id])?;
                    }
                    counters.execute(params![json])?;
                }
                Op::Flush(_) => {}
            }
        }
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checkpoint_another_process_still_holds_is_waited_for() {
        // As after a crash, when the dead process's lock on the file can
        // outlive it for a moment (on Windows especially).
        let dir = std::env::temp_dir().join(format!("netweir-cp-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("crawl.sqlite3");
        drop(Checkpoint::open(&path).unwrap());
        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            holder.execute_batch("COMMIT").unwrap();
        });
        let opened = Checkpoint::open(&path);
        release.join().unwrap();
        assert!(opened.is_ok(), "{:?}", opened.err());
        drop(opened);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn pending(row: i64, url: &str) -> Pending {
        Pending {
            row,
            url: url.into(),
            priority: 2,
            headers: vec![("x-a".into(), "1".into())],
            dont_filter: false,
            depth: 3,
            payload: r#"{"callback":"parse"}"#.into(),
            method: "GET".into(),
            kind: "navigate".into(),
            body: None,
            content_type: None,
            referer: Some("https://e.com/".into()),
            retry_post: false,
            proxy: Some("http://ann:secret@10.0.0.1:8080".into()),
            policy: crate::Policy {
                timeout: Some(Duration::from_millis(2500)),
                max_redirects: Some(4),
                dont_redirect: true,
                auth: Some(crate::Auth::new(
                    "ada",
                    format!("pw-{}", std::process::id()),
                    Some("e.com".into()),
                )),
            },
        }
    }

    /// A checkpoint holds requests' logins: only its owner may read it,
    /// nor its write-ahead log.
    #[cfg(unix)]
    #[test]
    fn a_checkpoint_is_its_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("netweir-cp-mode-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let (cp, _) = Checkpoint::open(&path).unwrap();
        cp.request([5; 16], pending(5, "https://e.com/b"));
        cp.flush().unwrap();
        for file in ["crawl.sqlite3", "crawl.sqlite3-wal", "crawl.sqlite3-shm"] {
            let mode = std::fs::metadata(dir.join(file))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{file}");
        }
        drop(cp);
        // One made before, readable by anyone, is closed off on opening.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(Checkpoint::open(&path).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_policy_that_cant_be_read_stops_the_resume() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-bad-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let (cp, _) = Checkpoint::open(&path).unwrap();
        cp.request([5; 16], pending(5, "https://e.com/b"));
        drop(cp);
        Connection::open(&path)
            .unwrap()
            .execute("UPDATE requests SET policy = '{\"timeout\": \"soon\"' ", [])
            .unwrap();
        let err = Checkpoint::open(&path).err().unwrap().to_string();
        assert!(err.contains("https://e.com/b"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_checkpoint_from_before_request_policies_is_upgraded() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-v3-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The third format's requests table, as 0.3.1 wrote it.
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES ('format', '3');
                 CREATE TABLE requests (row INTEGER PRIMARY KEY, url TEXT NOT NULL,
                     priority INTEGER NOT NULL, headers TEXT NOT NULL,
                     dont_filter INTEGER NOT NULL, depth INTEGER NOT NULL,
                     payload TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0,
                     method TEXT NOT NULL DEFAULT 'GET',
                     kind TEXT NOT NULL DEFAULT 'navigate', body BLOB,
                     content_type TEXT, referer TEXT,
                     retry_post INTEGER NOT NULL DEFAULT 0, proxy TEXT);
                 INSERT INTO requests (row, url, priority, headers, dont_filter, depth, payload)
                     VALUES (4, 'https://e.com/a', 0, '[]', 0, 1, '{}');",
            )
            .unwrap();
        let (cp, saved) = Checkpoint::open(&path).unwrap();
        assert_eq!(saved.pending[0].policy, crate::Policy::default());
        cp.request([5; 16], pending(5, "https://e.com/b"));
        drop(cp);
        let (_cp, saved) = Checkpoint::open(&path).unwrap();
        assert_eq!(saved.pending[1], pending(5, "https://e.com/b"));
        drop(_cp);
        let conn = Connection::open(&path).unwrap();
        let format: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'format'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(format, FORMAT.to_string());
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_was_written_comes_back_and_done_requests_do_not() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let (cp, saved) = Checkpoint::open(&path).unwrap();
            assert!(saved.pending.is_empty());
            assert_eq!(saved.next_row, 1);
            cp.request([1; 16], pending(1, "https://e.com/a"));
            cp.request([2; 16], pending(2, "https://e.com/b"));
            cp.seen([7; 16]);
            cp.done(1);
            cp.item("abc".into());
            cp.counters(r#"{"items":1}"#.into());
        }
        let (_cp, saved) = Checkpoint::open(&path).unwrap();
        assert_eq!(saved.pending, vec![pending(2, "https://e.com/b")]);
        assert!(saved.seen.contains(&[7; 16]));
        assert!(saved.items.contains("abc"));
        assert_eq!(saved.counters.as_deref(), Some(r#"{"items":1}"#));
        assert_eq!(saved.next_row, 3);
        drop(_cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_request_and_its_seen_mark_are_never_committed_apart() {
        // Committed apart, a crash between the two commits would leave the
        // URL seen but not queued: a resume would drop it as a duplicate
        // and never fetch it.
        let dir = std::env::temp_dir().join(format!("netweir-cp-pair-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        drop(Checkpoint::open(&path).unwrap());
        let mut conn = connect(&path).unwrap();
        // One write, as the writer thread may cut a batch after any op.
        commit(
            &mut conn,
            vec![Op::Request(
                [9; 16],
                Box::new(pending(1, "https://e.com/a")),
            )],
        )
        .unwrap();
        drop(conn);
        let (_cp, saved) = Checkpoint::open(&path).unwrap();
        assert_eq!(saved.pending, vec![pending(1, "https://e.com/a")]);
        assert!(saved.seen.contains(&[9; 16]));
        drop(_cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_write_is_reported_by_flush() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-fail-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let (cp, _) = Checkpoint::open(&path).unwrap();
        cp.flush().unwrap();
        Connection::open(&path)
            .unwrap()
            .execute("DROP TABLE requests", [])
            .unwrap();
        cp.request([1; 16], pending(1, "https://e.com/a"));
        let err = cp.commit().unwrap_err();
        assert!(err.contains("requests"), "{err}");
        // The table is back: the next commit works, and says so.
        Connection::open(&path)
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
        cp.request([2; 16], pending(2, "https://e.com/b"));
        cp.commit().unwrap();
        // The failure is still there for the flush that closes the crawl,
        // whoever committed in between.
        assert!(cp.flush().unwrap_err().contains("requests"));
        assert!(cp.flush().is_err(), "reported until the end");
        drop(cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_reports_a_failure_in_a_commit_it_did_not_wait_on() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-earlier-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let (cp, _) = Checkpoint::open(&path).unwrap();
        cp.commit().unwrap();
        Connection::open(&path)
            .unwrap()
            .execute("DROP TABLE requests", [])
            .unwrap();
        // The write goes in a commit before the one commit() waits on, as
        // when the writer's gathering window ends between the two.
        cp.request([1; 16], pending(1, "https://e.com/a"));
        let _ = cp.flush();
        Connection::open(&path)
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
        assert!(cp.commit().unwrap_err().contains("requests"));
        // Reported once: the next round commits whole.
        cp.request([2; 16], pending(2, "https://e.com/b"));
        cp.commit().unwrap();
        // With the writer gone, nothing can have been committed.
        cp.close();
        assert!(cp.commit().is_err());
        drop(cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_checkpoint_from_before_methods_and_bodies_is_upgraded() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-v1-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The first format's layout, as 0.1.0 wrote it.
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES ('format', '1');
                 CREATE TABLE requests (row INTEGER PRIMARY KEY, url TEXT NOT NULL,
                     priority INTEGER NOT NULL, headers TEXT NOT NULL,
                     dont_filter INTEGER NOT NULL, depth INTEGER NOT NULL,
                     payload TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO requests (row, url, priority, headers, dont_filter, depth, payload)
                     VALUES (4, 'https://e.com/a', 0, '[]', 0, 1, '{}');",
            )
            .unwrap();
        let (cp, saved) = Checkpoint::open(&path).unwrap();
        let p = &saved.pending[0];
        assert_eq!(
            (p.row, p.method.as_str(), p.kind.as_str()),
            (4, "GET", "navigate")
        );
        assert_eq!(
            (
                p.body.as_ref(),
                p.referer.as_ref(),
                p.retry_post,
                p.proxy.as_ref()
            ),
            (None, None, false, None)
        );
        // And it takes requests with bodies from then on.
        let mut post = pending(5, "https://e.com/search");
        post.method = "POST".into();
        post.kind = "form".into();
        post.body = Some(b"q=rust".to_vec());
        cp.request([5; 16], post.clone());
        drop(cp);
        let (_cp, saved) = Checkpoint::open(&path).unwrap();
        assert_eq!(saved.pending[1], post);
        drop(_cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_checkpoint_from_before_request_proxies_is_upgraded() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-v2-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The second format's requests table, as 0.2.0 wrote it.
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES ('format', '2');
                 CREATE TABLE requests (row INTEGER PRIMARY KEY, url TEXT NOT NULL,
                     priority INTEGER NOT NULL, headers TEXT NOT NULL,
                     dont_filter INTEGER NOT NULL, depth INTEGER NOT NULL,
                     payload TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0,
                     method TEXT NOT NULL DEFAULT 'GET',
                     kind TEXT NOT NULL DEFAULT 'navigate', body BLOB,
                     content_type TEXT, referer TEXT,
                     retry_post INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO requests (row, url, priority, headers, dont_filter, depth, payload)
                     VALUES (4, 'https://e.com/a', 0, '[]', 0, 1, '{}');",
            )
            .unwrap();
        let (cp, saved) = Checkpoint::open(&path).unwrap();
        assert_eq!(saved.pending[0].proxy, None);
        cp.request([5; 16], pending(5, "https://e.com/b"));
        drop(cp);
        let (_cp, saved) = Checkpoint::open(&path).unwrap();
        assert_eq!(saved.pending[1], pending(5, "https://e.com/b"));
        drop(_cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_newer_format_is_refused() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-new-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        drop(Checkpoint::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE meta SET value = '99' WHERE key = 'format'", [])
            .unwrap();
        drop(conn);
        let err = Checkpoint::open(&path).err().unwrap();
        assert!(err.to_string().contains("newer netweir"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
