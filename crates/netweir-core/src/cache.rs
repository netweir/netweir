//! Responses kept on disk while a spider is written, so a rerun is served
//! from there instead of asking the site again. See docs/design/cache.md.
//!
//! SQLite in WAL mode with a busy timeout, so a second process opening the
//! same file waits its turn. Keyed by the request fingerprint the duplicate
//! filter uses. Which keys it holds is kept in memory too, so deciding a
//! hit never waits on the file. Nothing is evicted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::canonical::Fingerprint;
#[cfg(unix)]
use crate::checkpoint::owners_only;
use crate::checkpoint::retrying;
use crate::fetch::Response;

/// The layout this build writes, as the file's `user_version`. Older files
/// are upgraded on open; newer ones are refused.
pub const FORMAT: i64 = 1;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS responses (
        key BLOB PRIMARY KEY,
        url TEXT NOT NULL,
        status INTEGER NOT NULL,
        version TEXT NOT NULL,
        headers TEXT NOT NULL,
        body BLOB NOT NULL,
        stored_at REAL NOT NULL,
        redirects TEXT NOT NULL,
        robots_agent TEXT,
        obey_tdmrep INTEGER NOT NULL
    );
";

/// The rules a crawl fetches by. A page kept under one set is served only
/// to a crawl whose rules are no stricter.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// The robots.txt agent, if robots.txt is obeyed.
    pub robots: Option<String>,
    pub tdm: bool,
}

impl Policy {
    /// Whether a page fetched under `kept` may be served under these rules.
    fn allows(&self, kept: &Policy) -> bool {
        (self.robots.is_none() || self.robots == kept.robots) && (!self.tdm || kept.tdm)
    }
}

/// A redirect followed on the way to a kept response: what a live fetch
/// would have done to the cookie jar and the duplicate filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Redirect {
    /// The URL that answered with the redirect.
    pub url: String,
    /// Its Set-Cookie values.
    pub cookies: Vec<String>,
    /// The fingerprint of the request it led to, marked seen.
    pub seen: Option<Fingerprint>,
}

pub struct Cache {
    path: PathBuf,
    policy: Policy,
    /// None once closed.
    conn: Mutex<Option<Connection>>,
    /// Set by `close`; whoever holds the connection next closes it.
    closed: AtomicBool,
    /// Each key the file holds, when it was stored (seconds since 1970)
    /// and under which rules.
    keys: Mutex<HashMap<Fingerprint, (f64, Policy)>>,
    /// Entries older than this are stale.
    expiry: Option<Duration>,
}

impl Cache {
    pub fn open(path: &Path, expiry: Option<Duration>, policy: Policy) -> Result<Cache, String> {
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        let unusable = |e: rusqlite::Error| {
            format!(
                "the cache at {} can't be used: {e}; delete {} to start it afresh",
                path.display(),
                dir.display()
            )
        };
        // It holds the pages a crawl's logins reached.
        #[cfg(unix)]
        owners_only(path).map_err(unusable)?;
        // The format is read before anything is written, so a file this
        // build can't read is left as it was.
        let (conn, format) = retrying(|| {
            let conn = Connection::open(path)?;
            // Another process using the same cache: wait for it, don't fail.
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
        retrying(|| prepare(&conn)).map_err(unusable)?;
        let keys = read_keys(&conn).map_err(unusable)?;
        Ok(Cache {
            path: path.to_path_buf(),
            policy,
            conn: Mutex::new(Some(conn)),
            closed: AtomicBool::new(false),
            keys: Mutex::new(keys),
            expiry,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn keys(&self) -> std::sync::MutexGuard<'_, HashMap<Fingerprint, (f64, Policy)>> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `work` on the connection; None once the cache is closed.
    fn with<T>(&self, work: impl FnOnce(&Connection) -> T) -> Option<T> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let out = conn.as_ref().filter(|_| !self.closed()).map(work);
        #[cfg(test)]
        if tests::CLOSE_BEFORE_RELEASE.take() {
            self.close();
        }
        drop(conn);
        // `close` may have come while this held the connection, and left it
        // here.
        self.take_if_closed();
        out
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Lets go of the file. From then on everything is a miss and nothing
    /// is kept. It never waits: a write held up by another process (for as
    /// long as the busy timeout) has the connection, and closes it itself
    /// as soon as it's done, as `with` sees the flag this sets.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.take_if_closed();
        self.keys().clear();
    }

    /// Closes the connection if the cache is closed and nobody holds it.
    ///
    /// `close` sets the flag and then tries the lock; `with` releases the
    /// lock and then reads the flag. The fences make that a Dekker pair:
    /// of the two, the one that runs second sees what the first did, so
    /// either `close` finds the lock free or `with` finds the flag set,
    /// and the file is closed either way. A third holder that beat both to
    /// the lock goes through `with` too, and closes it on its way out.
    fn take_if_closed(&self) {
        fence(Ordering::SeqCst);
        if !self.closed() {
            return;
        }
        match self.conn.try_lock() {
            Ok(mut conn) => drop(conn.take()),
            Err(TryLockError::Poisoned(e)) => drop(e.into_inner().take()),
            Err(TryLockError::WouldBlock) => {}
        }
    }

    /// The oldest an entry may have been stored and still be fresh.
    fn fresh_since(&self) -> f64 {
        self.expiry.map_or(f64::MIN, |e| now() - e.as_secs_f64())
    }

    /// Whether a fresh response this crawl may use is kept for `key`, from
    /// memory: the file isn't touched.
    pub fn has(&self, key: &Fingerprint) -> bool {
        let since = self.fresh_since();
        self.keys()
            .get(key)
            .is_some_and(|(at, kept)| *at >= since && self.policy.allows(kept))
    }

    /// The fresh response kept for `key`, and the redirects that led to it,
    /// if there is still one this crawl may use. Data that can't be decoded
    /// is an error.
    pub fn get(&self, key: &Fingerprint) -> Result<Option<(Response, Vec<Redirect>)>, String> {
        let since = self.fresh_since();
        let row = self.with(|conn| {
            conn.query_row(
                "SELECT url, status, version, headers, body, redirects, robots_agent, obey_tdmrep
                 FROM responses WHERE key = ?1 AND stored_at >= ?2",
                params![key.as_slice(), since],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u16>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                        r.get::<_, String>(5)?,
                        Policy {
                            robots: r.get(6)?,
                            tdm: r.get(7)?,
                        },
                    ))
                },
            )
            .optional()
        });
        let Some((url, status, version, headers, body, redirects, kept)) =
            row.transpose().map_err(|e| e.to_string())?.flatten()
        else {
            return Ok(None);
        };
        if !self.policy.allows(&kept) {
            return Ok(None);
        }
        let undecodable = |e: serde_json::Error| format!("a stored response is damaged ({e})");
        let response = Response {
            url,
            status,
            version: version_of(&version),
            headers: serde_json::from_str(&headers).map_err(undecodable)?,
            body: body.into(),
            steps: Default::default(),
        };
        let redirects = serde_json::from_str(&redirects).map_err(undecodable)?;
        Ok(Some((response, redirects)))
    }

    /// Keeps `response`, and the redirects that led to it, under `key`,
    /// replacing what was there. False if the cache is closed.
    pub fn put(
        &self,
        key: &Fingerprint,
        response: &Response,
        redirects: &[Redirect],
    ) -> Result<bool, String> {
        let headers = serde_json::to_string(&response.headers).map_err(|e| e.to_string())?;
        let redirects = serde_json::to_string(redirects).map_err(|e| e.to_string())?;
        let at = now();
        let written = self.with(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO responses (key, url, status, version, headers, body,
                     stored_at, redirects, robots_agent, obey_tdmrep)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    key.as_slice(),
                    response.url,
                    response.status,
                    response.version,
                    headers,
                    &response.body[..],
                    at,
                    redirects,
                    self.policy.robots,
                    self.policy.tdm
                ],
            )
        });
        match written {
            None => Ok(false),
            Some(Err(e)) => Err(e.to_string()),
            Some(Ok(_)) => {
                self.keys().insert(*key, (at, self.policy.clone()));
                Ok(true)
            }
        }
    }
}

fn read_keys(conn: &Connection) -> rusqlite::Result<HashMap<Fingerprint, (f64, Policy)>> {
    let mut keys = HashMap::new();
    let mut stmt =
        conn.prepare("SELECT key, stored_at, robots_agent, obey_tdmrep FROM responses")?;
    let rows = stmt.query_map([], |r| {
        let policy = Policy {
            robots: r.get(2)?,
            tdm: r.get(3)?,
        };
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, f64>(1)?, policy))
    })?;
    for row in rows {
        let (key, at, policy) = row?;
        if let Ok(key) = Fingerprint::try_from(key.as_slice()) {
            keys.insert(key, (at, policy));
        }
    }
    Ok(keys)
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// The version string as `Response` holds it.
fn version_of(s: &str) -> &'static str {
    match s {
        "HTTP/0.9" => "HTTP/0.9",
        "HTTP/1.0" => "HTTP/1.0",
        "HTTP/2" => "HTTP/2",
        "HTTP/3" => "HTTP/3",
        _ => "HTTP/1.1",
    }
}

/// Puts the file in WAL mode, with the table in place and the format set.
fn prepare(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA)?;
    conn.pragma_update(None, "user_version", FORMAT)
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Has `with` call `close` just before it lets go of the connection:
        /// the moment when `close` can't take it, and leaves it to `with`.
        pub(super) static CLOSE_BEFORE_RELEASE: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    #[cfg(unix)]
    #[test]
    fn the_cache_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("netweir-cache-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let policy = Policy {
            robots: None,
            tdm: false,
        };
        let path = dir.join("cache.sqlite3");
        let mode = |f: &Path| std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
        drop(Cache::open(&path, None, policy.clone()).unwrap());
        assert_eq!(mode(&path), 0o600, "a new cache");
        // One from before, left readable by anyone with its -wal and -shm,
        // by a run still holding it open.
        let older = Connection::open(&path).unwrap();
        older.pragma_update(None, "journal_mode", "WAL").unwrap();
        older
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x)")
            .unwrap();
        let files = ["", "-wal", "-shm"].map(|end| dir.join(format!("cache.sqlite3{end}")));
        for f in &files {
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let cache = Cache::open(&path, None, policy).unwrap();
        for f in &files {
            assert_eq!(mode(f), 0o600, "{}", f.display());
        }
        drop((cache, older));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_close_that_comes_while_the_connection_is_in_use_still_closes_it() {
        let dir = std::env::temp_dir().join(format!("netweir-cache-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let policy = Policy {
            robots: None,
            tdm: false,
        };
        let cache = Cache::open(&dir.join("cache.sqlite3"), None, policy).unwrap();
        CLOSE_BEFORE_RELEASE.set(true);
        assert!(matches!(cache.get(&[0; 16]), Ok(None)));
        assert!(
            cache.conn.lock().unwrap().is_none(),
            "the file is still open"
        );
        drop(cache);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
