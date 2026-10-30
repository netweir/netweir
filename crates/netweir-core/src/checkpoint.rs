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
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::canonical::Fingerprint;

/// The layout this build writes. Older files are upgraded on open; newer
/// ones are refused.
pub const FORMAT: i64 = 7;

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
    /// One with a login is kept as `masked` writes it, with the SHA-256
    /// of its whole URL beside it, which a resume finds the login by
    /// among the crawl's proxies. Format 5, and queue members written
    /// before, kept the login in it, and no hash.
    pub proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_sha256: Option<String>,
    /// Its timeout, redirect limit and login, as `saved_policy` keeps
    /// them. Formats 1 to 3, and queue members written before, had none:
    /// the fetcher's own. Formats 4 to 6, and queue members written
    /// before, kept the login's password.
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
    /// Each cookie jar the crawl kept, by name ("" for its own), as
    /// `Jar::snapshot` wrote it.
    pub jars: Vec<(String, String)>,
    /// The jar of each host's own session (one a block gave it): its
    /// host, its snapshot, and the identity the session had (JSON: its
    /// profile and proxy), which its cookies are only good with.
    pub host_jars: Vec<(String, String, Option<String>)>,
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
    /// A cookie jar's snapshot, by kind ("jar" or "host") and name, and
    /// for a host's, its session's identity.
    Jar(&'static str, String, String, Option<String>),
    /// Commit what's queued and reply.
    Flush(Sender<Result<(), String>>),
}

/// How many writes a failing checkpoint holds back to try again before
/// it stops: about 300 MB of them, at some 290 bytes each, and several
/// hundred thousand pages' worth of work that a resume does again. A
/// write that fails for a moment (a file a virus scanner or a dead
/// process still holds, on Windows) is long through by then; one that
/// has failed for that long (a full disk, a file that went away) isn't
/// going to start working, and holding on would end the process for
/// want of memory instead, with no word of why.
const MAX_HELD: usize = 1_000_000;

/// The open checkpoint. Writes are queued and committed by its thread;
/// dropping it commits what's left.
pub struct Checkpoint {
    ops: Mutex<Option<Sender<Op>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
    health: Arc<Mutex<Health>>,
}

/// What the writer has made of its commits, for the crawl to read
/// without waiting on one.
#[derive(Default)]
struct Health {
    /// Why the writes not yet committed failed, while they wait to go
    /// again with the next commit.
    failed: Option<String>,
    /// Commits that have failed.
    failures: u64,
    /// Whether the writer has given up: it held more than it may, and
    /// now drops every write.
    stopped: bool,
}

/// The writes whose commit failed, in order, each to go again with the
/// next. Those a later write replaces whole are let go: the counters,
/// and a cookie jar's snapshot.
#[derive(Default)]
struct Held {
    ops: Vec<Option<Op>>,
    /// Rows the ops would write: a settle's items count one each.
    rows: usize,
    counters: Option<usize>,
    jars: std::collections::HashMap<(&'static str, String), usize>,
}

impl Held {
    fn push(&mut self, op: Op) {
        let at = self.ops.len();
        let replaced = match &op {
            Op::Counters(_) => self.counters.replace(at),
            Op::Jar(kind, name, ..) => self.jars.insert((*kind, name.clone()), at),
            _ => None,
        };
        if let Some(old) = replaced.and_then(|i| self.ops[i].take()) {
            self.rows -= rows(&old);
        }
        self.rows += rows(&op);
        self.ops.push(Some(op));
    }

    fn clear(&mut self) {
        *self = Held::default();
    }
}

fn rows(op: &Op) -> usize {
    match op {
        Op::Settle(ids, _) => ids.len() + 1,
        _ => 1,
    }
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
        policy TEXT,
        proxy_sha256 TEXT
    );
    CREATE INDEX IF NOT EXISTS pending ON requests (row) WHERE done = 0;
    CREATE TABLE IF NOT EXISTS seen (fingerprint BLOB PRIMARY KEY) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS items (id TEXT PRIMARY KEY) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS jars (kind TEXT NOT NULL, name TEXT NOT NULL,
        cookies TEXT NOT NULL, identity TEXT, PRIMARY KEY (kind, name)) WITHOUT ROWID;
";

impl Checkpoint {
    /// Opens or creates the checkpoint at `path` and reads back what it
    /// holds.
    pub fn open(path: &Path) -> Result<(Checkpoint, Saved), CheckpointError> {
        Checkpoint::open_holding(path, MAX_HELD)
    }

    /// As `open`, with the writer giving up past `max_held` writes held
    /// back.
    fn open_holding(path: &Path, max_held: usize) -> Result<(Checkpoint, Saved), CheckpointError> {
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
                     ALTER TABLE requests ADD COLUMN proxy_sha256 TEXT;
                     UPDATE meta SET value = '6' WHERE key = 'format';
                     COMMIT;",
                )
                .map_err(sql)?;
            }
            Some(Ok(2)) => {
                conn.execute_batch(
                    "BEGIN;
                     ALTER TABLE requests ADD COLUMN proxy TEXT;
                     ALTER TABLE requests ADD COLUMN policy TEXT;
                     ALTER TABLE requests ADD COLUMN proxy_sha256 TEXT;
                     UPDATE meta SET value = '6' WHERE key = 'format';
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
                     ALTER TABLE requests ADD COLUMN proxy_sha256 TEXT;
                     UPDATE meta SET value = '6' WHERE key = 'format';
                     COMMIT;",
                )
                .map_err(sql)?;
            }
            Some(Ok(4 | 5)) => {
                // Format 4 kept no cookies: its `jars` table, made with the
                // others, is empty, so a resume starts as a new visitor.
                conn.execute_batch(
                    "BEGIN;
                     ALTER TABLE requests ADD COLUMN proxy_sha256 TEXT;
                     UPDATE meta SET value = '6' WHERE key = 'format';
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
        if matches!(format.as_deref().map(str::parse::<i64>), Some(Ok(v)) if v < FORMAT) {
            hide_proxy_logins(&conn).map_err(sql)?;
            hide_request_logins(&conn).map_err(sql)?;
            conn.execute(
                "UPDATE meta SET value = ?1 WHERE key = 'format'",
                params![FORMAT.to_string()],
            )
            .map_err(sql)?;
        }
        let saved = read(&conn).map_err(sql)?;
        let (ops, inbox) = channel();
        let health = Arc::new(Mutex::new(Health::default()));
        let writer_health = health.clone();
        let writer = std::thread::Builder::new()
            .name("netweir-checkpoint".into())
            .spawn(move || write_loop(conn, inbox, writer_health, max_held))
            .map_err(|e| CheckpointError(format!("can't start the checkpoint writer: {e}")))?;
        Ok((
            Checkpoint {
                ops: Mutex::new(Some(ops)),
                writer: Mutex::new(Some(writer)),
                health,
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

    /// Saves a cookie jar's snapshot under `name`, replacing the last.
    pub fn jar(&self, name: String, snapshot: String) {
        self.send(Op::Jar("jar", name, snapshot, None));
    }

    /// Saves the snapshot of `host`'s own session's jar, and the identity
    /// the session goes out with (JSON), replacing the last. Kept apart
    /// from the named jars, so no name can clash.
    pub fn host_jar(&self, host: String, snapshot: String, identity: String) {
        self.send(Op::Jar("host", host, snapshot, Some(identity)));
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

    /// Waits until everything queued so far is committed. An error while
    /// a write has failed: it isn't in the file, and goes again with every
    /// commit after it until one works, so no later commit lands without
    /// it. A resume from the file as it is does that work again.
    pub fn flush(&self) -> Result<(), String> {
        let (tx, rx) = channel();
        self.send(Op::Flush(tx));
        let _ = rx.recv();
        match self.failing() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn health(&self) -> std::sync::MutexGuard<'_, Health> {
        self.health.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Why the last commit failed, if it did, without waiting on one.
    pub fn failing(&self) -> Option<String> {
        self.health().failed.clone()
    }

    /// Whether the writer has stopped for good, having held back more
    /// failed writes than it may. `failing` says why.
    pub fn stopped(&self) -> bool {
        self.health().stopped
    }

    /// Commits that have failed so far.
    pub fn failures(&self) -> u64 {
        self.health().failures
    }

    /// As `flush`, but an error too when the writer has stopped, as
    /// nothing queued since can have been committed.
    pub fn commit(&self) -> Result<(), String> {
        let (tx, rx) = channel();
        self.send(Op::Flush(tx));
        rx.recv()
            .unwrap_or_else(|_| Err("the checkpoint writer has stopped".into()))
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
/// checkpoint holds the logins requests were given and the crawl's
/// cookies, the cache the pages they logged in to.
#[cfg(unix)]
pub(crate) fn owners_only(path: &Path) -> rusqlite::Result<()> {
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

/// The SHA-256 of a proxy's URL, login and all, in hex. Unkeyed: a key
/// would have to be kept in the same file, so it would hide nothing from
/// whoever can read the checkpoint.
pub(crate) fn proxy_sha256(url: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(url.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `proxy` as a checkpoint or a shared queue keeps it: without its
/// login, and the hash a resume finds the login by. One with no login
/// is kept as it is.
pub fn saved_proxy(proxy: Option<&str>) -> (Option<String>, Option<String>) {
    match proxy {
        Some(p) if crate::fetch::masked(p) != p => {
            (Some(crate::fetch::masked(p)), Some(proxy_sha256(p)))
        }
        p => (p.map(str::to_string), None),
    }
}

/// Formats 3 to 5 kept each request's proxy, login and all: from now
/// on, as `saved_proxy` keeps it.
fn hide_proxy_logins(conn: &Connection) -> rusqlite::Result<()> {
    let rows = conn
        .prepare("SELECT row, proxy FROM requests WHERE proxy IS NOT NULL")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let tx = conn.unchecked_transaction()?;
    for (row, proxy) in rows {
        if let (proxy, Some(sha)) = saved_proxy(Some(&proxy)) {
            tx.execute(
                "UPDATE requests SET proxy = ?1, proxy_sha256 = ?2 WHERE row = ?3",
                params![proxy, sha, row],
            )?;
        }
    }
    tx.commit()
}

/// `policy` as a checkpoint or a shared queue keeps it: a login without
/// its password. An empty password has nothing to hide, and needs no
/// finding on resume.
pub(crate) fn saved_policy(policy: &crate::Policy) -> crate::Policy {
    let mut policy = policy.clone();
    if let Some(auth) = policy.auth.as_mut().filter(|a| !a.password.is_empty()) {
        auth.password.clear();
        auth.hidden = true;
    }
    policy
}

/// Formats 4 to 6 kept each request's login, password and all: from now
/// on, as `saved_policy` keeps it. A policy that can't be read is left
/// for `read` to report.
fn hide_request_logins(conn: &Connection) -> rusqlite::Result<()> {
    let rows = conn
        .prepare("SELECT row, policy FROM requests WHERE policy IS NOT NULL")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let tx = conn.unchecked_transaction()?;
    for (row, json) in rows {
        let Ok(policy) = serde_json::from_str::<crate::Policy>(&json) else {
            continue;
        };
        let saved = saved_policy(&policy);
        if saved != policy {
            let json = serde_json::to_string(&saved)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
            tx.execute(
                "UPDATE requests SET policy = ?1 WHERE row = ?2",
                params![json, row],
            )?;
        }
    }
    tx.commit()
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    #[cfg(unix)]
    owners_only(path)?;
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // WAL with NORMAL is crash-safe for the process dying; a power cut may
    // lose the last commits, which resuming fetches again.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // What's deleted or replaced is overwritten too, so a proxy login
    // hidden on upgrade is gone from the file, not just unlisted.
    conn.pragma_update(None, "secure_delete", "ON")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

fn read(conn: &Connection) -> rusqlite::Result<Saved> {
    let mut saved = Saved::default();
    let mut stmt = conn.prepare(
        "SELECT row, url, priority, headers, dont_filter, depth, payload,
                method, kind, body, content_type, referer, retry_post, proxy, policy,
                proxy_sha256
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
            proxy_sha256: r.get(15)?,
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
    let mut stmt = conn.prepare("SELECT kind, name, cookies, identity FROM jars")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    for row in rows {
        let (kind, name, cookies, identity) = row?;
        match kind.as_str() {
            "host" => saved.host_jars.push((name, cookies, identity)),
            _ => saved.jars.push((name, cookies)),
        }
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
    health: Arc<Mutex<Health>>,
    max_held: usize,
) {
    // Waits for the first write, then takes whatever else arrives within
    // this long, so a busy crawl commits in batches rather than per row.
    const GATHER: Duration = Duration::from_millis(50);
    // Writes whose commit failed, which go again, first, with the next.
    // Committed without them, a later batch would leave the file
    // inconsistent: counters saying how long the output files are with
    // items in them that the items table doesn't have, whose requests
    // aren't done either, so a resume would write them again. So a
    // write that can never be made stops the checkpoint where it was
    // until the writer gives up, past max_held.
    let mut held = Held::default();
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
        let stopped = health.lock().unwrap_or_else(|e| e.into_inner()).stopped;
        let result = if stopped {
            // Given up: the file stays as the last commit that worked
            // left it, which a resume carries on from.
            Err(health
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .failed
                .clone()
                .unwrap_or_default())
        } else {
            for op in writes {
                held.push(op);
            }
            let result = commit(&mut conn, held.ops.iter().flatten())
                .map_err(|e| format!("checkpoint write failed: {e}"));
            let mut health = health.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(()) => {
                    held.clear();
                    health.failed = None;
                    Ok(())
                }
                // The crawl goes on, and every flush says so until a
                // commit with these writes in it works.
                Err(e) => {
                    health.failures += 1;
                    let e = if held.rows > max_held {
                        health.stopped = true;
                        let e = format!(
                            "{e}, and {} checkpoint writes are held back waiting on it, \
                             more than the {max_held} it keeps: the checkpoint has \
                             stopped, as it was at its last commit that worked",
                            held.rows
                        );
                        held.clear();
                        e
                    } else {
                        e
                    };
                    health.failed = Some(e.clone());
                    Err(e)
                }
            }
        };
        for op in flushes {
            if let Op::Flush(reply) = op {
                let _ = reply.send(result.clone());
            }
        }
    }
}

fn commit<'a>(
    conn: &mut Connection,
    batch: impl IntoIterator<Item = &'a Op>,
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut request = tx.prepare_cached(
            "INSERT OR REPLACE INTO requests (row, url, priority, headers, dont_filter, depth, payload,
                 method, kind, body, content_type, referer, retry_post, proxy, policy,
                 proxy_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?;
        let mut seen = tx.prepare_cached("INSERT OR IGNORE INTO seen (fingerprint) VALUES (?1)")?;
        let mut done = tx.prepare_cached("UPDATE requests SET done = 1 WHERE row = ?1")?;
        let mut item = tx.prepare_cached("INSERT OR IGNORE INTO items (id) VALUES (?1)")?;
        let mut jar = tx.prepare_cached(
            "INSERT INTO jars (kind, name, cookies, identity) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (kind, name)
             DO UPDATE SET cookies = excluded.cookies, identity = excluded.identity",
        )?;
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
                        policy,
                        p.proxy_sha256
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
                Op::Jar(kind, name, snapshot, identity) => {
                    jar.execute(params![kind, name, snapshot, identity])?;
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
            proxy: Some("http://***@10.0.0.1:8080".into()),
            proxy_sha256: Some(proxy_sha256("http://ann:secret@10.0.0.1:8080")),
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

    #[cfg(unix)]
    #[test]
    fn the_wal_and_shm_an_older_run_left_readable_are_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("netweir-cp-sidecars-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A run from before checkpoints were private, still open: its
        // -wal and -shm are there, readable by anyone.
        let older = Connection::open(&path).unwrap();
        older.pragma_update(None, "journal_mode", "WAL").unwrap();
        older
            .execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
            .unwrap();
        let files = ["", "-wal", "-shm"].map(|end| dir.join(format!("crawl.sqlite3{end}")));
        for f in &files {
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let (cp, _) = Checkpoint::open(&path).unwrap();
        cp.jar("".into(), "cookies".into());
        cp.flush().unwrap();
        for f in &files {
            let mode = std::fs::metadata(f).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", f.display());
        }
        drop((cp, older));
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn a_checkpoint_from_before_cookie_jars_is_upgraded_and_keeps_them_after() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-v4-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The fourth format, with request policies but no jars table.
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES ('format', '4');
                 CREATE TABLE requests (row INTEGER PRIMARY KEY, url TEXT NOT NULL,
                     priority INTEGER NOT NULL, headers TEXT NOT NULL,
                     dont_filter INTEGER NOT NULL, depth INTEGER NOT NULL,
                     payload TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0,
                     method TEXT NOT NULL DEFAULT 'GET',
                     kind TEXT NOT NULL DEFAULT 'navigate', body BLOB,
                     content_type TEXT, referer TEXT,
                     retry_post INTEGER NOT NULL DEFAULT 0, proxy TEXT, policy TEXT);",
            )
            .unwrap();
        let (cp, saved) = Checkpoint::open(&path).unwrap();
        assert!(saved.jars.is_empty());
        cp.jar("".into(), "first".into());
        cp.jar("\"ada\"".into(), "ada's".into());
        cp.jar("".into(), "second".into());
        drop(cp);
        let (_cp, mut saved) = Checkpoint::open(&path).unwrap();
        saved.jars.sort();
        assert_eq!(
            saved.jars,
            [
                ("".to_string(), "second".to_string()),
                ("\"ada\"".to_string(), "ada's".to_string())
            ]
        );
        drop(_cp);
        let format: String = Connection::open(&path)
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key = 'format'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(format, FORMAT.to_string());
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
            &[Op::Request(
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
    fn a_failed_write_goes_again_with_the_next_commit() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-fail-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let (cp, _) = Checkpoint::open(&path).unwrap();
        cp.flush().unwrap();
        Connection::open(&path)
            .unwrap()
            .execute("DROP TABLE requests", [])
            .unwrap();
        // A page's items and the request its links led to, in a commit
        // that fails.
        cp.request([1; 16], pending(1, "https://e.com/a"));
        cp.settle(vec!["first".into()], r#"{"files":1}"#.into());
        let err = cp.commit().unwrap_err();
        assert!(err.contains("requests"), "{err}");
        assert!(
            cp.flush().unwrap_err().contains("requests"),
            "until one works"
        );
        // The table is back. The next batch's counters, which count the
        // first batch's items in the files' lengths, never land without
        // that batch.
        Connection::open(&path)
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
        cp.settle(vec!["second".into()], r#"{"files":2}"#.into());
        cp.commit().unwrap();
        cp.flush().unwrap();
        drop(cp);
        let (cp, saved) = Checkpoint::open(&path).unwrap();
        assert_eq!(saved.pending, vec![pending(1, "https://e.com/a")]);
        assert!(saved.items.contains("first") && saved.items.contains("second"));
        assert_eq!(saved.counters.as_deref(), Some(r#"{"files":2}"#));
        drop(cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failing_checkpoint_holds_one_counters_and_one_snapshot_per_jar() {
        let mut held = Held::default();
        held.push(Op::Counters("1".into()));
        held.push(Op::Jar("jar", "a".into(), "a1".into(), None));
        held.push(Op::Settle(vec!["x".into(), "y".into()], "2".into()));
        held.push(Op::Jar("jar", "a".into(), "a2".into(), None));
        held.push(Op::Jar("host", "a".into(), "h1".into(), None));
        held.push(Op::Counters("3".into()));
        let kept: Vec<String> = held
            .ops
            .iter()
            .flatten()
            .map(|op| match op {
                Op::Counters(c) => format!("counters {c}"),
                Op::Settle(ids, c) => format!("settle {} {c}", ids.len()),
                Op::Jar(kind, _, snapshot, _) => format!("{kind} {snapshot}"),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(kept, ["settle 2 2", "jar a2", "host h1", "counters 3"]);
        assert_eq!(held.rows, 6);
    }

    #[test]
    fn a_checkpoint_that_holds_back_too_much_stops_and_says_so() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-cap-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let (cp, _) = Checkpoint::open_holding(&path, 3).unwrap();
        cp.item("before".into());
        cp.commit().unwrap();
        assert_eq!(
            (cp.failing(), cp.stopped(), cp.failures()),
            (None, false, 0)
        );
        Connection::open(&path)
            .unwrap()
            .execute("DROP TABLE requests", [])
            .unwrap();
        cp.request([1; 16], pending(1, "https://e.com/a"));
        assert!(cp.commit().is_err());
        assert!(cp.failing().unwrap().contains("requests") && !cp.stopped());
        cp.item("one".into());
        cp.item("two".into());
        cp.item("three".into());
        let err = cp.commit().unwrap_err();
        assert!(cp.stopped() && cp.failures() == 2, "{err}");
        assert!(err.contains("4 checkpoint writes are held back"), "{err}");
        assert_eq!(cp.failing().as_ref(), Some(&err));
        // Stopped for good, the table back or not.
        Connection::open(&path)
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
        cp.item("after".into());
        assert_eq!(cp.commit().unwrap_err(), err);
        assert_eq!(cp.flush().unwrap_err(), err);
        drop(cp);
        let (cp, saved) = Checkpoint::open(&path).unwrap();
        let items: HashSet<&str> = saved.items.iter().map(String::as_str).collect();
        assert_eq!(
            items,
            HashSet::from(["before"]),
            "as at its last good commit"
        );
        drop(cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_fails_once_the_writer_has_stopped() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-stopped-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let (cp, _) = Checkpoint::open(&path).unwrap();
        cp.commit().unwrap();
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
    fn a_checkpoint_that_kept_passwords_is_upgraded_without_them() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-v6-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let (cp, _) = Checkpoint::open(&path).unwrap();
        cp.request([6; 16], pending(6, "https://e.com/a"));
        drop(cp);
        let conn = Connection::open(&path).unwrap();
        let login = r#"{"auth":{"user":"ann","password":"hunter2","domain":null}}"#;
        conn.execute("UPDATE requests SET policy = ?1", params![login])
            .unwrap();
        conn.execute("UPDATE meta SET value = '6' WHERE key = 'format'", [])
            .unwrap();
        drop(conn);
        let (cp, saved) = Checkpoint::open(&path).unwrap();
        let auth = saved.pending[0].policy.auth.as_ref().unwrap();
        assert_eq!(
            (auth.user.as_str(), auth.password.as_str(), auth.hidden),
            ("ann", "", true)
        );
        drop(cp);
        let conn = Connection::open(&path).unwrap();
        let (kept, format): (String, String) = conn
            .query_row(
                "SELECT policy, (SELECT value FROM meta WHERE key = 'format') FROM requests",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(!kept.contains("hunter2"), "{kept}");
        assert_eq!(format, FORMAT.to_string());
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_password_is_kept_as_it_is() {
        let policy = crate::Policy {
            auth: Some(crate::Auth::new("ann", String::new(), None)),
            ..Default::default()
        };
        assert_eq!(saved_policy(&policy), policy);
    }

    #[test]
    fn a_checkpoint_that_kept_proxy_logins_is_upgraded_without_them() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-v5-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The fifth format's requests table, as 0.5.0 wrote it.
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES ('format', '5');
                 CREATE TABLE requests (row INTEGER PRIMARY KEY, url TEXT NOT NULL,
                     priority INTEGER NOT NULL, headers TEXT NOT NULL,
                     dont_filter INTEGER NOT NULL, depth INTEGER NOT NULL,
                     payload TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0,
                     method TEXT NOT NULL DEFAULT 'GET',
                     kind TEXT NOT NULL DEFAULT 'navigate', body BLOB,
                     content_type TEXT, referer TEXT,
                     retry_post INTEGER NOT NULL DEFAULT 0, proxy TEXT, policy TEXT);
                 INSERT INTO requests (row, url, priority, headers, dont_filter, depth, payload,
                     proxy)
                     VALUES (4, 'https://e.com/a', 0, '[]', 0, 1, '{}',
                         'http://ann:secret@10.0.0.1:8080'),
                     (5, 'https://e.com/b', 0, '[]', 0, 1, '{}', 'http://10.0.0.2:8080');",
            )
            .unwrap();
        let (cp, saved) = Checkpoint::open(&path).unwrap();
        let proxies: Vec<_> = saved
            .pending
            .iter()
            .map(|p| (p.proxy.as_deref(), p.proxy_sha256.clone()))
            .collect();
        assert_eq!(
            proxies,
            [
                (
                    Some("http://***@10.0.0.1:8080"),
                    Some(proxy_sha256("http://ann:secret@10.0.0.1:8080"))
                ),
                (Some("http://10.0.0.2:8080"), None),
            ]
        );
        drop(cp);
        for file in std::fs::read_dir(&dir).unwrap() {
            let bytes = std::fs::read(file.unwrap().path()).unwrap();
            assert!(!bytes.windows(6).any(|w| w == b"secret"));
        }
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
