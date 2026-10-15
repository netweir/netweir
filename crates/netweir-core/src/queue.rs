//! A crawl's request queue and duplicate filter in Redis, shared by every
//! process of the crawl, on any number of machines. The design is in
//! docs/design/distributed.md.
//!
//! Requests wait in a sorted set by priority. A process leases some by
//! moving them, in one transaction, into a held set of its own, and
//! releases each once its caller has finished with it. A process that
//! stops sending heartbeats is taken for dead, and whichever process
//! notices first moves its held requests back, in one script. A request
//! is never in neither place, so a process dying at any point loses
//! nothing; delivery is at least once.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use redis::{Pipeline, RedisResult, Value};

use crate::canonical::Fingerprint;
use crate::checkpoint::Pending;

/// A pipeline's replies, one per command.
type Reply<'a> = Pin<Box<dyn Future<Output = RedisResult<Vec<Value>>> + Send + 'a>>;

/// Where the queue's commands go: a Redis server, or a stand-in in tests.
trait Store: Send + Sync {
    /// Runs the pipeline (as a transaction, if it's atomic).
    fn run<'a>(&'a self, pipe: &'a Pipeline) -> Reply<'a>;
}

struct Server {
    conn: redis::aio::ConnectionManager,
    /// The longest any one call may take, reconnecting included.
    wait: Duration,
}

impl Store for Server {
    fn run<'a>(&'a self, pipe: &'a Pipeline) -> Reply<'a> {
        // A clone shares the connection, and the reconnecting.
        let mut conn = self.conn.clone();
        let wait = self.wait;
        Box::pin(async move {
            // The client's own timeouts are per attempt: a call made while
            // it reconnects waits out several attempts and the backoff
            // between them. This bounds the whole call. A call cut short
            // may still be applied, as a lost reply is.
            match tokio::time::timeout(wait, pipe.query_async(&mut conn)).await {
                Ok(reply) => reply,
                Err(_) => Err(redis::RedisError::from((
                    redis::ErrorKind::Io,
                    "no answer in a third of a lease",
                ))),
            }
        })
    }
}

/// Marks each request seen, and queues those it was new to (or that skip
/// the filter), in one step: run again after a reply that was lost, it
/// finds them seen and queues nothing more. KEYS: seen, requests,
/// queued. ARGV: fingerprint, score, member, "1" to skip the filter, for
/// each request. Returns 1 for each this batch queued, now or on an
/// earlier run whose reply was lost (its member is still waiting), and 0
/// for a duplicate.
const PUSH: &str = "-- netweir push
local out, queued = {}, 0
for i = 1, #ARGV, 4 do
  local new = redis.call('SADD', KEYS[1], ARGV[i])
  if new == 1 or ARGV[i + 3] == '1' then
    queued = queued + redis.call('ZADD', KEYS[2], 'NX', ARGV[i + 1], ARGV[i + 2])
    out[#out + 1] = 1
  elseif redis.call('ZSCORE', KEYS[2], ARGV[i + 2]) then
    out[#out + 1] = 1
  else
    out[#out + 1] = 0
  end
end
if queued > 0 then redis.call('INCRBY', KEYS[3], queued) end
return out";

/// Puts what a process holds back in the queue and forgets the process,
/// if its lease has run out by `now` (always, without `now`). A member
/// already back in the queue keeps the better of its two places. KEYS:
/// processes, requests, the held set. ARGV: the process, now or "".
/// Returns how many went back, or -1 if the process wasn't due.
const REAP: &str = "-- netweir reap
if ARGV[2] ~= '' then
  local deadline = redis.call('ZSCORE', KEYS[1], ARGV[1])
  if not deadline or tonumber(deadline) > tonumber(ARGV[2]) then return -1 end
end
local held = redis.call('ZRANGE', KEYS[3], 0, -1, 'WITHSCORES')
for i = 1, #held, 2 do
  redis.call('ZADD', KEYS[2], 'LT', held[i + 1], held[i])
end
redis.call('DEL', KEYS[3])
redis.call('ZREM', KEYS[1], ARGV[1])
return #held / 2";

/// Puts back the given members of a held set. KEYS: the held set,
/// requests. ARGV: members. Returns how many were there.
const UNHOLD: &str = "-- netweir unhold
local n = 0
for i = 1, #ARGV do
  local score = redis.call('ZSCORE', KEYS[1], ARGV[i])
  if score then
    redis.call('ZADD', KEYS[2], 'LT', score, ARGV[i])
    redis.call('ZREM', KEYS[1], ARGV[i])
    n = n + 1
  end
end
return n";

/// How the queue stands, read in one step, so a lease or a reap between
/// two reads can't make it look empty. KEYS: requests, seen, queued,
/// processes. ARGV: the held sets' key prefix. Returns waiting, seen,
/// queued, processes, held.
const STATUS: &str = "-- netweir status
local held = 0
local processes = redis.call('ZRANGE', KEYS[4], 0, -1)
for _, id in ipairs(processes) do
  held = held + redis.call('ZCARD', ARGV[1] .. id)
end
return {redis.call('ZCARD', KEYS[1]), redis.call('SCARD', KEYS[2]),
  tonumber(redis.call('GET', KEYS[3]) or '0'), #processes, held}";

/// Requests sent to Redis in one go, at most.
pub const BATCH: usize = 1000;

/// Why a queue couldn't be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The URL, key or lease can't work, wherever the server is.
    Invalid(String),
    /// The server couldn't be reached, or won't do.
    Unreachable(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Invalid(m) | ConnectError::Unreachable(m) => f.write_str(m),
        }
    }
}

/// One process's handle on a crawl's queue.
pub struct Queue {
    store: Box<dyn Store>,
    key: String,
    /// This process, among the crawl's: its pid and a random part.
    id: String,
    lease: Duration,
    /// Redis's clock in ms when it was last read, and when that was here.
    clock: Mutex<Option<(i64, Instant)>>,
    /// The members this process knows it holds. A heartbeat gives back
    /// any other its held set has, such as those of a lease whose reply
    /// was lost.
    known: Mutex<HashSet<Vec<u8>>>,
    /// The server's address with any password hidden, for messages.
    shown: String,
}

/// A request made ready to queue: numbered once, so sending it again after
/// a lost reply can't make a second copy.
#[derive(Debug, Clone)]
pub struct Prepared {
    fp: Fingerprint,
    priority: i32,
    dont_filter: bool,
    /// None for one already seen, which isn't sent.
    member: Option<String>,
}

/// How a crawl's queue stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Requests waiting to be leased.
    pub waiting: u64,
    /// Requests leased and not yet released, by every process.
    pub held: u64,
    /// Processes with a heartbeat on record, including dead ones not yet
    /// found out.
    pub processes: u64,
    /// Fingerprints in the duplicate filter.
    pub seen: u64,
    /// Requests ever put in the queue under the key.
    pub queued: i64,
}

/// Refuses a key or lease that can't work.
fn check(key: &str, lease: Duration) -> Result<(), String> {
    if key.is_empty() {
        return Err("queue_key can't be empty".into());
    }
    if lease.is_zero() {
        return Err("queue_lease must be positive".into());
    }
    Ok(())
}

/// Checks a queue URL without connecting: a scheme and a form the client
/// takes. The message hides any password.
pub fn check_url(url: &str) -> Result<(), String> {
    let shown = shown(url);
    // Known limit: no TLS to Redis; the client is built without it.
    if url.starts_with("rediss://") {
        return Err(format!(
            "{shown}: TLS to Redis (rediss://) isn't supported; use redis:// on a \
             private network, or a TLS tunnel to the server"
        ));
    }
    // Known limit: no unix sockets where the client has none (Windows).
    if !cfg!(unix) && (url.starts_with("unix://") || url.starts_with("redis+unix://")) {
        return Err(format!(
            "{shown}: unix sockets aren't available on this platform; use redis://host:port"
        ));
    }
    redis::Client::open(url)
        .map(drop)
        .map_err(|e| format!("queue {shown}: {e}"))
}

/// `url` with any password hidden: in its user part, or in a `pass` or
/// `password` parameter, as unix-socket URLs give it.
pub fn shown(url: &str) -> String {
    let masked = crate::fetch::masked(url);
    let Some((head, query)) = masked.split_once('?') else {
        return masked;
    };
    let query: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _))
                if matches!(name.to_ascii_lowercase().as_str(), "pass" | "password") =>
            {
                format!("{name}=***")
            }
            _ => pair.to_string(),
        })
        .collect();
    format!("{head}?{}", query.join("&"))
}

impl Queue {
    /// Connects to the Redis at `url` for the crawl `key`. A lease lasts
    /// `lease` past the process's last heartbeat.
    pub async fn connect(url: &str, key: &str, lease: Duration) -> Result<Queue, ConnectError> {
        let shown = shown(url);
        check_url(url).map_err(ConnectError::Invalid)?;
        let client = redis::Client::open(url)
            .map_err(|e| ConnectError::Invalid(format!("queue {shown}: {e}")))?;
        check(key, lease).map_err(ConnectError::Invalid)?;
        // A reply that takes a third of a lease is as good as none: by the
        // time a few such have gone by, the other processes take this one
        // for dead.
        let wait = (lease / 3).max(Duration::from_millis(100));
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(Some(wait))
            .set_response_timeout(Some(wait))
            .set_number_of_retries(2);
        // Bounded as every call after it is: the client's own timeouts are
        // per attempt, and it makes three.
        let conn =
            match tokio::time::timeout(wait, client.get_connection_manager_with_config(config))
                .await
            {
                Ok(conn) => conn.map_err(|e| e.to_string()),
                Err(_) => Err("no answer in a third of a lease".to_string()),
            }
            .map_err(|e| {
                ConnectError::Unreachable(format!("can't reach the queue at {shown}: {e}"))
            })?;
        let server = Server { conn, wait };
        let mut queue =
            Queue::with_store(Box::new(server), key, lease).map_err(ConnectError::Invalid)?;
        queue.shown = shown;
        // A server too old for the commands the queue needs is found out
        // now, not after the crawl has leased something.
        let mut pipe = redis::pipe();
        pipe.cmd("SMISMEMBER").arg(queue.k("seen")).arg("-");
        queue.run(&pipe).await.map_err(|e| {
            ConnectError::Unreachable(format!(
                "{e} (the queue needs Redis 6.2 or later, or a server compatible with it)"
            ))
        })?;
        Ok(queue)
    }

    fn with_store(store: Box<dyn Store>, key: &str, lease: Duration) -> Result<Queue, String> {
        check(key, lease)?;
        Ok(Queue {
            store,
            key: key.to_string(),
            id: format!("{}-{:08x}", std::process::id(), random_u32()),
            lease,
            clock: Mutex::new(None),
            known: Mutex::new(HashSet::new()),
            shown: "the queue".into(),
        })
    }

    /// How long a lease lasts past the last heartbeat.
    pub fn lease_time(&self) -> Duration {
        self.lease
    }

    fn k(&self, part: &str) -> String {
        format!("{}:{part}", self.key)
    }

    fn held_key(&self, id: &str) -> String {
        format!("{}:held:{id}", self.key)
    }

    fn known(&self) -> std::sync::MutexGuard<'_, HashSet<Vec<u8>>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn run(&self, pipe: &Pipeline) -> Result<Vec<Value>, String> {
        self.store
            .run(pipe)
            .await
            .map_err(|e| format!("queue {}: {e}", self.shown))
    }

    /// Runs one of the scripts above.
    async fn script(
        &self,
        script: &str,
        keys: &[String],
        args: Vec<Vec<u8>>,
    ) -> Result<Value, String> {
        let mut pipe = redis::pipe();
        pipe.cmd("EVAL").arg(script).arg(keys.len());
        for key in keys {
            pipe.arg(key);
        }
        for arg in args {
            pipe.arg(arg);
        }
        Ok(self
            .run(&pipe)
            .await?
            .into_iter()
            .next()
            .unwrap_or(Value::Nil))
    }

    /// Redis's clock now, in ms: read afresh, or carried forward from the
    /// last reading.
    async fn now(&self, fresh: bool) -> Result<i64, String> {
        let known = *self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if let (false, Some((ms, at))) = (fresh, known) {
            return Ok(ms + at.elapsed().as_millis() as i64);
        }
        let mut pipe = redis::pipe();
        pipe.cmd("TIME");
        let reply = self.run(&pipe).await?;
        let parts = strings(reply.first());
        let (Some(secs), Some(micros)) = (
            parts.first().and_then(|s| s.parse::<i64>().ok()),
            parts.get(1).and_then(|s| s.parse::<i64>().ok()),
        ) else {
            return Err(format!("queue {}: TIME gave no time", self.shown));
        };
        let ms = secs * 1000 + micros / 1000;
        *self.clock.lock().unwrap_or_else(|e| e.into_inner()) = Some((ms, Instant::now()));
        Ok(ms)
    }

    /// Numbers requests for the queue and leaves out those already seen
    /// (unless they skip the filter). Send what it gives with `enqueue`,
    /// as many times as it takes.
    pub async fn prepare(
        &self,
        requests: &[(Fingerprint, &Pending)],
    ) -> Result<Vec<Prepared>, String> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let mut pipe = redis::pipe();
        pipe.cmd("INCRBY").arg(self.k("seq")).arg(requests.len());
        pipe.cmd("SMISMEMBER").arg(self.k("seen"));
        for (fp, _) in requests {
            pipe.arg(fp.as_slice());
        }
        let reply = self.run(&pipe).await?;
        let last = int(reply.first());
        let seen = ints(reply.get(1));
        // Numbered in the order given, so equal priorities keep it.
        let first = last - requests.len() as i64 + 1;
        requests
            .iter()
            .enumerate()
            .map(|(i, (fp, p))| {
                let skip = seen.get(i).copied().unwrap_or(0) == 1 && !p.dont_filter;
                let member = match skip {
                    true => None,
                    false => Some(format!(
                        "{:016x}{}",
                        first + i as i64,
                        serde_json::to_string(p).map_err(|e| e.to_string())?
                    )),
                };
                Ok(Prepared {
                    fp: *fp,
                    priority: p.priority,
                    dont_filter: p.dont_filter,
                    member,
                })
            })
            .collect()
    }

    /// Marks prepared requests seen and queues those new to the filter (or
    /// that skip it). For each, whether it was queued. Safe to send again
    /// after an error: what was applied stays applied once.
    ///
    /// Known limit: a `dont_filter` request leased (or leased and
    /// released) between a lost reply and the retry is queued again by the
    /// retry, and fetched twice if another process leases that copy before
    /// the holder's release takes it out.
    pub async fn enqueue(&self, prepared: &[Prepared]) -> Result<Vec<bool>, String> {
        let mut args = Vec::new();
        for p in prepared {
            if let Some(member) = &p.member {
                args.push(p.fp.to_vec());
                args.push((-f64::from(p.priority)).to_string().into_bytes());
                args.push(member.clone().into_bytes());
                args.push(if p.dont_filter {
                    b"1".to_vec()
                } else {
                    b"0".to_vec()
                });
            }
        }
        if args.is_empty() {
            return Ok(vec![false; prepared.len()]);
        }
        let keys = [self.k("seen"), self.k("requests"), self.k("queued")];
        let sent = ints(Some(&self.script(PUSH, &keys, args).await?));
        let mut sent = sent.into_iter();
        Ok(prepared
            .iter()
            .map(|p| p.member.is_some() && sent.next() == Some(1))
            .collect())
    }

    /// Queues requests, each with its fingerprint, and marks them seen, in
    /// batches of `BATCH`. For each, whether it was queued: false for one
    /// already seen, unless it has `dont_filter`.
    pub async fn push(&self, requests: &[(Fingerprint, Pending)]) -> Result<Vec<bool>, String> {
        let mut queued = Vec::new();
        for chunk in requests.chunks(BATCH) {
            let chunk: Vec<(Fingerprint, &Pending)> = chunk.iter().map(|(f, p)| (*f, p)).collect();
            let prepared = self.prepare(&chunk).await?;
            queued.extend(self.enqueue(&prepared).await?);
        }
        Ok(queued)
    }

    /// Takes up to `n` requests, highest priority first and in the order
    /// they were queued among equals, for this process. Each comes with
    /// the member to release it by, as its bytes; then how many members
    /// couldn't be read as requests, which are dropped.
    pub async fn lease(&self, n: usize) -> Result<(Vec<(Vec<u8>, Pending)>, u64), String> {
        if n == 0 {
            return Ok((Vec::new(), 0));
        }
        let deadline = self.now(false).await? + self.lease.as_millis() as i64;
        let taking = format!("{}:taking:{}", self.key, self.id);
        let held = self.held_key(&self.id);
        let mut pipe = redis::pipe();
        pipe.atomic();
        // Registered in the same transaction: no held set without a
        // heartbeat on record to run out.
        pipe.cmd("ZADD")
            .arg(self.k("processes"))
            .arg(deadline)
            .arg(&self.id);
        pipe.cmd("ZRANGESTORE")
            .arg(&taking)
            .arg(self.k("requests"))
            .arg(0)
            .arg(n - 1);
        pipe.cmd("ZREMRANGEBYRANK")
            .arg(self.k("requests"))
            .arg(0)
            .arg(n - 1);
        pipe.cmd("ZUNIONSTORE")
            .arg(&held)
            .arg(2)
            .arg(&held)
            .arg(&taking)
            .arg("AGGREGATE")
            .arg("MIN");
        pipe.cmd("ZRANGE").arg(&taking).arg(0).arg(-1);
        pipe.cmd("DEL").arg(&taking);
        let reply = self.run(&pipe).await?;
        let mut out = Vec::new();
        let mut unreadable = Vec::new();
        for member in blobs(reply.get(4)) {
            // Kept as the bytes Redis has: a member that isn't UTF-8 must
            // still be released by exactly those bytes.
            let request = member
                .get(16..)
                .and_then(|json| std::str::from_utf8(json).ok())
                .and_then(|json| serde_json::from_str::<Pending>(json).ok());
            match request {
                Some(p) => out.push((member, p)),
                // Not one of ours: nothing could fetch it, so it goes.
                None => unreadable.push(member),
            }
        }
        let bad = unreadable.len() as u64;
        if !unreadable.is_empty() {
            // If this fails, the caller never hears of the good ones: left
            // out of `known`, the next heartbeat gives them back, and the
            // bad ones too, to be dropped by whoever takes them next.
            self.release(&unreadable).await?;
        }
        self.known().extend(out.iter().map(|(m, _)| m.clone()));
        Ok((out, bad))
    }

    /// Releases leased requests: they're finished. One that was given
    /// back meanwhile, as when this process was taken for dead, is taken
    /// out of the queue again if no one has leased it yet.
    pub async fn release(&self, members: &[Vec<u8>]) -> Result<(), String> {
        if members.is_empty() {
            return Ok(());
        }
        let mut pipe = redis::pipe();
        pipe.cmd("ZREM").arg(self.held_key(&self.id));
        for m in members {
            pipe.arg(m);
        }
        pipe.cmd("ZREM").arg(self.k("requests"));
        for m in members {
            pipe.arg(m);
        }
        self.run(&pipe).await?;
        let mut known = self.known();
        for m in members {
            known.remove(m);
        }
        Ok(())
    }

    /// Says this process is alive for another lease; gives back anything
    /// its held set has that it doesn't know it holds (a lease whose reply
    /// was lost); and gives back the held requests of any process whose
    /// lease has run out. Returns how many requests it gave back.
    pub async fn heartbeat(&self) -> Result<u64, String> {
        let now = self.now(true).await?;
        let mut pipe = redis::pipe();
        pipe.cmd("ZADD")
            .arg(self.k("processes"))
            .arg(now + self.lease.as_millis() as i64)
            .arg(&self.id);
        pipe.cmd("ZRANGEBYSCORE")
            .arg(self.k("processes"))
            .arg("-inf")
            .arg(now);
        let reply = self.run(&pipe).await?;
        let mut back = self.reconcile().await?;
        for dead in strings(reply.get(1)) {
            back += self.reap(&dead, now).await?;
        }
        Ok(back)
    }

    /// Gives back what this process's held set has and it doesn't know of.
    /// Its own work is one step at a time, so nothing is half-known here.
    pub async fn reconcile(&self) -> Result<u64, String> {
        let held = self.held_key(&self.id);
        let mut pipe = redis::pipe();
        pipe.cmd("ZRANGE").arg(&held).arg(0).arg(-1);
        let members = blobs(self.run(&pipe).await?.first());
        let unknown: Vec<Vec<u8>> = {
            let known = self.known();
            members.into_iter().filter(|m| !known.contains(m)).collect()
        };
        if unknown.is_empty() {
            return Ok(0);
        }
        let keys = [held, self.k("requests")];
        Ok(int(Some(&self.script(UNHOLD, &keys, unknown).await?)).max(0) as u64)
    }

    /// Puts back what process `id` holds and forgets it, if its lease has
    /// run out by `now`; checked in the same script, so a process that
    /// beat since it was found late keeps what it has. How many went back.
    async fn reap(&self, id: &str, now: i64) -> Result<u64, String> {
        self.put_back(id, now.to_string()).await
    }

    async fn put_back(&self, id: &str, now: String) -> Result<u64, String> {
        let keys = [self.k("processes"), self.k("requests"), self.held_key(id)];
        let args = vec![id.as_bytes().to_vec(), now.into_bytes()];
        Ok(int(Some(&self.script(REAP, &keys, args).await?)).max(0) as u64)
    }

    /// Gives back everything this process holds, at once, and leaves the
    /// crawl: for a process that's stopping.
    pub async fn give_back(&self) -> Result<u64, String> {
        let back = self.put_back(&self.id.clone(), String::new()).await?;
        self.known().clear();
        Ok(back)
    }

    pub async fn status(&self) -> Result<Status, String> {
        let keys = [
            self.k("requests"),
            self.k("seen"),
            self.k("queued"),
            self.k("processes"),
        ];
        let prefix = self.held_key("").into_bytes();
        let reply = ints(Some(&self.script(STATUS, &keys, vec![prefix]).await?));
        let at = |i: usize| reply.get(i).copied().unwrap_or(0);
        Ok(Status {
            waiting: at(0) as u64,
            seen: at(1) as u64,
            queued: at(2),
            processes: at(3) as u64,
            held: at(4) as u64,
        })
    }

    /// Deletes every key of the crawl: its queue, its seen set, and what
    /// any process holds. Processes still running carry on with what
    /// they have.
    pub async fn clear(&self) -> Result<(), String> {
        let pattern = format!("{}:*", glob_escape(&self.key));
        let mut cursor = "0".to_string();
        loop {
            let mut pipe = redis::pipe();
            pipe.cmd("SCAN")
                .arg(&cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(1000);
            let reply = self.run(&pipe).await?;
            let Some(Value::Array(parts)) = reply.into_iter().next() else {
                return Err(format!("queue {}: SCAN gave no cursor", self.shown));
            };
            cursor = string(parts.first()).unwrap_or_else(|| "0".into());
            let keys = strings(parts.get(1));
            if !keys.is_empty() {
                let mut pipe = redis::pipe();
                pipe.cmd("DEL");
                for key in keys {
                    pipe.arg(key);
                }
                self.run(&pipe).await?;
            }
            if cursor == "0" {
                return Ok(());
            }
        }
    }
}

/// `text` with the characters a Redis glob reads escaped.
fn glob_escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn random_u32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.finish() as u32
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Int(n)) => *n,
        Some(Value::BulkString(b)) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        Some(Value::SimpleString(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn ints(value: Option<&Value>) -> Vec<i64> {
    match value {
        Some(Value::Array(items) | Value::Set(items)) => {
            items.iter().map(|v| int(Some(v))).collect()
        }
        _ => Vec::new(),
    }
}

fn string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
        Some(Value::SimpleString(s)) => Some(s.clone()),
        Some(Value::Int(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// The members of an array reply as the bytes they are.
fn blobs(value: Option<&Value>) -> Vec<Vec<u8>> {
    match value {
        Some(Value::Array(items) | Value::Set(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::BulkString(b) => Some(b.clone()),
                Value::SimpleString(s) => Some(s.clone().into_bytes()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items) | Value::Set(items)) => {
            items.iter().filter_map(|v| string(Some(v))).collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    /// Redis, as far as the queue uses it, in memory. Every pipeline runs
    /// under one lock, so each is atomic, as a transaction is.
    #[derive(Default)]
    pub(crate) struct Fake {
        db: Mutex<Db>,
    }

    /// A queue on `fake`, for the engine's tests too.
    pub(crate) fn fake_queue(fake: &Arc<Fake>, key: &str, lease: Duration) -> Queue {
        Queue::with_store(Box::new(fake.clone()), key, lease).unwrap()
    }

    #[derive(Default)]
    struct Db {
        /// Pipelines run so far: each a round trip.
        runs: usize,
        /// Members in order: by score, then by their bytes.
        zsets: HashMap<Vec<u8>, Vec<(f64, Vec<u8>)>>,
        sets: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
        strings: HashMap<Vec<u8>, Vec<u8>>,
        /// Added to the real clock, so leases run out without waiting.
        skew_ms: i64,
        /// SMISMEMBER says nothing is seen, as it would have just before
        /// another process marked it.
        stale_seen: bool,
        /// Pipelines left before every one fails, as when the process is
        /// cut off (or dies) part way.
        pipelines_left: Option<usize>,
        /// The next pipeline with this key among its arguments is applied,
        /// and then its reply is lost, as when the connection drops.
        lose_reply_touching: Option<Vec<u8>>,
    }

    impl Store for Arc<Fake> {
        fn run<'a>(&'a self, pipe: &'a Pipeline) -> Reply<'a> {
            let mut db = self.db.lock().unwrap();
            db.runs += 1;
            let gone = || redis::RedisError::from((redis::ErrorKind::Io, "cut off"));
            if let Some(left) = db.pipelines_left.as_mut() {
                if *left == 0 {
                    let gone = gone();
                    return Box::pin(async move { Err(gone) });
                }
                *left -= 1;
            }
            let commands: Vec<Vec<Vec<u8>>> = pipe
                .cmd_iter()
                .map(|cmd| {
                    cmd.args_iter()
                        .map(|a| match a {
                            redis::Arg::Simple(b) => b.to_vec(),
                            _ => panic!("the queue sends plain arguments"),
                        })
                        .collect()
                })
                .collect();
            let lose = db
                .lose_reply_touching
                .as_ref()
                .is_some_and(|key| commands.iter().flatten().any(|a| a == key));
            let replies: Vec<Value> = commands.iter().map(|args| db.exec(args)).collect();
            if lose {
                db.lose_reply_touching = None;
                let gone = gone();
                return Box::pin(async move { Err(gone) });
            }
            Box::pin(async move { Ok(replies) })
        }
    }

    fn text(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    fn num(b: &[u8]) -> f64 {
        match text(b).as_str() {
            "-inf" => f64::NEG_INFINITY,
            "+inf" | "inf" => f64::INFINITY,
            s => s.parse().unwrap(),
        }
    }

    /// Redis's reading of a start and stop rank: the indexes they cover.
    fn ranks(len: usize, start: &[u8], stop: &[u8]) -> std::ops::Range<usize> {
        let len = len as i64;
        let (mut a, mut b): (i64, i64) =
            (text(start).parse().unwrap(), text(stop).parse().unwrap());
        if a < 0 {
            a = (len + a).max(0);
        }
        if b < 0 {
            b += len;
        }
        b = b.min(len - 1);
        if a > b || a >= len {
            return 0..0;
        }
        a as usize..b as usize + 1
    }

    fn bulk(b: &[u8]) -> Value {
        Value::BulkString(b.to_vec())
    }

    impl Db {
        fn zset(&mut self, key: &[u8]) -> &mut Vec<(f64, Vec<u8>)> {
            self.zsets.entry(key.to_vec()).or_default()
        }

        fn zadd(&mut self, key: &[u8], score: f64, member: &[u8]) -> bool {
            let z = self.zset(key);
            let new = match z.iter().position(|(_, m)| m == member) {
                Some(i) => {
                    z.remove(i);
                    false
                }
                None => true,
            };
            let at = z.partition_point(|(s, m)| (*s, m.as_slice()) < (score, member));
            z.insert(at, (score, member.to_vec()));
            new
        }

        fn store(&mut self, key: &[u8], z: Vec<(f64, Vec<u8>)>) -> Value {
            let n = z.len();
            self.zsets.remove(key);
            for (s, m) in z {
                self.zadd(key, s, &m);
            }
            Value::Int(n as i64)
        }

        fn zscore(&mut self, key: &[u8], member: &[u8]) -> Option<f64> {
            self.zset(key)
                .iter()
                .find(|(_, m)| m == member)
                .map(|(s, _)| *s)
        }

        /// ZADD LT: adds a member, or moves it only to a lower score.
        fn zadd_lt(&mut self, key: &[u8], score: f64, member: &[u8]) {
            if self.zscore(key, member).is_none_or(|s| score < s) {
                self.zadd(key, score, member);
            }
        }

        /// The scripts, by their first line, done as their Lua does them.
        fn eval(&mut self, keys: &[Vec<u8>], argv: &[Vec<u8>], script: &str) -> Value {
            match script.lines().next().unwrap() {
                "-- netweir push" => {
                    let (mut out, mut queued) = (Vec::new(), 0);
                    for r in argv.chunks(4) {
                        let set = self.sets.entry(keys[0].clone()).or_default();
                        let new = set.insert(r[0].clone());
                        if new || r[3] == b"1" {
                            if self.zscore(&keys[1], &r[2]).is_none() {
                                self.zadd(&keys[1], num(&r[1]), &r[2]);
                                queued += 1;
                            }
                            out.push(Value::Int(1));
                        } else if self.zscore(&keys[1], &r[2]).is_some() {
                            out.push(Value::Int(1));
                        } else {
                            out.push(Value::Int(0));
                        }
                    }
                    if queued > 0 {
                        self.exec(&[
                            b"INCRBY".to_vec(),
                            keys[2].clone(),
                            queued.to_string().into_bytes(),
                        ]);
                    }
                    Value::Array(out)
                }
                "-- netweir reap" => {
                    if !argv[1].is_empty() {
                        let due = self
                            .zscore(&keys[0], &argv[0])
                            .is_some_and(|deadline| deadline <= num(&argv[1]));
                        if !due {
                            return Value::Int(-1);
                        }
                    }
                    let held = self.zsets.remove(&keys[2]).unwrap_or_default();
                    for (s, m) in &held {
                        self.zadd_lt(&keys[1], *s, m);
                    }
                    self.zset(&keys[0]).retain(|(_, m)| *m != argv[0]);
                    Value::Int(held.len() as i64)
                }
                "-- netweir unhold" => {
                    let mut n = 0;
                    for m in argv {
                        if let Some(s) = self.zscore(&keys[0], m) {
                            self.zadd_lt(&keys[1], s, m);
                            self.zset(&keys[0]).retain(|(_, x)| x != m);
                            n += 1;
                        }
                    }
                    Value::Int(n)
                }
                "-- netweir status" => {
                    let processes: Vec<Vec<u8>> =
                        self.zset(&keys[3]).iter().map(|(_, m)| m.clone()).collect();
                    let held: usize = processes
                        .iter()
                        .map(|id| self.zset(&[argv[0].as_slice(), id].concat()).len())
                        .sum();
                    let queued = self.strings.get(&keys[2]).map_or(0, |v| num(v) as i64);
                    Value::Array(vec![
                        Value::Int(self.zset(&keys[0]).len() as i64),
                        Value::Int(self.sets.get(&keys[1]).map_or(0, HashSet::len) as i64),
                        Value::Int(queued),
                        Value::Int(processes.len() as i64),
                        Value::Int(held as i64),
                    ])
                }
                other => panic!("the fake doesn't know the script {other}"),
            }
        }

        fn exec(&mut self, a: &[Vec<u8>]) -> Value {
            let name = text(&a[0]).to_uppercase();
            match name.as_str() {
                "EVAL" => {
                    let n: usize = text(&a[2]).parse().unwrap();
                    let script = text(&a[1]);
                    self.eval(&a[3..3 + n], &a[3 + n..], &script)
                }
                "TIME" => {
                    let ms = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64
                        + self.skew_ms;
                    Value::Array(vec![
                        bulk((ms / 1000).to_string().as_bytes()),
                        bulk(((ms % 1000) * 1000).to_string().as_bytes()),
                    ])
                }
                "ZADD" => {
                    let (mut nx, mut xx, mut i) = (false, false, 2);
                    loop {
                        match text(&a[i]).to_uppercase().as_str() {
                            "NX" => nx = true,
                            "XX" => xx = true,
                            _ => break,
                        }
                        i += 1;
                    }
                    let mut added = 0;
                    for pair in a[i..].chunks(2) {
                        let exists = self.zset(&a[1]).iter().any(|(_, m)| *m == pair[1]);
                        if (nx && exists) || (xx && !exists) {
                            continue;
                        }
                        added += i64::from(self.zadd(&a[1], num(&pair[0]), &pair[1]));
                    }
                    Value::Int(added)
                }
                "ZRANGE" => {
                    let z = self.zset(&a[1]).clone();
                    let r = ranks(z.len(), &a[2], &a[3]);
                    Value::Array(z[r].iter().map(|(_, m)| bulk(m)).collect())
                }
                "ZRANGESTORE" => {
                    let z = self.zset(&a[2]).clone();
                    let r = ranks(z.len(), &a[3], &a[4]);
                    self.store(&a[1], z[r].to_vec())
                }
                "ZREMRANGEBYRANK" => {
                    let z = self.zset(&a[1]);
                    let r = ranks(z.len(), &a[2], &a[3]);
                    let n = r.len();
                    z.drain(r);
                    Value::Int(n as i64)
                }
                "ZUNIONSTORE" => {
                    let n: usize = text(&a[2]).parse().unwrap();
                    assert_eq!(text(&a[3 + n..].concat()).to_uppercase(), "AGGREGATEMIN");
                    let mut union: HashMap<Vec<u8>, f64> = HashMap::new();
                    for key in &a[3..3 + n] {
                        for (s, m) in self.zset(key).clone() {
                            let e = union.entry(m).or_insert(s);
                            *e = e.min(s);
                        }
                    }
                    self.store(&a[1], union.into_iter().map(|(m, s)| (s, m)).collect())
                }
                "ZREM" => {
                    let z = self.zset(&a[1]);
                    let before = z.len();
                    z.retain(|(_, m)| !a[2..].contains(m));
                    Value::Int((before - z.len()) as i64)
                }
                "ZCARD" => Value::Int(self.zset(&a[1]).len() as i64),
                "ZRANGEBYSCORE" => {
                    let (lo, hi) = (num(&a[2]), num(&a[3]));
                    Value::Array(
                        self.zset(&a[1])
                            .iter()
                            .filter(|(s, _)| lo <= *s && *s <= hi)
                            .map(|(_, m)| bulk(m))
                            .collect(),
                    )
                }
                "DEL" => {
                    let mut n = 0;
                    for key in &a[1..] {
                        let z = self.zsets.remove(key).is_some_and(|z| !z.is_empty());
                        let s = self.sets.remove(key).is_some_and(|s| !s.is_empty());
                        let v = self.strings.remove(key).is_some();
                        n += i64::from(z || s || v);
                    }
                    Value::Int(n)
                }
                "SADD" => {
                    let set = self.sets.entry(a[1].clone()).or_default();
                    Value::Int(a[2..].iter().filter(|m| set.insert(m.to_vec())).count() as i64)
                }
                "SMISMEMBER" => {
                    let stale = self.stale_seen;
                    let set = self.sets.entry(a[1].clone()).or_default();
                    Value::Array(
                        a[2..]
                            .iter()
                            .map(|m| Value::Int(i64::from(set.contains(m) && !stale)))
                            .collect(),
                    )
                }
                "SCARD" => Value::Int(self.sets.get(&a[1]).map_or(0, HashSet::len) as i64),
                "INCRBY" => {
                    let now = self.strings.get(&a[1]).map_or(0, |v| num(v) as i64)
                        + text(&a[2]).parse::<i64>().unwrap();
                    self.strings
                        .insert(a[1].clone(), now.to_string().into_bytes());
                    Value::Int(now)
                }
                "GET" => self.strings.get(&a[1]).map_or(Value::Nil, |v| bulk(v)),
                "SCAN" => {
                    let prefix = text(&a[3]).replace('\\', "");
                    let prefix = prefix.strip_suffix('*').unwrap().as_bytes().to_vec();
                    let keys: HashSet<&Vec<u8>> = self
                        .zsets
                        .iter()
                        .filter(|(_, z)| !z.is_empty())
                        .map(|(k, _)| k)
                        .chain(
                            self.sets
                                .iter()
                                .filter(|(_, s)| !s.is_empty())
                                .map(|(k, _)| k),
                        )
                        .chain(self.strings.keys())
                        .filter(|k| k.starts_with(&prefix))
                        .collect();
                    Value::Array(vec![
                        bulk(b"0"),
                        Value::Array(keys.into_iter().map(|k| bulk(k)).collect()),
                    ])
                }
                other => panic!("the fake doesn't know {other}"),
            }
        }
    }

    /// Where a test's queues meet: the fake, or a real redis-server.
    enum Env {
        Fake(Arc<Fake>),
        Real {
            server: std::process::Child,
            url: String,
            _dir: TempDir,
        },
    }

    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            if let Env::Real { server, .. } = self {
                let _ = server.kill();
                let _ = server.wait();
            }
        }
    }

    const LEASE: Duration = Duration::from_millis(600);

    impl Env {
        fn fake() -> Env {
            Env::Fake(Arc::default())
        }

        /// A redis-server of its own, if there's one on the PATH.
        async fn real() -> Option<Env> {
            let path = std::env::var_os("PATH")?;
            let binary = std::env::split_paths(&path)
                .map(|d| d.join("redis-server"))
                .find(|p| p.is_file());
            let Some(binary) = binary else {
                assert!(
                    std::env::var_os("NETWEIR_REQUIRE_REDIS").is_none_or(|v| v.is_empty()),
                    "NETWEIR_REQUIRE_REDIS is set, and there's no redis-server on the PATH"
                );
                eprintln!("no redis-server on the PATH: skipped");
                return None;
            };
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let dir = std::env::temp_dir().join(format!("netweir-redis-{port}"));
            std::fs::create_dir_all(&dir).unwrap();
            let server = std::process::Command::new(binary)
                .args(["--port", &port.to_string(), "--bind", "127.0.0.1"])
                .args(["--save", "", "--appendonly", "no"])
                .arg("--dir")
                .arg(&dir)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let url = format!("redis://127.0.0.1:{port}/0");
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Some(Env::Real {
                server,
                url,
                _dir: TempDir(dir),
            })
        }

        async fn queue(&self) -> Queue {
            match self {
                Env::Fake(fake) => {
                    Queue::with_store(Box::new(fake.clone()), "books", LEASE).unwrap()
                }
                Env::Real { url, .. } => Queue::connect(url, "books", LEASE).await.unwrap(),
            }
        }

        /// Lets `d` go by, as far as leases are concerned.
        async fn pass(&self, d: Duration) {
            match self {
                Env::Fake(fake) => fake.db.lock().unwrap().skew_ms += d.as_millis() as i64,
                Env::Real { .. } => tokio::time::sleep(d).await,
            }
        }
    }

    /// Runs a test against the fake, and against a real redis-server when
    /// there is one.
    macro_rules! both {
        ($name:ident) => {
            mod $name {
                #[tokio::test]
                async fn fake() {
                    super::$name(super::Env::fake()).await
                }

                #[tokio::test]
                async fn redis() {
                    if let Some(env) = super::Env::real().await {
                        super::$name(env).await
                    }
                }
            }
        };
    }

    fn pending(url: &str, priority: i32) -> Pending {
        Pending {
            row: 0,
            url: url.into(),
            priority,
            headers: vec![("x-a".into(), "1".into())],
            dont_filter: false,
            depth: 2,
            payload: r#"{"callback": "item", "meta": {"n": 1}}"#.into(),
            method: "POST".into(),
            kind: "form".into(),
            body: Some(vec![0, 1, 254, 255]),
            content_type: Some("application/x-www-form-urlencoded".into()),
            referer: Some("https://e.com/".into()),
            retry_post: true,
            proxy: Some("http://10.0.1:8080".into()),
            policy: crate::Policy {
                timeout: Some(Duration::from_secs(5)),
                max_redirects: Some(0),
                dont_redirect: false,
                auth: Some(crate::Auth::new("ada", "secret", None)),
            },
        }
    }

    #[test]
    fn a_member_from_before_request_policies_reads_with_none() {
        let mut json = serde_json::to_value(pending("https://e.com/a", 0)).unwrap();
        json.as_object_mut().unwrap().remove("policy");
        let read: Pending = serde_json::from_value(json).unwrap();
        assert_eq!(read.policy, crate::Policy::default());
        let whole = pending("https://e.com/a", 0);
        let again: Pending = serde_json::from_str(&serde_json::to_string(&whole).unwrap()).unwrap();
        assert_eq!(again, whole);
    }

    fn fp(n: u8) -> Fingerprint {
        [n; 16]
    }

    fn urls(leased: &[(Vec<u8>, Pending)]) -> Vec<String> {
        leased.iter().map(|(_, p)| p.url.clone()).collect()
    }

    both!(requests_come_back_whole_by_priority_then_in_order);
    async fn requests_come_back_whole_by_priority_then_in_order(env: Env) {
        let q = env.queue().await;
        let batch = vec![
            (fp(1), pending("https://e.com/a", 0)),
            (fp(2), pending("https://e.com/b", 5)),
            (fp(3), pending("https://e.com/c", 0)),
        ];
        assert_eq!(q.push(&batch).await.unwrap(), vec![true; 3]);
        q.push(&[(fp(4), pending("https://e.com/d", -1))])
            .await
            .unwrap();
        q.push(&[(fp(5), pending("https://e.com/e", 0))])
            .await
            .unwrap();
        let leased = q.lease(10).await.unwrap().0;
        assert_eq!(
            urls(&leased),
            ["b", "a", "c", "e", "d"].map(|p| format!("https://e.com/{p}"))
        );
        // Every field survives the trip, the body's bytes included.
        assert_eq!(leased[1].1, pending("https://e.com/a", 0));
    }

    both!(a_request_seen_before_is_not_queued_again);
    async fn a_request_seen_before_is_not_queued_again(env: Env) {
        let (q1, q2) = (env.queue().await, env.queue().await);
        q1.push(&[(fp(1), pending("https://e.com/a", 0))])
            .await
            .unwrap();
        let mut again = pending("https://e.com/a", 0);
        assert_eq!(q2.push(&[(fp(1), again.clone())]).await.unwrap(), [false]);
        // Twice in one batch: the second is a duplicate.
        let twice = [
            (fp(2), pending("https://e.com/b", 0)),
            (fp(2), pending("https://e.com/b", 0)),
        ];
        assert_eq!(q2.push(&twice).await.unwrap(), [true, false]);
        again.dont_filter = true;
        assert_eq!(q2.push(&[(fp(1), again)]).await.unwrap(), [true]);
        let leased = q2.lease(10).await.unwrap().0;
        assert_eq!(
            urls(&leased),
            ["a", "b", "a"].map(|p| format!("https://e.com/{p}"))
        );
        let status = q1.status().await.unwrap();
        assert_eq!((status.seen, status.queued), (2, 3));
    }

    both!(two_processes_never_lease_the_same_request);
    async fn two_processes_never_lease_the_same_request(env: Env) {
        let (q1, q2) = (env.queue().await, env.queue().await);
        let batch: Vec<_> = (0..20)
            .map(|i| (fp(i), pending(&format!("https://e.com/{i}"), 0)))
            .collect();
        q1.push(&batch).await.unwrap();
        let (a, b) = tokio::join!(q1.lease(7), q2.lease(30));
        let (a, b) = (urls(&a.unwrap().0), urls(&b.unwrap().0));
        assert_eq!(a.len() + b.len(), 20);
        let all: HashSet<String> = a.iter().chain(&b).cloned().collect();
        assert_eq!(all.len(), 20, "none twice");
        let status = q1.status().await.unwrap();
        assert_eq!((status.waiting, status.held, status.processes), (0, 20, 2));
    }

    both!(a_released_request_is_gone_for_good);
    async fn a_released_request_is_gone_for_good(env: Env) {
        let q = env.queue().await;
        q.push(&[(fp(1), pending("https://e.com/a", 0))])
            .await
            .unwrap();
        let leased = q.lease(5).await.unwrap().0;
        assert_eq!(q.status().await.unwrap().held, 1);
        q.release(&[leased[0].0.clone()]).await.unwrap();
        let status = q.status().await.unwrap();
        assert_eq!((status.waiting, status.held), (0, 0));
        assert!(q.lease(5).await.unwrap().0.is_empty());
    }

    both!(a_dead_process_requests_go_back_once_its_lease_runs_out);
    async fn a_dead_process_requests_go_back_once_its_lease_runs_out(env: Env) {
        let (q1, q2) = (env.queue().await, env.queue().await);
        let batch: Vec<_> = (0..4)
            .map(|i| (fp(i), pending(&format!("https://e.com/{i}"), i32::from(i))))
            .collect();
        q1.push(&batch).await.unwrap();
        let taken = q1.lease(3).await.unwrap().0;
        assert_eq!(
            urls(&taken),
            ["3", "2", "1"].map(|p| format!("https://e.com/{p}"))
        );
        q2.heartbeat().await.unwrap();
        // Killed: no give back, no more heartbeats.
        let dead = q1.held_key(&q1.id);
        drop(q1);
        assert_eq!(q2.heartbeat().await.unwrap(), 0, "not dead yet");
        env.pass(LEASE + Duration::from_millis(200)).await;
        assert_eq!(q2.heartbeat().await.unwrap(), 3);
        assert_eq!(q2.heartbeat().await.unwrap(), 0, "and only once");
        let mut pipe = redis::pipe();
        pipe.cmd("ZCARD").arg(&dead);
        assert_eq!(
            q2.run(&pipe).await.unwrap(),
            [Value::Int(0)],
            "its set is gone"
        );
        // Back with their priorities, ahead of the one never taken.
        let leased = q2.lease(10).await.unwrap().0;
        assert_eq!(
            urls(&leased),
            ["3", "2", "1", "0"].map(|p| format!("https://e.com/{p}"))
        );
        assert_eq!(q2.status().await.unwrap().processes, 1);
    }

    both!(a_process_that_keeps_beating_keeps_its_requests);
    async fn a_process_that_keeps_beating_keeps_its_requests(env: Env) {
        let (q1, q2) = (env.queue().await, env.queue().await);
        q1.push(&[(fp(1), pending("https://e.com/a", 0))])
            .await
            .unwrap();
        q1.lease(5).await.unwrap();
        for _ in 0..3 {
            env.pass(LEASE / 2).await;
            q1.heartbeat().await.unwrap();
            assert_eq!(q2.heartbeat().await.unwrap(), 0);
        }
        assert_eq!(q2.status().await.unwrap().held, 1);
    }

    both!(a_process_that_stops_gives_back_at_once);
    async fn a_process_that_stops_gives_back_at_once(env: Env) {
        let (q1, q2) = (env.queue().await, env.queue().await);
        let batch: Vec<_> = (0..3)
            .map(|i| (fp(i), pending(&format!("https://e.com/{i}"), 0)))
            .collect();
        q1.push(&batch).await.unwrap();
        let taken = q1.lease(2).await.unwrap().0;
        q1.release(&[taken[0].0.clone()]).await.unwrap();
        assert_eq!(q1.give_back().await.unwrap(), 1);
        let status = q2.status().await.unwrap();
        assert_eq!((status.waiting, status.held, status.processes), (2, 0, 0));
        assert_eq!(
            urls(&q2.lease(5).await.unwrap().0),
            ["1", "2"].map(|p| format!("https://e.com/{p}"))
        );
    }

    both!(a_request_finished_after_it_was_given_back_is_not_fetched_again);
    async fn a_request_finished_after_it_was_given_back_is_not_fetched_again(env: Env) {
        let (q1, q2) = (env.queue().await, env.queue().await);
        q1.push(&[(fp(1), pending("https://e.com/a", 0))])
            .await
            .unwrap();
        let taken = q1.lease(1).await.unwrap().0;
        // q1 was only slow, and was taken for dead.
        env.pass(LEASE + Duration::from_millis(200)).await;
        assert_eq!(q2.heartbeat().await.unwrap(), 1);
        q1.release(&[taken[0].0.clone()]).await.unwrap();
        assert!(q2.lease(5).await.unwrap().0.is_empty());
    }

    both!(clearing_removes_every_key_of_the_crawl_and_no_other);
    async fn clearing_removes_every_key_of_the_crawl_and_no_other(env: Env) {
        let q = env.queue().await;
        let other = match &env {
            Env::Fake(fake) => Queue::with_store(Box::new(fake.clone()), "books2", LEASE).unwrap(),
            Env::Real { url, .. } => Queue::connect(url, "books2", LEASE).await.unwrap(),
        };
        let batch: Vec<_> = (0..3)
            .map(|i| (fp(i), pending(&format!("https://e.com/{i}"), 0)))
            .collect();
        q.push(&batch).await.unwrap();
        other.push(&batch).await.unwrap();
        q.lease(1).await.unwrap();
        q.clear().await.unwrap();
        assert_eq!(q.status().await.unwrap(), Status::default());
        assert_eq!(q.push(&batch).await.unwrap(), [true; 3], "nothing seen");
        assert_eq!(other.status().await.unwrap().waiting, 3);
    }

    #[tokio::test]
    async fn two_processes_queueing_one_new_url_at_once_queue_it_once() {
        let fake = Arc::new(Fake::default());
        let queue = || Queue::with_store(Box::new(fake.clone()), "books", LEASE).unwrap();
        let (q1, q2) = (queue(), queue());
        q1.push(&[(fp(1), pending("https://e.com/a", 0))])
            .await
            .unwrap();
        // q2 asked whether it was seen just before q1 marked it.
        fake.db.lock().unwrap().stale_seen = true;
        assert_eq!(
            q2.push(&[(fp(1), pending("https://e.com/a", 0))])
                .await
                .unwrap(),
            [false]
        );
        let status = q1.status().await.unwrap();
        assert_eq!((status.waiting, status.held), (1, 0));
    }

    #[tokio::test]
    async fn a_process_that_dies_while_queueing_loses_nothing() {
        let fake = Arc::new(Fake::default());
        let queue = || Queue::with_store(Box::new(fake.clone()), "books", LEASE).unwrap();
        let (q1, q2) = (queue(), queue());
        // Cut off after numbering the request, before queueing it: seen
        // and queued go in one script, so neither happened.
        fake.db.lock().unwrap().pipelines_left = Some(1);
        let batch = [(fp(1), pending("https://e.com/a", 0))];
        assert!(q1.push(&batch).await.is_err());
        drop(q1);
        fake.db.lock().unwrap().pipelines_left = None;
        assert_eq!(q2.push(&batch).await.unwrap(), [true], "not seen");
        let status = q2.status().await.unwrap();
        assert_eq!((status.waiting, status.held, status.seen), (1, 0, 1));
    }

    fn lose_next_reply(fake: &Fake) {
        fake.db.lock().unwrap().lose_reply_touching = Some(b"books:requests".to_vec());
    }

    #[tokio::test]
    async fn a_push_retried_after_its_reply_was_lost_leaves_nothing_stuck() {
        let fake = Arc::new(Fake::default());
        let q = Queue::with_store(Box::new(fake.clone()), "books", LEASE).unwrap();
        let batch = [(fp(1), pending("https://e.com/a", 0))];
        lose_next_reply(&fake);
        assert!(q.push(&batch).await.is_err());
        q.push(&batch).await.unwrap();
        let status = q.status().await.unwrap();
        assert_eq!((status.waiting, status.held, status.queued), (1, 0, 1));
    }

    #[tokio::test]
    async fn a_retried_push_without_the_filter_queues_one_copy() {
        let fake = Arc::new(Fake::default());
        let q = Queue::with_store(Box::new(fake.clone()), "books", LEASE).unwrap();
        let mut p = pending("https://e.com/a", 0);
        p.dont_filter = true;
        let prepared = q.prepare(&[(fp(1), &p)]).await.unwrap();
        lose_next_reply(&fake);
        assert!(q.enqueue(&prepared).await.is_err());
        assert_eq!(q.enqueue(&prepared).await.unwrap(), [true]);
        assert_eq!(q.status().await.unwrap().waiting, 1);
    }

    #[tokio::test]
    async fn a_lease_whose_reply_was_lost_is_given_back() {
        let fake = Arc::new(Fake::default());
        let (q1, q2) = (
            Queue::with_store(Box::new(fake.clone()), "books", LEASE).unwrap(),
            Queue::with_store(Box::new(fake.clone()), "books", LEASE).unwrap(),
        );
        q1.push(&[(fp(1), pending("https://e.com/a", 0))])
            .await
            .unwrap();
        let kept = q1.push(&[(fp(2), pending("https://e.com/b", 1))]).await;
        kept.unwrap();
        let (taken, _) = q1.lease(1).await.unwrap();
        lose_next_reply(&fake);
        assert!(q1.lease(5).await.is_err());
        assert_eq!(q1.status().await.unwrap().held, 2, "applied, unknown here");
        // The next heartbeat gives back what this process doesn't know it
        // holds, and keeps what it does.
        assert_eq!(q1.heartbeat().await.unwrap(), 1);
        let status = q1.status().await.unwrap();
        assert_eq!((status.waiting, status.held), (1, 1));
        assert_eq!(urls(&q2.lease(5).await.unwrap().0), ["https://e.com/a"]);
        q1.release(&[taken[0].0.clone()]).await.unwrap();
    }

    both!(a_process_that_beat_since_it_was_found_late_keeps_its_requests);
    async fn a_process_that_beat_since_it_was_found_late_keeps_its_requests(env: Env) {
        let (q1, q2) = (env.queue().await, env.queue().await);
        q1.push(&[(fp(1), pending("https://e.com/a", 0))])
            .await
            .unwrap();
        q1.lease(1).await.unwrap();
        // q2 read q1 as expired a lease ago; q1 has beaten since.
        let then = q2.now(true).await.unwrap() - LEASE.as_millis() as i64;
        q1.heartbeat().await.unwrap();
        assert_eq!(q2.reap(&q1.id, then).await.unwrap(), 0);
        assert_eq!(q2.status().await.unwrap().held, 1);
        // Found once its lease has really run out, it's reaped.
        let later = q2.now(true).await.unwrap() + 2 * LEASE.as_millis() as i64;
        assert_eq!(q2.reap(&q1.id, later).await.unwrap(), 1);
        assert_eq!(q2.status().await.unwrap().waiting, 1);
    }

    both!(a_held_request_this_process_doesnt_know_of_goes_back_at_its_heartbeat);
    async fn a_held_request_this_process_doesnt_know_of_goes_back_at_its_heartbeat(env: Env) {
        let q = env.queue().await;
        let batch: Vec<_> = (0..2)
            .map(|i| (fp(i), pending(&format!("https://e.com/{i}"), i32::from(i))))
            .collect();
        q.push(&batch).await.unwrap();
        let (taken, _) = q.lease(2).await.unwrap();
        // As if the reply of the lease of "1" had been lost.
        q.known().remove(&taken[0].0);
        assert_eq!(q.heartbeat().await.unwrap(), 1);
        let status = q.status().await.unwrap();
        assert_eq!((status.waiting, status.held), (1, 1));
        assert_eq!(q.heartbeat().await.unwrap(), 0, "the known one stays");
    }

    both!(a_prepared_batch_sent_twice_is_queued_once);
    async fn a_prepared_batch_sent_twice_is_queued_once(env: Env) {
        let q = env.queue().await;
        let mut again = pending("https://e.com/a", 0);
        again.dont_filter = true;
        let plain = pending("https://e.com/b", 0);
        let prepared = q
            .prepare(&[(fp(1), &again), (fp(2), &plain)])
            .await
            .unwrap();
        assert_eq!(q.enqueue(&prepared).await.unwrap(), [true, true]);
        // Sent again, as after a lost reply: still queued, by this batch,
        // not duplicates, and still one copy each.
        assert_eq!(q.enqueue(&prepared).await.unwrap(), [true, true]);
        let status = q.status().await.unwrap();
        assert_eq!((status.waiting, status.queued), (2, 2));
    }

    both!(a_member_that_isnt_utf8_is_dropped_by_its_own_bytes);
    async fn a_member_that_isnt_utf8_is_dropped_by_its_own_bytes(env: Env) {
        let q = env.queue().await;
        let mut pipe = redis::pipe();
        pipe.cmd("ZADD")
            .arg(q.k("requests"))
            .arg(0)
            .arg(b"0000000000000001{\"url\": \"\xff\xfe\"}".as_slice());
        q.run(&pipe).await.unwrap();
        q.push(&[(fp(1), pending("https://e.com/a", 1))])
            .await
            .unwrap();
        let (leased, unreadable) = q.lease(5).await.unwrap();
        assert_eq!(
            (urls(&leased), unreadable),
            (vec!["https://e.com/a".into()], 1)
        );
        let status = q.status().await.unwrap();
        assert_eq!((status.waiting, status.held), (0, 1), "the bad one is gone");
        assert_eq!(q.heartbeat().await.unwrap(), 0, "and isn't given back");
    }

    #[tokio::test]
    async fn status_is_read_in_one_round_trip() {
        let fake = Arc::new(Fake::default());
        let (q1, q2) = (
            fake_queue(&fake, "books", LEASE),
            fake_queue(&fake, "books", LEASE),
        );
        q1.push(&[(fp(1), pending("https://e.com/a", 0))])
            .await
            .unwrap();
        q1.lease(1).await.unwrap();
        q2.heartbeat().await.unwrap();
        let before = fake.db.lock().unwrap().runs;
        let status = q2.status().await.unwrap();
        assert_eq!(fake.db.lock().unwrap().runs, before + 1);
        assert_eq!(
            (
                status.waiting,
                status.held,
                status.processes,
                status.seen,
                status.queued
            ),
            (0, 1, 2, 1, 1)
        );
    }

    #[tokio::test]
    async fn a_lease_whose_release_of_a_bad_member_fails_leaves_nothing_stuck() {
        let fake = Arc::new(Fake::default());
        let q = fake_queue(&fake, "books", LEASE);
        q.push(&[(fp(1), pending("https://e.com/a", 0))])
            .await
            .unwrap();
        let mut pipe = redis::pipe();
        pipe.cmd("ZADD")
            .arg(q.k("requests"))
            .arg(1)
            .arg("not one of ours");
        q.run(&pipe).await.unwrap();
        // The bad member's release is applied, and its reply lost: the
        // lease fails, and its good request never reaches the caller.
        fake.db.lock().unwrap().lose_reply_touching = Some(b"not one of ours".to_vec());
        assert!(q.lease(5).await.is_err());
        // So the next heartbeat gives it back, as one this process doesn't
        // know it holds.
        assert_eq!(q.heartbeat().await.unwrap(), 1);
        let status = q.status().await.unwrap();
        assert_eq!((status.waiting, status.held), (1, 0));
    }

    /// A listener that never accepts, its backlog of one filled: a
    /// connection to it isn't even answered.
    #[cfg(not(windows))]
    async fn black_hole() -> (std::net::SocketAddr, Vec<tokio::task::JoinHandle<()>>) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let addr = listener.local_addr().unwrap();
        std::mem::forget(listener);
        let fillers = (0..4)
            .map(|_| {
                tokio::spawn(async move {
                    let _ = tokio::net::TcpStream::connect(addr).await;
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(200)).await;
        (addr, fillers)
    }

    // Windows answers a full backlog with a reset, so nothing hangs there
    // to be bounded.
    #[cfg(not(windows))]
    #[tokio::test(flavor = "multi_thread")]
    async fn connecting_to_a_server_that_never_answers_ends_within_a_third_of_a_lease() {
        let (addr, fillers) = black_hole().await;
        let started = Instant::now();
        let err = Queue::connect(
            &format!("redis://{addr}/0"),
            "books",
            Duration::from_millis(1500),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, ConnectError::Unreachable(_)), "{err}");
        // A third of a lease is 0.5 s; the client alone would wait out three
        // connection attempts of 0.5 s and the backoff between.
        assert!(
            started.elapsed() < Duration::from_millis(1000),
            "{:?}",
            started.elapsed()
        );
        for filler in fillers {
            filler.abort();
        }
    }

    /// Answers every command on one connection with an empty array, as
    /// far as a client needs to get going; `drop_it` closes that
    /// connection.
    #[cfg(not(windows))]
    async fn one_connection(
        listener: tokio::net::TcpListener,
        drop_it: tokio::sync::oneshot::Receiver<()>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let serve = async {
            let mut line = String::new();
            loop {
                line.clear();
                if read.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let n: usize = line.trim()[1..].parse().unwrap();
                for _ in 0..n {
                    line.clear();
                    read.read_line(&mut line).await.unwrap();
                    let len: usize = line.trim()[1..].parse().unwrap();
                    let mut arg = vec![0; len + 2];
                    read.read_exact(&mut arg).await.unwrap();
                }
                write.write_all(b"*0\r\n").await.unwrap();
            }
        };
        tokio::select! {
            _ = serve => {}
            _ = drop_it => {}
        }
        // The listener lives on, never accepting again: with its backlog
        // of one filled below, a new connection isn't even answered.
        std::mem::forget(listener);
    }

    // Windows answers a full backlog with a reset, so the reconnect fails
    // at once there and there's nothing to bound.
    #[cfg(not(windows))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_made_while_the_client_reconnects_ends_within_a_third_of_a_lease() {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let addr = listener.local_addr().unwrap();
        let (drop_it, dropped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(one_connection(listener, dropped));
        let lease = Duration::from_millis(1500);
        let q = Queue::connect(&format!("redis://{addr}/0"), "books", lease)
            .await
            .unwrap();
        drop_it.send(()).unwrap();
        server.await.unwrap();
        let mut fillers = Vec::new();
        for _ in 0..4 {
            fillers.push(tokio::spawn(tokio::net::TcpStream::connect(addr)));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        // The first call finds the connection gone, which starts the
        // client reconnecting; the next waits on that.
        assert!(q.status().await.is_err());
        let started = Instant::now();
        assert!(q.status().await.is_err());
        // A third of a lease is 0.5 s; the client alone would wait out
        // three connection attempts of 0.5 s and the backoff between.
        assert!(
            started.elapsed() < Duration::from_millis(1000),
            "{:?}",
            started.elapsed()
        );
        for filler in fillers {
            filler.abort();
        }
    }

    #[test]
    fn a_url_is_checked_without_connecting() {
        assert!(check_url("redis://h:6379/0").is_ok());
        for url in [
            "redis+unix:///run/redis.sock?db=1",
            "unix:///run/redis.sock",
        ] {
            if cfg!(unix) {
                assert!(check_url(url).is_ok(), "{url}");
            } else {
                // The client has no unix sockets here (Windows), and says
                // so before anything is tried.
                let err = check_url(url).unwrap_err();
                assert!(err.contains("unix sockets"), "{err}");
            }
        }
        for bad in ["redis://h:notaport/0", "http://h/", "rediss://h/0"] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
        let err = check_url("redis://:secret@h:notaport/0").unwrap_err();
        assert!(!err.contains("secret"), "{err}");
    }

    #[test]
    fn a_body_that_isnt_hex_is_an_error_not_a_panic() {
        let mut json = serde_json::to_value(pending("https://e.com/a", 0)).unwrap();
        // "aéb" is four bytes, and its second pair cuts "é" in half.
        for body in ["aéb", "ééé", "0g", "abc"] {
            json["body"] = body.into();
            assert!(
                serde_json::from_value::<Pending>(json.clone()).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn passwords_are_hidden_wherever_the_url_puts_them() {
        for url in [
            "redis://:secret@h:6379/0",
            "redis+unix:///run/redis.sock?db=0&pass=secret",
            "unix:///run/redis.sock?password=secret&db=1",
        ] {
            let shown = shown(url);
            assert!(!shown.contains("secret"), "{shown}");
        }
        assert_eq!(
            shown("unix:///run/redis.sock?password=secret&db=1"),
            "unix:///run/redis.sock?password=***&db=1"
        );
    }

    #[tokio::test]
    async fn tls_and_bad_settings_are_refused() {
        let err = Queue::connect("rediss://h:6380/0", "k", LEASE)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("TLS"), "{err}");
        let err = Queue::connect("http://h/", "k", LEASE).await.err().unwrap();
        assert!(err.to_string().contains("http://h/"), "{err}");
        let fake = Arc::new(Fake::default());
        assert!(Queue::with_store(Box::new(fake.clone()), "", LEASE).is_err());
        assert!(Queue::with_store(Box::new(fake), "k", Duration::ZERO).is_err());
    }

    #[tokio::test]
    async fn a_server_that_isnt_there_is_said_so() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = Queue::connect(&format!("redis://:secret@127.0.0.1:{port}/0"), "k", LEASE)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("can't reach the queue"), "{err}");
        assert!(!err.to_string().contains("secret"), "{err}");
    }

    #[test]
    fn glob_characters_in_a_key_are_escaped() {
        assert_eq!(glob_escape(r"a*b?[c]\"), r"a\*b\?\[c\]\\");
    }
}
