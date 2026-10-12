//! The crawl engine: a queue per host, robots.txt and TDMRep checks before
//! a site's first request, per-host concurrency and adaptive delays, and
//! deduplication. The caller submits requests and takes events in batches.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use netweir_browser::{Browser, Cookie, LaunchOptions, WaitUntil};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use url::Url;

use crate::cache::{Cache, Policy, Redirect};
use crate::canonical::{Fingerprint, fingerprint, fingerprint_with};
use crate::checkpoint::{Checkpoint, Pending, Saved};
use crate::classify::{BlockKind, Outcome, classify};
use crate::fetch::{
    FetchError, FetchErrorKind, FetchOptions, Fetcher, Hop, Outgoing, Response, check_proxy, masked,
};
use crate::profile::Profile;
use crate::robots::Robots;
use crate::tdmrep::{self, Reservation, TdmFile};
use crate::traps;

#[derive(Debug, Clone)]
pub struct CrawlSettings {
    /// Requests in flight across all sites.
    pub concurrency: usize,
    /// Requests in flight to one host.
    pub per_domain: usize,
    pub obey_robots: bool,
    /// The product token matched against robots.txt user-agent lines.
    pub robots_agent: String,
    pub obey_tdmrep: bool,
    /// Adjust each host's delay to its response times (AutoThrottle).
    pub throttle: bool,
    /// Each host's delay before its first response is timed.
    pub start_delay: Duration,
    pub min_delay: Duration,
    pub max_delay: Duration,
    /// Requests the throttle aims to have in flight to one host.
    pub target_concurrency: f64,
    /// Links followed from a start page beyond which requests are refused.
    pub max_depth: Option<u32>,
    /// Requests accepted for one host beyond which more are refused.
    pub max_pages_per_domain: Option<u64>,
    /// Requests sent, each retry among them, beyond which none is started
    /// and the crawl ends once those under way finish. What was still
    /// queued stays in the checkpoint for a later run.
    pub max_pages: Option<u64>,
    /// Further tries after a server error, a network error, throttling or
    /// a block.
    pub retries: u32,
    /// Retry n waits a random time up to backoff_base * 2^n, capped at
    /// backoff_max ("full jitter").
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// The crawl's proxies, used as `proxy_rotation` says. Empty: the
    /// fetch options' proxy, with a fresh cookie jar for each new session.
    pub proxies: Vec<String>,
    pub proxy_rotation: ProxyRotation,
    /// Failures in a row (`FetchError::proxy_at_fault`, a 407 or a block)
    /// after which a proxy is benched.
    pub proxy_failures: u32,
    /// How long a proxy is benched the first time; twice that each time
    /// after, up to `MAX_COOLDOWN`, until it gets an answer through.
    pub proxy_cooldown: Duration,
    /// The circuit breaker looks at a host's last `breaker_window`
    /// responses; if more than `breaker_ratio` of them were blocks, the
    /// host pauses for `breaker_pause`.
    pub breaker_window: usize,
    pub breaker_ratio: f64,
    pub breaker_pause: Duration,
    /// A file to keep the crawl's state in, so it resumes after a crash.
    pub checkpoint: Option<std::path::PathBuf>,
    /// Which requests go through Chrome rather than the HTTP client.
    pub browser: BrowserMode,
    /// Chrome pages open at once.
    pub browser_pages: usize,
    /// How Chrome is started, the first time a request needs it.
    pub browser_launch: LaunchOptions,
    /// Domains requests may go to, with their subdomains, normalised by
    /// `allowed_domains`; empty for any. A request elsewhere is dropped
    /// unless it has `dont_filter`.
    pub allowed_domains: Vec<String>,
    /// A file to keep responses in, so a rerun is served from it rather
    /// than the site.
    pub cache: Option<std::path::PathBuf>,
    /// How long a kept response stays fresh; for good if None.
    pub cache_expiry: Option<Duration>,
}

/// How a crawl uses its `proxies`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyRotation {
    /// The fetch options' proxy until a host blocks a session; then the
    /// host's new session takes the next proxy.
    #[default]
    OnBlock,
    /// Every request takes the next proxy, with the crawl's cookies.
    PerRequest,
    /// Each host keeps a proxy, and its cookies with it, until a block or
    /// the proxy being benched moves it to the next.
    PerHost,
}

/// The longest a proxy is benched for, however often it has failed.
pub const MAX_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// Proxies outside `proxies` that requests named, each counted on its own
/// in `proxy_stats`; any more are counted together under `OTHER_PROXIES`.
pub const MAX_OTHER_PROXIES: usize = 1000;

/// The `proxy_stats` row for proxies beyond `MAX_OTHER_PROXIES`.
pub const OTHER_PROXIES: &str = "other";

/// Clients through proxies requests named, kept beyond the pool's own:
/// each holds a connection pool, so a gateway named with a new login per
/// request mustn't keep one per request for the whole crawl.
const VIA_EXTRA: usize = 256;

/// Clients a host keeps through proxies its requests named.
const HOST_VIA: usize = 16;

/// What `Pool::pick` found.
#[derive(Debug, PartialEq, Eq)]
enum Picked {
    Healthy(String),
    /// Back from the bench soonest, and whether to warn the pool is
    /// exhausted.
    Soonest(String, bool),
    None,
}

/// A map that keeps its `cap` most recently used entries.
struct Lru<T> {
    map: HashMap<String, (T, u64)>,
    clock: u64,
    cap: usize,
}

impl<T: Clone> Lru<T> {
    fn new(cap: usize) -> Lru<T> {
        Lru {
            map: HashMap::new(),
            clock: 0,
            cap: cap.max(1),
        }
    }

    fn get(&mut self, key: &str) -> Option<T> {
        self.clock += 1;
        let clock = self.clock;
        self.map.get_mut(key).map(|(value, used)| {
            *used = clock;
            value.clone()
        })
    }

    fn insert(&mut self, key: &str, value: T) {
        if self.map.len() >= self.cap && !self.map.contains_key(key) {
            // Known limit: a scan to find the oldest, only when full; a linked
            // list would make it O(1) if caps ever grow large.
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
            }
        }
        self.clock += 1;
        self.map.insert(key.to_string(), (value, self.clock));
    }

    fn clear(&mut self) {
        self.map.clear();
    }
}

impl Default for Lru<Fetcher> {
    fn default() -> Self {
        Lru::new(VIA_EXTRA)
    }
}

/// Which requests a crawl fetches in Chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrowserMode {
    /// Only those that ask for it.
    #[default]
    Off,
    /// Those that ask, and any still blocked after its retries, once.
    OnBlock,
    /// Every request.
    Always,
}

impl Default for CrawlSettings {
    fn default() -> Self {
        CrawlSettings {
            concurrency: 64,
            per_domain: 8,
            obey_robots: true,
            robots_agent: "netweir".to_string(),
            obey_tdmrep: true,
            throttle: true,
            start_delay: Duration::from_secs(1),
            min_delay: Duration::ZERO,
            max_delay: Duration::from_secs(60),
            target_concurrency: 1.0,
            max_depth: None,
            max_pages_per_domain: None,
            max_pages: None,
            retries: 3,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            proxies: Vec::new(),
            proxy_rotation: ProxyRotation::OnBlock,
            proxy_failures: 3,
            proxy_cooldown: Duration::from_secs(60),
            breaker_window: 50,
            breaker_ratio: 0.3,
            breaker_pause: Duration::from_secs(300),
            checkpoint: None,
            browser: BrowserMode::Off,
            browser_pages: 4,
            browser_launch: LaunchOptions::default(),
            allowed_domains: Vec::new(),
            cache: None,
            cache_expiry: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CrawlRequest {
    /// The caller's handle for this request; events carry it back.
    pub id: u64,
    pub url: String,
    /// Higher goes first among a host's waiting requests.
    pub priority: i32,
    pub headers: Vec<(String, String)>,
    /// Fetch even if an equal request was seen before.
    pub dont_filter: bool,
    /// Links followed to get here from a start page (which is 0).
    pub depth: u32,
    /// Fetch it in Chrome.
    pub browser: bool,
    /// The method, body and kind of request, and the page it comes from;
    /// typing the address by default.
    pub outgoing: Outgoing,
    /// Retry a POST or PATCH after a server error too. Off by default:
    /// the server may have acted on it, and a retry would act twice.
    pub retry_post: bool,
    /// Send it through this proxy, whatever the rotation. Over HTTP only:
    /// Chrome has one proxy for every page.
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    Queued,
    /// An equal request was already submitted.
    Duplicate,
    /// Not an http(s) URL.
    Invalid,
    /// Deeper than `max_depth`.
    TooDeep,
    /// The path repeats itself, as crawler traps do.
    Trap,
    /// The host already has `max_pages_per_domain` requests.
    DomainFull,
    /// Outside `allowed_domains`.
    Offsite,
    /// Its proxy isn't one the client can use.
    BadProxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// robots.txt disallows it.
    Robots,
    /// The site reserves text and data mining rights for it (TDMRep).
    TdmReserved,
}

#[derive(Debug, Clone)]
pub enum Event {
    Fetched {
        id: u64,
        response: Response,
        /// For a request fetched in Chrome, the page, still open.
        page: Option<LivePage>,
    },
    Failed {
        id: u64,
        error: FetchError,
    },
    Dropped {
        id: u64,
        reason: DropReason,
    },
    /// Still blocked after every retry and a new session for each.
    Blocked {
        id: u64,
        vendor: String,
        kind: BlockKind,
        response: Response,
    },
    /// The circuit breaker paused a host that kept blocking.
    Paused {
        host: String,
        pause: Duration,
    },
    /// Something the caller should hear about, once.
    Warning {
        message: String,
    },
}

/// A page still open in Chrome after a browser fetch. Closing it, or
/// dropping the last clone, closes the page and frees its place among
/// `browser_pages`.
#[derive(Clone)]
pub struct LivePage {
    inner: Arc<Live>,
}

struct Live {
    page: netweir_browser::Page,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    runtime: tokio::runtime::Handle,
    /// Told when the page's slot frees, so a browser request waiting for
    /// one can start.
    shared: std::sync::Weak<Shared>,
}

impl Live {
    fn free(&self) {
        let freed = self
            .permit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some();
        if freed && let Some(shared) = self.shared.upgrade() {
            shared.schedule.notify_one();
        }
    }
}

impl LivePage {
    pub fn page(&self) -> &netweir_browser::Page {
        &self.inner.page
    }

    pub async fn close(&self) {
        let _ = self.inner.page.close().await;
        self.inner.free();
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.free();
        if !self.page.is_closed() {
            let page = self.page.clone();
            self.runtime.spawn(async move {
                let _ = page.close().await;
            });
        }
    }
}

impl std::fmt::Debug for LivePage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LivePage({})", self.inner.page.url())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Waiting in a host's queue.
    pub queued: usize,
    pub in_flight: usize,
    pub fetched: u64,
    pub failed: u64,
    pub duplicates: u64,
    pub dropped_robots: u64,
    pub dropped_tdm: u64,
    pub bytes: u64,
    pub skipped_depth: u64,
    pub skipped_traps: u64,
    pub skipped_domain_full: u64,
    pub retries: u64,
    /// Responses that were blocks, retried or not.
    pub blocked: u64,
    /// 429s, and 503s with Retry-After.
    pub throttled: u64,
    pub sessions_replaced: u64,
    pub breaker_trips: u64,
    /// Pages fetched in Chrome.
    pub browser_fetches: u64,
    /// Requests blocked over HTTP that Chrome got through.
    pub browser_unblocked: u64,
    /// Requests dropped for going outside `allowed_domains`.
    pub offsite: u64,
    /// Pages served from the cache rather than fetched.
    pub cache_hits: u64,
    /// Responses written to the cache.
    pub cache_stores: u64,
    /// Times a proxy was benched.
    pub proxies_benched: u64,
}

/// What one proxy did in a crawl.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyStats {
    /// Requests sent through it.
    pub requests: u64,
    /// Those that failed it (`FetchError::proxy_at_fault`, a 407 or a
    /// block).
    pub failures: u64,
    /// Times it was benched.
    pub benched: u64,
}

/// A crawl in progress. Dropping it stops the scheduler; fetches already
/// under way finish on their own.
pub struct Crawler {
    shared: Arc<Shared>,
}

struct Shared {
    fetcher: Fetcher,
    checkpoint: Option<Checkpoint>,
    cache: Option<Arc<Cache>>,
    /// What the checkpoint held at the start, until the caller takes it.
    saved: Mutex<Option<Saved>>,
    /// What new sessions are made from.
    options: FetchOptions,
    settings: CrawlSettings,
    state: Mutex<State>,
    /// Woken when there may be something to schedule.
    schedule: Notify,
    /// Woken when an event arrives or the crawl may have finished.
    events: Notify,
    /// Chrome, started the first time a request needs it.
    /// A Chrome that dies is started again for the next request that
    /// needs one. One that can't start is tried again a minute on, not for
    /// every request meanwhile.
    browser: tokio::sync::Mutex<Option<Result<Browser, (String, Instant)>>>,
    /// Places for open Chrome pages.
    pages: Arc<Semaphore>,
    runtime: tokio::runtime::Handle,
}

#[derive(Default)]
struct State {
    seq: u64,
    /// Each saved request's row in the checkpoint, by id.
    rows: HashMap<u64, i64>,
    next_row: i64,
    seen: HashSet<Fingerprint>,
    /// Hosts outside allowed_domains that requests were dropped for, so
    /// each is warned about once.
    offsite_hosts: HashSet<String>,
    hosts: HashMap<String, Host>,
    origins: HashMap<String, Origin>,
    events: VecDeque<Event>,
    /// Hosts that may be able to start a request. The scheduler visits only
    /// these, so its work follows what changed, not how many hosts exist.
    ready: VecDeque<String>,
    /// Hosts waiting out their delay, soonest first.
    timers: BinaryHeap<Reverse<(Instant, String)>>,
    /// robots.txt and tdmrep.json fetches under way.
    gate_fetches: usize,
    /// Requests sent so far, retries among them, for `max_pages`.
    sent: u64,
    /// Requests the cache has a response for, served ahead of any host.
    hits: BinaryHeap<Queued>,
    /// Whether a hit is being read from the cache.
    serving: bool,
    stats: Stats,
    closed: bool,
    /// Whether the crawl was told no HTTP profile matches its Chrome.
    warned_profile: bool,
    /// Whether the crawl was told a blocked request couldn't go to Chrome.
    warned_browser: bool,
    /// Whether the crawl was told its cache failed.
    warned_cache: bool,
    /// Hosts whose next request waits for a free Chrome page.
    page_waiters: Vec<String>,
    proxies: Pool,
    /// The crawl's cookies through each proxy, made the first time a
    /// request goes through it without a session of its host's own. Room
    /// for every proxy of the pool and `VIA_EXTRA` that requests named.
    via: Lru<Fetcher>,
    /// The proxy Chrome was started with, which the cookies it earns are
    /// tied to.
    chrome_proxy: Option<String>,
    /// Whether the crawl was told no proxy of the pool suits Chrome.
    warned_chrome_proxy: bool,
}

/// The crawl's proxies and how each is doing, in `proxies` order; then
/// any other proxy a request named, which is counted but never benched.
#[derive(Default)]
struct Pool {
    entries: Vec<PoolEntry>,
    index: HashMap<String, usize>,
    /// The first `pooled` entries are `settings.proxies`.
    pooled: usize,
    /// The entry the round robin looks at next.
    turn: usize,
    /// Whether the crawl was told every proxy is benched, since one was
    /// last healthy.
    warned_exhausted: bool,
}

struct PoolEntry {
    url: String,
    stats: ProxyStats,
    /// Failures since its last success.
    run: u32,
    /// Times benched since its last success, which doubles the cooldown.
    level: u32,
    /// Benched until then.
    until: Option<Instant>,
}

impl Pool {
    fn new(proxies: &[String]) -> Pool {
        let mut pool = Pool::default();
        for url in proxies {
            pool.push(url);
        }
        pool.pooled = pool.entries.len();
        pool
    }

    /// The entry for `url`, made if it's new. Proxies outside the pool
    /// beyond `MAX_OTHER_PROXIES` share one entry, `OTHER_PROXIES`: a
    /// rotating gateway with a session in each login would otherwise add
    /// one for every request.
    fn entry(&mut self, url: &str) -> usize {
        if let Some(&k) = self.index.get(url) {
            return k;
        }
        if self.entries.len() - self.pooled >= MAX_OTHER_PROXIES {
            return self.push(OTHER_PROXIES);
        }
        self.push(url)
    }

    fn push(&mut self, url: &str) -> usize {
        if let Some(&k) = self.index.get(url) {
            return k;
        }
        self.entries.push(PoolEntry {
            url: url.to_string(),
            stats: ProxyStats::default(),
            run: 0,
            level: 0,
            until: None,
        });
        self.index.insert(url.to_string(), self.entries.len() - 1);
        self.entries.len() - 1
    }

    fn pooled(&self, url: &str) -> bool {
        self.index.get(url).is_some_and(|&k| k < self.pooled)
    }

    /// Whether `url` is in the pool and benched now.
    fn benched(&self, url: &str, now: Instant) -> bool {
        self.index
            .get(url)
            .is_some_and(|&k| k < self.pooled && self.entries[k].benched(now))
    }

    fn any_healthy(&self, now: Instant) -> bool {
        self.entries[..self.pooled].iter().any(|e| !e.benched(now))
    }

    /// The first of the pool in turn that `usable` takes and isn't
    /// benched, and true; or, with every one it takes benched, the one
    /// back soonest, and false. None if it takes none.
    fn choose(&self, now: Instant, usable: impl Fn(&str) -> bool) -> Option<(usize, bool)> {
        let n = self.pooled;
        let usable_at = |k: usize| usable(&self.entries[k].url);
        let healthy = (0..n)
            .map(|i| (self.turn + i) % n)
            .find(|&k| usable_at(k) && !self.entries[k].benched(now));
        if let Some(k) = healthy {
            return Some((k, true));
        }
        (0..n)
            .filter(|&k| usable_at(k))
            .min_by_key(|&k| self.entries[k].until)
            .map(|k| (k, false))
    }

    /// The next healthy proxy in turn that `usable` takes, another than
    /// `avoid` if there is one; or, with every one it takes benched, the
    /// one back soonest. The second says to warn that the pool is
    /// exhausted: only when every proxy is benched, not only every one
    /// `usable` takes, and once until one is healthy again.
    fn pick(&mut self, now: Instant, avoid: Option<&str>, usable: impl Fn(&str) -> bool) -> Picked {
        let other = self
            .choose(now, |p| usable(p) && Some(p) != avoid)
            .filter(|(_, healthy)| *healthy);
        let Some((k, healthy)) = other.or_else(|| self.choose(now, &usable)) else {
            return Picked::None;
        };
        let url = self.entries[k].url.clone();
        if healthy {
            self.turn = k + 1;
            self.warned_exhausted = false;
            return Picked::Healthy(url);
        }
        let warn = !self.any_healthy(now) && !self.warned_exhausted;
        self.warned_exhausted |= warn;
        Picked::Soonest(url, warn)
    }

    /// The pool's proxy back from the bench soonest.
    fn soonest(&self) -> Option<&str> {
        self.entries[..self.pooled]
            .iter()
            .min_by_key(|e| e.until)
            .map(|e| e.url.as_str())
    }
}

impl PoolEntry {
    fn benched(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| until > now)
    }
}

struct Host {
    queue: BinaryHeap<Queued>,
    in_flight: usize,
    next_at: Instant,
    delay: Duration,
    /// The robots.txt Crawl-delay, below which the delay never goes.
    floor: Duration,
    /// In `State::ready`.
    listed: bool,
    /// When this host's timer is set for, so it is set once.
    timer: Option<Instant>,
    /// Requests submitted for this host, for `max_pages_per_domain`.
    accepted: u64,
    /// This host's own session after a block, or from its first request
    /// with per_host rotation; until then, the crawl's.
    session: Option<Fetcher>,
    /// The proxy of `proxies` the session goes through, if it's one.
    proxy: Option<String>,
    /// What the session was made with (Chrome's profile, after a hand
    /// back), for the clients through proxies its requests name.
    options: Option<FetchOptions>,
    /// The session through proxies requests named, made as they're
    /// needed; the last `HOST_VIA` of them.
    via: Lru<Fetcher>,
    /// Whether each of the last responses was a block, newest last.
    recent: VecDeque<bool>,
}

#[derive(Default)]
struct Origin {
    robots: Gate<Robots>,
    tdm: Gate<Option<TdmFile>>,
    /// Hosts waiting for this origin's robots.txt or TDMRep file.
    waiting: Vec<String>,
    /// Proxies of the pool that failed to fetch this origin's robots.txt
    /// or tdmrep.json, so the next try goes through another.
    failed_proxies: Vec<String>,
    /// The proxy that next try goes through, when the host has no session
    /// of its own to move.
    retry_through: Option<String>,
}

#[derive(Default)]
enum Gate<T> {
    #[default]
    Unknown,
    Fetching,
    Ready(T),
}

struct Queued {
    priority: i32,
    seq: u64,
    request: CrawlRequest,
    host: String,
    origin: String,
    path: String,
    /// Redirects followed so far to get to `request.url`.
    hops: usize,
    /// Tries already made.
    attempts: u32,
    /// Fetch it in Chrome.
    in_browser: bool,
    /// What the cache needs to keep its response. None if it isn't cached.
    keep: Option<Keep>,
    /// Blocked over HTTP and sent to Chrome: what the block was, for
    /// on_block if Chrome doesn't get through either.
    blocked: Option<Box<(String, BlockKind, Response)>>,
}

/// A cached request's way through to its response.
struct Keep {
    /// The fingerprint of the request as submitted, before any redirect,
    /// which the cache keeps the response under.
    key: Fingerprint,
    /// The redirects followed so far.
    redirects: Vec<Redirect>,
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Queued {
    /// Higher priority first, then first submitted first.
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then(other.seq.cmp(&self.seq))
    }
}

impl Crawler {
    /// Starts the scheduler on the current tokio runtime.
    pub fn new(fetch: FetchOptions, settings: CrawlSettings) -> Result<Crawler, FetchError> {
        check_proxies(&fetch, &settings)?;
        let mut state = State {
            proxies: Pool::new(&settings.proxies),
            via: Lru::new(settings.proxies.len() + VIA_EXTRA),
            ..State::default()
        };
        let (checkpoint, saved) = match &settings.checkpoint {
            Some(path) => {
                let (cp, mut saved) =
                    Checkpoint::open(path).map_err(|e| FetchError::invalid(e.to_string()))?;
                state.seen = std::mem::take(&mut saved.seen);
                state.next_row = saved.next_row;
                (Some(cp), Some(saved))
            }
            None => (None, None),
        };
        let cache = match &settings.cache {
            Some(path) => Some(Arc::new(
                Cache::open(
                    path,
                    settings.cache_expiry,
                    Policy {
                        robots: settings.obey_robots.then(|| settings.robots_agent.clone()),
                        tdm: settings.obey_tdmrep,
                    },
                )
                .map_err(FetchError::invalid)?,
            )),
            None => None,
        };
        let settings_pages = settings.browser_pages.max(1);
        let shared = Arc::new(Shared {
            fetcher: Fetcher::new(fetch.clone())?,
            checkpoint,
            cache,
            saved: Mutex::new(saved),
            options: fetch,
            settings,
            state: Mutex::new(state),
            schedule: Notify::new(),
            events: Notify::new(),
            browser: tokio::sync::Mutex::new(None),
            pages: Arc::new(Semaphore::new(settings_pages)),
            runtime: tokio::runtime::Handle::current(),
        });
        tokio::spawn(schedule_loop(shared.clone()));
        Ok(Crawler { shared })
    }

    pub fn submit(&self, request: CrawlRequest) -> Submitted {
        self.submit_with(request, "{}")
    }

    /// `submit`, saving `payload` (the caller's own part of the request, as
    /// JSON) with it in the checkpoint, if there is one.
    pub fn submit_with(&self, request: CrawlRequest, payload: &str) -> Submitted {
        self.admit(request, Persist::New(payload))
    }

    /// Queues a request read back from the checkpoint (`saved().pending`).
    /// It was accepted before, so it isn't checked again.
    pub fn resubmit(&self, request: CrawlRequest, row: i64) -> Submitted {
        self.admit(request, Persist::Restored(row))
    }

    /// What the checkpoint held when the crawl started; once.
    pub fn saved(&self) -> Option<Saved> {
        self.shared
            .saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The caller has finished with these requests' events: they are done
    /// for good, and a resumed crawl won't fetch them again.
    pub fn done(&self, ids: &[u64]) {
        let Some(cp) = &self.shared.checkpoint else {
            return;
        };
        let mut state = self.shared.lock();
        for id in ids {
            if let Some(row) = state.rows.remove(id) {
                cp.done(row);
            }
        }
    }

    /// Records an item as delivered, by its stable id.
    pub fn item_done(&self, id: String) {
        if let Some(cp) = &self.shared.checkpoint {
            cp.item(id);
        }
    }

    /// Records items as delivered and saves the caller's counters (JSON)
    /// in one commit.
    pub fn settle(&self, items: Vec<String>, counters: String) {
        if let Some(cp) = &self.shared.checkpoint {
            cp.settle(items, counters);
        }
    }

    /// Saves the caller's counters (JSON) alongside the crawl.
    pub fn save_counters(&self, json: String) {
        if let Some(cp) = &self.shared.checkpoint {
            cp.counters(json);
        }
    }

    /// Waits until every checkpoint write so far is on disk; an error if
    /// one of them failed.
    pub fn flush(&self) -> Result<(), String> {
        match &self.shared.checkpoint {
            Some(cp) => cp.flush(),
            None => Ok(()),
        }
    }

    /// Commits the checkpoint and closes its file, for when the crawl is
    /// over. Checkpoint writes after this are dropped.
    pub fn close(&self) -> Result<(), String> {
        let flushed = self.flush();
        if let Some(cp) = &self.shared.checkpoint {
            cp.close();
        }
        if let Some(cache) = &self.shared.cache {
            cache.close();
        }
        flushed
    }

    /// The Chrome the crawl is using, if it has started one.
    pub async fn browser(&self) -> Option<Browser> {
        match &*self.shared.browser.lock().await {
            Some(Ok(browser)) => Some(browser.clone()),
            _ => None,
        }
    }

    /// Closes Chrome, if the crawl started it.
    pub async fn close_browser(&self) {
        if let Some(browser) = self.browser().await {
            let _ = browser.close().await;
        }
    }

    fn admit(&self, request: CrawlRequest, persist: Persist<'_>) -> Submitted {
        let o = &request.outgoing;
        let Some(fp) = fingerprint_with(&o.method, &request.url, o.body.as_deref()) else {
            return Submitted::Invalid;
        };
        let Ok(parsed) = Url::parse(&request.url) else {
            return Submitted::Invalid;
        };
        if request
            .proxy
            .as_deref()
            // An empty proxy sends the request direct.
            .is_some_and(|p| !p.is_empty() && check_proxy(p).is_err())
        {
            return Submitted::BadProxy;
        }
        let settings = &self.shared.settings;
        let mut state = self.shared.lock();
        if !request.dont_filter && offsite(&parsed, &settings.allowed_domains) {
            state.stats.offsite += 1;
            let host = parsed.host_str().unwrap_or_default().to_string();
            if state.offsite_hosts.insert(host.clone()) {
                // Once per site, as there may be thousands of links to it.
                self.shared.push_event(
                    &mut state,
                    Event::Warning {
                        message: format!(
                            "requests to {host} are dropped: it isn't in allowed_domains"
                        ),
                    },
                );
            }
            return Submitted::Offsite;
        }
        if settings.max_depth.is_some_and(|max| request.depth > max) {
            state.stats.skipped_depth += 1;
            return Submitted::TooDeep;
        }
        if traps::repeats(parsed.path()) {
            state.stats.skipped_traps += 1;
            return Submitted::Trap;
        }
        let restored = matches!(persist, Persist::Restored(_));
        if !state.seen.insert(fp) && !request.dont_filter && !restored {
            state.stats.duplicates += 1;
            return Submitted::Duplicate;
        }
        let host = parsed.host_str().unwrap_or_default();
        let accepted = state.hosts.get(host).map_or(0, |h| h.accepted);
        if settings
            .max_pages_per_domain
            .is_some_and(|max| accepted >= max)
        {
            // Not seen after all: the same URL is refused the same way.
            state.seen.remove(&fp);
            state.stats.skipped_domain_full += 1;
            return Submitted::DomainFull;
        }
        // Chrome hands the callback a live page, which no kept response
        // can stand in for.
        let cache = self
            .shared
            .cache
            .as_ref()
            .filter(|_| !wants_browser(settings, &request));
        let hit = cache.is_some_and(|c| c.has(&fp));
        let keep = cache.map(|_| Keep {
            key: fp,
            redirects: Vec::new(),
        });
        let url = request.url.clone();
        let saving = self.shared.checkpoint.as_ref().map(|cp| {
            let row = match persist {
                Persist::Restored(row) => row,
                Persist::New(payload) => {
                    let row = state.next_row;
                    state.next_row += 1;
                    cp.request(
                        fp,
                        Pending {
                            row,
                            url: request.url.clone(),
                            priority: request.priority,
                            headers: request.headers.clone(),
                            dont_filter: request.dont_filter,
                            depth: request.depth,
                            payload: payload.to_string(),
                            method: request.outgoing.method.clone(),
                            kind: request.outgoing.kind.as_str().to_string(),
                            body: request.outgoing.body.clone(),
                            content_type: request.outgoing.content_type.clone(),
                            referer: request.outgoing.referer.clone(),
                            retry_post: request.retry_post,
                            proxy: request.proxy.clone(),
                        },
                    );
                    row
                }
            };
            (request.id, row)
        });
        if !enqueue(settings, &mut state, request, &url, 0, keep, hit) {
            return Submitted::Invalid;
        }
        if let Some((id, row)) = saving {
            state.rows.insert(id, row);
        }
        if let Some(h) = parsed.host_str().and_then(|h| state.hosts.get_mut(h)) {
            h.accepted += 1;
        }
        drop(state);
        self.shared.schedule.notify_one();
        Submitted::Queued
    }

    /// Waits for at least one event and returns up to `max`. Returns an
    /// empty batch once nothing is queued, in flight or being checked: the
    /// crawl is finished unless more is submitted.
    pub async fn next(&self, max: usize) -> Vec<Event> {
        next_events(self.shared.clone(), max).await
    }

    /// `next`, as a future that owns what it needs, for callers that can't
    /// hold a borrow across the await (such as Python bindings).
    pub fn next_owned(
        &self,
        max: usize,
    ) -> impl std::future::Future<Output = Vec<Event>> + Send + 'static {
        next_events(self.shared.clone(), max)
    }

    pub fn stats(&self) -> Stats {
        self.shared.lock().stats.clone()
    }

    /// Each of `proxies`, then each other proxy a request named, with what
    /// it did.
    pub fn proxy_stats(&self) -> Vec<(String, ProxyStats)> {
        self.shared
            .lock()
            .proxies
            .entries
            .iter()
            .map(|e| (e.url.clone(), e.stats.clone()))
            .collect()
    }
}

/// Refuses proxy settings that can't work, or that leave it unclear which
/// proxy a request goes through.
fn check_proxies(fetch: &FetchOptions, settings: &CrawlSettings) -> Result<(), FetchError> {
    let mode = match settings.proxy_rotation {
        ProxyRotation::OnBlock => None,
        ProxyRotation::PerRequest => Some("per_request"),
        ProxyRotation::PerHost => Some("per_host"),
    };
    if let Some(mode) = mode {
        if settings.proxies.is_empty() {
            return Err(FetchError::invalid(format!(
                "proxy_rotation \"{mode}\" rotates through proxies, and none were given"
            )));
        }
        if fetch.proxy.is_some() {
            return Err(FetchError::invalid(format!(
                "proxy_rotation \"{mode}\" takes proxies, not proxy as well: it isn't clear \
                 which would win; put that proxy in proxies"
            )));
        }
    }
    if settings.proxy_failures == 0 {
        return Err(FetchError::invalid("proxy_failures must be at least 1"));
    }
    if settings.proxy_cooldown.is_zero() {
        return Err(FetchError::invalid("proxy_cooldown must be positive"));
    }
    for proxy in &settings.proxies {
        check_proxy(proxy)?;
    }
    Ok(())
}

async fn next_events(shared: Arc<Shared>, max: usize) -> Vec<Event> {
    loop {
        let wake = shared.events.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        {
            let mut state = shared.lock();
            if !state.events.is_empty() {
                let n = max.max(1).min(state.events.len());
                let batch = state.events.drain(..n).collect();
                drop(state);
                // Room in the backlog may let the scheduler start more.
                shared.schedule.notify_one();
                return batch;
            }
            let idle = state.stats.in_flight == 0 && state.gate_fetches == 0 && !state.serving;
            if idle && (state.stats.queued == 0 || state.capped(&shared.settings)) {
                return Vec::new();
            }
        }
        wake.await;
    }
}

impl Drop for Crawler {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.schedule.notify_one();
        // The scheduler keeps the shared state alive a while longer; the
        // checkpoint file shouldn't stay open with it.
        if let Some(cp) = &self.shared.checkpoint {
            cp.close();
        }
        if let Some(cache) = &self.shared.cache {
            cache.close();
        }
        let shared = self.shared.clone();
        self.shared.runtime.spawn(async move {
            if let Some(Ok(browser)) = &*shared.browser.lock().await {
                let _ = browser.close().await;
            }
        });
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push_event(&self, state: &mut State, event: Event) {
        match &event {
            Event::Fetched { response, .. } => {
                state.stats.fetched += 1;
                state.stats.bytes += response.body.len() as u64;
            }
            Event::Failed { .. } => state.stats.failed += 1,
            Event::Dropped {
                reason: DropReason::Robots,
                ..
            } => state.stats.dropped_robots += 1,
            Event::Dropped {
                reason: DropReason::TdmReserved,
                ..
            } => state.stats.dropped_tdm += 1,
            Event::Blocked { .. } | Event::Paused { .. } | Event::Warning { .. } => {}
        }
        state.events.push_back(event);
        self.events.notify_waiters();
    }
}

/// `domains`, made ready for `offsite`: lowercase, international names in
/// their ASCII form, an optional port kept. A URL rather than a domain is an
/// error, since it would match nothing.
pub fn allowed_domains(domains: &[String]) -> Result<Vec<String>, String> {
    domains
        .iter()
        .map(|d| {
            let d = d.trim();
            let parsed = (!d.is_empty() && !d.contains(['/', '?', '#', '@']))
                .then(|| Url::parse(&format!("http://{d}/")).ok())
                .flatten()
                .filter(|u| u.host_str().is_some());
            let Some(u) = parsed else {
                return Err(format!(
                    "allowed_domains takes domains such as example.com, not {d:?}"
                ));
            };
            let host = u
                .host_str()
                .unwrap_or_default()
                .trim_end_matches('.')
                .to_string();
            Ok(match u.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .collect()
}

/// Whether `url` is outside `allowed` (from `allowed_domains`): not one of
/// them or a subdomain of one, or not the port a domain names. Nothing is
/// outside an empty list.
pub fn offsite(url: &Url, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return false;
    }
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    let port = url.port_or_known_default();
    !allowed.iter().any(|d| {
        let (domain, want) = match d.rsplit_once(':').filter(|(_, p)| p.parse::<u16>().is_ok()) {
            Some((h, p)) => (h, p.parse::<u16>().ok()),
            None => (d.as_str(), None),
        };
        let within = host == domain
            || (host.len() > domain.len()
                && host.ends_with(domain)
                && host.as_bytes()[host.len() - domain.len() - 1] == b'.');
        within && want.is_none_or(|w| port == Some(w))
    })
}

/// Whether a submitted request is new to the checkpoint or read back from it.
enum Persist<'a> {
    New(&'a str),
    Restored(i64),
}

/// The session a request goes out with, and the proxy of the pool or
/// the request's own it goes through (None for the fetch options').
type Route = (Result<Fetcher, FetchError>, Option<String>);

/// What the scheduler decided to do with a host's next request.
enum Action {
    Fetch(Box<Queued>, Route),
    /// In Chrome, in the page slot the permit holds.
    Render(Box<Queued>, OwnedSemaphorePermit),
    CheckRobots(String, Route),
    CheckTdm(String, Route),
    /// Read a hit from the cache.
    Serve(Box<Queued>),
}

/// Runs until the Crawler is dropped, which sets `closed` and wakes it.
async fn schedule_loop(s: Arc<Shared>) {
    loop {
        let wake = s.schedule.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let (actions, sleep_until) = {
            let mut state = s.lock();
            if state.closed {
                return;
            }
            plan(&s.settings, &mut state, &s)
        };
        for action in actions {
            match action {
                Action::Render(q, permit) => {
                    tokio::spawn(browser_fetch(s.clone(), *q, permit));
                }
                Action::Fetch(q, route) => {
                    tokio::spawn(fetch(s.clone(), *q, route));
                }
                Action::CheckRobots(origin, route) => {
                    tokio::spawn(check_robots(s.clone(), origin, route));
                }
                Action::CheckTdm(origin, route) => {
                    tokio::spawn(check_tdm(s.clone(), origin, route));
                }
                Action::Serve(q) => {
                    tokio::spawn(serve(s.clone(), *q));
                }
            }
        }
        match sleep_until {
            Some(at) => {
                tokio::select! {
                    _ = wake => {}
                    _ = tokio::time::sleep_until(at.into()) => {}
                }
            }
            None => wake.await,
        }
    }
}

/// Whether `request` goes to Chrome. Chrome navigates to a URL; a POST
/// stays with the HTTP client.
fn wants_browser(settings: &CrawlSettings, request: &CrawlRequest) -> bool {
    (request.browser || settings.browser == BrowserMode::Always) && request.outgoing.method == "GET"
}

/// Queues `request` for `url` (the request's own URL, or where a redirect
/// led) on its host, and marks the host ready; or, for a `hit`, on the
/// queue served from the cache. False if the URL can't be crawled.
fn enqueue(
    settings: &CrawlSettings,
    state: &mut State,
    mut request: CrawlRequest,
    url: &str,
    hops: usize,
    keep: Option<Keep>,
    hit: bool,
) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let (Some(host), true) = (
        parsed.host_str(),
        matches!(parsed.scheme(), "http" | "https"),
    ) else {
        return false;
    };
    let host = host.to_string();
    let origin = parsed.origin().ascii_serialization();
    let path = match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().to_string(),
    };
    request.url = url.to_string();
    state.seq += 1;
    let in_browser = wants_browser(settings, &request);
    let queued = Queued {
        priority: request.priority,
        seq: state.seq,
        request,
        host: host.clone(),
        origin,
        path,
        hops,
        attempts: 0,
        in_browser,
        keep,
        blocked: None,
    };
    // The host is made either way: it counts the request toward
    // max_pages_per_domain, and a hit the cache no longer has goes there.
    let host_entry = state.hosts.entry(host.clone()).or_insert_with(|| Host {
        queue: BinaryHeap::new(),
        in_flight: 0,
        next_at: Instant::now(),
        delay: if settings.throttle {
            settings.start_delay
        } else {
            settings.min_delay
        },
        floor: Duration::ZERO,
        listed: false,
        timer: None,
        accepted: 0,
        session: None,
        proxy: None,
        options: None,
        via: Lru::new(HOST_VIA),
        recent: VecDeque::new(),
    });
    if hit {
        state.hits.push(queued);
    } else {
        host_entry.queue.push(queued);
        state.make_ready(&host);
    }
    state.stats.queued += 1;
    true
}

impl State {
    /// Whether `max_pages` requests have been sent.
    fn capped(&self, settings: &CrawlSettings) -> bool {
        settings.max_pages.is_some_and(|max| self.sent >= max)
    }

    fn make_ready(&mut self, name: &str) {
        if let Some(host) = self.hosts.get_mut(name)
            && !host.listed
        {
            host.listed = true;
            self.ready.push_back(name.to_string());
        }
    }
}

/// Events held for the reader before the scheduler stops starting fetches,
/// so a slow reader doesn't mean the whole frontier downloaded into memory.
fn backlog_limit(settings: &CrawlSettings) -> usize {
    settings.concurrency.saturating_mul(2)
}

/// Starts everything that may start now; returns what to do and when to
/// look again.
fn plan(
    settings: &CrawlSettings,
    state: &mut State,
    shared: &Shared,
) -> (Vec<Action>, Option<Instant>) {
    let now = Instant::now();
    while let Some(Reverse((at, _))) = state.timers.peek() {
        if *at > now {
            break;
        }
        let Some(Reverse((_, name))) = state.timers.pop() else {
            break;
        };
        if let Some(host) = state.hosts.get_mut(&name) {
            host.timer = None;
        }
        state.make_ready(&name);
    }
    // A Chrome page has freed: the hosts that waited for one go first.
    if !state.page_waiters.is_empty() && shared.pages.available_permits() > 0 {
        for name in std::mem::take(&mut state.page_waiters) {
            state.make_ready(&name);
        }
    }
    let mut actions = Vec::new();
    // Hits first: they wait for no host, delay or slot, only for room in
    // the backlog, and count toward max_pages as a fetch would. One at a
    // time, as the cache has one connection anyway: they come back in the
    // order they were queued.
    if !state.serving
        && !state.capped(settings)
        && state.events.len() < backlog_limit(settings)
        && let Some(q) = state.hits.pop()
    {
        state.stats.queued -= 1;
        state.serving = true;
        state.sent += 1;
        actions.push(Action::Serve(Box::new(q)));
    }
    'hosts: while let Some(name) = state.ready.front().cloned() {
        // Gate fetches count against the limit like any other request.
        if state.stats.in_flight + state.gate_fetches >= settings.concurrency
            || state.events.len() >= backlog_limit(settings)
            || state.capped(settings)
        {
            break;
        }
        state.ready.pop_front();
        if let Some(host) = state.hosts.get_mut(&name) {
            host.listed = false;
        }
        loop {
            if state.capped(settings) {
                break 'hosts;
            }
            if state.stats.in_flight + state.gate_fetches >= settings.concurrency {
                // Still has work: back in line for when a slot frees.
                state.make_ready(&name);
                break 'hosts;
            }
            let Some(host) = state.hosts.get(&name) else {
                break;
            };
            let Some(next) = host.queue.peek() else { break };
            let origin = next.origin.clone();
            // A robots.txt or tdmrep.json goes the way this request will.
            let wanted = next.request.proxy.clone();
            let path = &next.path;
            // Robots and TDMRep must be known for this origin first.
            let o = state.origins.entry(origin.clone()).or_default();
            // Some(true) to fetch robots.txt, Some(false) tdmrep.json.
            let mut gate = None;
            let mut wait = false;
            if settings.obey_robots {
                match o.robots {
                    Gate::Unknown => {
                        o.robots = Gate::Fetching;
                        gate = Some(true);
                    }
                    Gate::Fetching => wait = true,
                    Gate::Ready(_) => {}
                }
            }
            if gate.is_none() && !wait && settings.obey_tdmrep {
                match o.tdm {
                    Gate::Unknown => {
                        o.tdm = Gate::Fetching;
                        gate = Some(false);
                    }
                    Gate::Fetching => wait = true,
                    Gate::Ready(_) => {}
                }
            }
            if gate.is_some() || wait {
                if !o.waiting.contains(&name) {
                    o.waiting.push(name.clone());
                }
                if let Some(robots) = gate {
                    state.gate_fetches += 1;
                    // Through the proxy the host's pages would go through,
                    // so the site sees one visitor.
                    let route = gate_route(shared, state, &origin, &name, wanted.as_deref());
                    actions.push(if robots {
                        Action::CheckRobots(origin, route)
                    } else {
                        Action::CheckTdm(origin, route)
                    });
                }
                break;
            }
            let blocked = match (&o.robots, &o.tdm) {
                (Gate::Ready(r), _)
                    if settings.obey_robots && !r.allowed(&settings.robots_agent, path) =>
                {
                    Some(DropReason::Robots)
                }
                (_, Gate::Ready(Some(f)))
                    if settings.obey_tdmrep && f.reservation(path).reserved == Some(true) =>
                {
                    Some(DropReason::TdmReserved)
                }
                _ => None,
            };
            let floor = match &o.robots {
                Gate::Ready(r) => r
                    .crawl_delay(&settings.robots_agent)
                    .unwrap_or_default()
                    .min(settings.max_delay),
                _ => Duration::ZERO,
            };
            let host = state.hosts.get_mut(&name).expect("looked up above");
            if let Some(reason) = blocked {
                let q = host.queue.pop().expect("peeked above");
                state.stats.queued -= 1;
                shared.push_event(
                    state,
                    Event::Dropped {
                        id: q.request.id,
                        reason,
                    },
                );
                continue;
            }
            host.floor = floor;
            if host.in_flight >= settings.per_domain {
                // Ready again when one of its fetches finishes.
                break;
            }
            if now < host.next_at {
                if host.timer != Some(host.next_at) {
                    host.timer = Some(host.next_at);
                    state.timers.push(Reverse((host.next_at, name.clone())));
                }
                break;
            }
            // A browser request starts only with a Chrome page to use, so
            // it never holds a slot other requests could have had while it
            // waits for one.
            let permit = if host.queue.peek().is_some_and(|q| q.in_browser) {
                match shared.pages.clone().try_acquire_owned() {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        if !state.page_waiters.contains(&name) {
                            state.page_waiters.push(name.clone());
                        }
                        break;
                    }
                }
            } else {
                None
            };
            let q = host.queue.pop().expect("peeked above");
            host.in_flight += 1;
            host.next_at = now + host.delay.max(host.floor);
            state.stats.queued -= 1;
            state.stats.in_flight += 1;
            state.sent += 1;
            actions.push(match permit {
                Some(permit) => Action::Render(Box::new(q), permit),
                None => {
                    let route = route(shared, state, &name, q.request.proxy.as_deref());
                    Action::Fetch(Box::new(q), route)
                }
            });
        }
    }
    let sleep_until = state.timers.peek().map(|Reverse((at, _))| *at);
    (actions, sleep_until)
}

/// Runs `work` as its own task, so a panic inside it (a parser bug on
/// hostile input, say) becomes `fallback` instead of killing the caller
/// before it has done its bookkeeping.
async fn guarded<T: Send + 'static>(
    work: impl std::future::Future<Output = T> + Send + 'static,
    fallback: impl FnOnce(String) -> T,
) -> T {
    match tokio::spawn(work).await {
        Ok(value) => value,
        Err(e) => fallback(e.to_string()),
    }
}

async fn fetch(shared: Arc<Shared>, mut q: Queued, (session, proxy): Route) {
    let started = Instant::now();
    let (url, headers, hops) = (q.request.url.clone(), q.request.headers.clone(), q.hops);
    let outgoing = q.request.outgoing.clone();
    let result = match session {
        Ok(session) => {
            guarded(
                async move { session.hop_as(&url, &outgoing, &headers, hops).await },
                |why| {
                    Err(FetchError {
                        kind: FetchErrorKind::Other,
                        message: format!("the fetch failed inside netweir: {why}"),
                    })
                },
            )
            .await
        }
        Err(e) => Err(e),
    };
    let latency = started.elapsed();
    let settings = &shared.settings;
    let outcome = match &result {
        Ok(Hop::Done(r)) => Some(classify(r)),
        _ => None,
    };
    let reserved = match &result {
        Ok(Hop::Done(r)) if settings.obey_tdmrep => {
            page_reserved(&shared.lock(), &q.origin, &q.path, r)
        }
        _ => false,
    };
    // Kept before its event is out, so a page the caller has is on disk.
    let mut stored = Ok(false);
    if let (Some(keep), Some(cache), Ok(Hop::Done(r))) = (&q.keep, &shared.cache, &result)
        && !reserved
        && worth_keeping(r, outcome.as_ref())
    {
        let (cache, r) = (cache.clone(), r.clone());
        let (key, redirects) = (keep.key, keep.redirects.clone());
        stored = tokio::task::spawn_blocking(move || cache.put(&key, &r, &redirects))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
    }
    let mut state = shared.lock();
    match stored {
        Ok(true) => state.stats.cache_stores += 1,
        Ok(false) => {}
        Err(e) => cache_failed(
            &shared,
            &mut state,
            "written",
            &e,
            "carrying on without it for this page",
        ),
    }
    state.stats.in_flight -= 1;
    if let Some(host) = state.hosts.get_mut(&q.host) {
        host.in_flight -= 1;
        if settings.throttle {
            let status = match &result {
                Ok(Hop::Done(r)) => Some(r.status),
                _ => None,
            };
            host.delay = adjust_delay(settings, host.delay, latency, status);
        } else if matches!(outcome, Some(Outcome::Ok)) {
            // No throttle to bring a delay a block added back down: each
            // good response halves it.
            host.delay = (host.delay / 2).max(settings.min_delay);
        }
    }
    if let Some(outcome) = &outcome {
        breaker(
            &shared,
            &mut state,
            &q.host,
            matches!(outcome, Outcome::Blocked { .. }),
        );
    }
    state.make_ready(&q.host);
    // Before a block picks a new session's proxy, so it knows whether this
    // one is still fit to pick.
    if let Some(p) = &proxy {
        let failed = match &result {
            Err(e) => proxy_trouble(e),
            Ok(Hop::Done(r)) => r.status == 407 || matches!(outcome, Some(Outcome::Blocked { .. })),
            Ok(Hop::Redirect(..)) => false,
        };
        record(&shared, &mut state, p, failed);
    }
    // The pool picked this proxy, so the next try can go through another
    // (with per_host, once this one is benched).
    let rotated =
        q.request.proxy.is_none() && proxy.as_deref().is_some_and(|p| state.proxies.pooled(p));
    let can_retry = q.attempts < settings.retries;
    // A POST the server may have acted on is only sent again when asked.
    let may_repeat = q.request.outgoing.idempotent() || q.request.retry_post;
    let event = match (result, outcome) {
        (Ok(Hop::Done(response)), Some(Outcome::Blocked { vendor, kind })) => {
            state.stats.blocked += 1;
            if can_retry {
                let _ = new_session(&shared, &mut state, &q.host);
            }
            slow_down(settings, &mut state, &q.host, None, true);
            if can_retry {
                retry(settings, &mut state, q);
                None
            } else if settings.browser == BrowserMode::OnBlock && q.request.outgoing.method == "GET"
            {
                // One more go, in Chrome, once the host's delay is up. Not a
                // retry: the retries are spent. Chrome replaying a POST could
                // submit it twice, so only GETs go.
                q.in_browser = true;
                q.blocked = Some(Box::new((vendor, kind, response)));
                requeue(&mut state, q, Duration::ZERO);
                None
            } else {
                Some(Event::Blocked {
                    id: q.request.id,
                    vendor,
                    kind,
                    response,
                })
            }
        }
        (Ok(Hop::Done(response)), Some(Outcome::Throttled { retry_after })) => {
            state.stats.throttled += 1;
            // With the throttle on, it has already doubled the delay for
            // this 429.
            slow_down(
                settings,
                &mut state,
                &q.host,
                retry_after,
                !settings.throttle,
            );
            if can_retry {
                retry(settings, &mut state, q);
                None
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: None,
                })
            }
        }
        (Ok(Hop::Done(response)), Some(Outcome::HttpError(status)))
            if can_retry && may_repeat && RETRY_STATUSES.contains(&status) =>
        {
            drop(response);
            retry(settings, &mut state, q);
            None
        }
        // A proxy that wants a login never passed the request on.
        (Ok(Hop::Done(response)), _) if response.status == 407 && rotated && can_retry => {
            drop(response);
            retry(settings, &mut state, q);
            None
        }
        (Ok(Hop::Done(response)), _) => {
            if reserved {
                Some(Event::Dropped {
                    id: q.request.id,
                    reason: DropReason::TdmReserved,
                })
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: None,
                })
            }
        }
        // The next hop is queued like a new request, on its own host, so it
        // goes through that host's robots.txt, TDMRep and limits. Its URL
        // counts as seen, but is fetched even if seen: this request has to
        // end somewhere.
        (Ok(Hop::Redirect(next, then, cookies)), _) => {
            let seen = fingerprint_with(&then.method, &next, then.body.as_deref());
            if let Some(fp) = seen {
                state.seen.insert(fp);
                if let Some(cp) = &shared.checkpoint {
                    cp.seen(fp);
                }
            }
            // For a hit to do the same, with no hops.
            if let Some(keep) = &mut q.keep {
                keep.redirects.push(Redirect {
                    url: q.request.url.clone(),
                    cookies,
                    seen,
                });
            }
            let id = q.request.id;
            q.attempts = 0;
            q.request.outgoing = then;
            if enqueue(
                settings,
                &mut state,
                q.request,
                &next,
                q.hops + 1,
                q.keep,
                false,
            ) {
                None
            } else {
                Some(Event::Failed {
                    id,
                    error: FetchError::invalid(format!(
                        "redirect to a URL netweir can't fetch: {next}"
                    )),
                })
            }
        }
        // A connection that never opened never carried the request; any
        // other failure may have come after the server acted on it.
        (Err(error), _)
            if can_retry
                && transient(&error)
                && (may_repeat || error.kind == FetchErrorKind::Connect) =>
        {
            retry(settings, &mut state, q);
            None
        }
        (Err(error), _) => Some(Event::Failed {
            id: q.request.id,
            error,
        }),
    };
    if let Some(event) = event {
        shared.push_event(&mut state, event);
    }
    drop(state);
    shared.schedule.notify_one();
}

/// Whether the cache keeps `r`: an answer that ended the request and will
/// be the same next time. Never a block, a throttle, a 402, a server error
/// or a status that would have been retried; nor a 401, a 403 or a 451,
/// which may be a block no signature knows yet, a 407 from the crawl's own
/// proxy, or a 304, which only makes sense to the request it answered.
fn worth_keeping(r: &Response, outcome: Option<&Outcome>) -> bool {
    matches!(outcome, Some(Outcome::Ok | Outcome::HttpError(_)))
        && r.status < 500
        && !RETRY_STATUSES.contains(&r.status)
        && ![304, 401, 403, 407, 451].contains(&r.status)
}

/// Hands the caller a hit, read from the cache, as the `Fetched` event a
/// fetch would have made.
async fn serve(shared: Arc<Shared>, q: Queued) {
    let read = match (shared.cache.clone(), q.keep.as_ref().map(|k| k.key)) {
        (Some(cache), Some(key)) => tokio::task::spawn_blocking(move || cache.get(&key))
            .await
            .unwrap_or_else(|e| Err(e.to_string())),
        _ => Ok(None),
    };
    let mut state = shared.lock();
    state.serving = false;
    let read = read.unwrap_or_else(|e| {
        cache_failed(&shared, &mut state, "read", &e, "fetching the page instead");
        None
    });
    match read {
        Some((mut response, redirects)) => {
            state.stats.cache_hits += 1;
            // What the fetch did along the way, done again: each hop's
            // cookies into the jar its host's requests use, and where each
            // led marked seen.
            for r in &redirects {
                session_for(&shared, &state, &r.url).add_set_cookies(&r.url, &r.cookies);
                if let Some(fp) = r.seen {
                    state.seen.insert(fp);
                    if let Some(cp) = &shared.checkpoint {
                        cp.seen(fp);
                    }
                }
            }
            let cookies: Vec<String> = response
                .headers
                .iter()
                .filter(|(k, _)| k == "set-cookie")
                .map(|(_, v)| v.clone())
                .collect();
            session_for(&shared, &state, &response.url).add_set_cookies(&response.url, &cookies);
            // Kept under a key that ignores fragments and tracking
            // parameters, so without a redirect the URL is the one asked for,
            // written as a fetch would write it.
            if redirects.is_empty() {
                response.url =
                    Url::parse(&q.request.url).map_or_else(|_| q.request.url.clone(), String::from);
            }
            // `bytes` counts what was downloaded.
            let size = response.body.len() as u64;
            let event = Event::Fetched {
                id: q.request.id,
                response,
                page: None,
            };
            shared.push_event(&mut state, event);
            state.stats.bytes -= size;
        }
        // Gone or gone stale since it was admitted: fetched after all.
        None => {
            state.sent -= 1;
            requeue(&mut state, q, Duration::ZERO);
        }
    }
    drop(state);
    shared.schedule.notify_one();
}

/// The session a request to `url` goes out with: its host's own, after a
/// block, or the crawl's.
fn session_for(shared: &Shared, state: &State, url: &str) -> Fetcher {
    Url::parse(url)
        .ok()
        .and_then(|u| state.hosts.get(u.host_str()?)?.session.clone())
        .unwrap_or_else(|| shared.fetcher.clone())
}

/// Tells the caller the cache failed, once per crawl: a disk that's full
/// or a file another hand broke would otherwise say so for every page.
fn cache_failed(shared: &Shared, state: &mut State, doing: &str, error: &str, instead: &str) {
    if state.warned_cache {
        return;
    }
    state.warned_cache = true;
    let path = shared
        .cache
        .as_ref()
        .map(|c| c.path().display().to_string());
    let message = format!(
        "the cache at {} couldn't be {doing}: {error}; {instead}",
        path.unwrap_or_default()
    );
    shared.push_event(state, Event::Warning { message });
}

/// How long a challenge page gets to pass and move on in Chrome.
const CHALLENGE_WAIT: Duration = Duration::from_secs(20);

/// What `render` brings back: the response, the open page, its cookies
/// when asked for, how long the page took to load, and Chrome's version.
type Rendered = (Response, LivePage, Vec<Cookie>, Duration, String);

/// Fetches `q` in Chrome, in the page slot `permit` holds, and deals with
/// the outcome as `fetch` does for the HTTP client. A page that's still
/// blocked isn't retried in Chrome.
async fn browser_fetch(shared: Arc<Shared>, mut q: Queued, permit: OwnedSemaphorePermit) {
    let timeout = shared.options.timeout;
    // A Chrome that stops answering mid-page mustn't hold the request, and
    // its place in the crawl, for good.
    let limit = timeout.saturating_mul(2) + CHALLENGE_WAIT;
    // Set if robots.txt or TDMRep stopped a document Chrome was about to
    // fetch: a redirect hop, or a navigation the page's script started.
    let refused: Refusal = Arc::default();
    // What render is doing, for the error if it runs out of time.
    let doing: Doing = Arc::new(Mutex::new("starting"));
    let work = render(
        shared.clone(),
        q.request.url.clone(),
        q.blocked.is_some(),
        permit,
        refused.clone(),
        doing.clone(),
    );
    let rendered = guarded(
        async move {
            tokio::time::timeout(limit, work).await.unwrap_or_else(|_| {
                Err(FetchError {
                    kind: FetchErrorKind::Timeout,
                    message: format!(
                        "Chrome took more than {}s, and was still {}",
                        limit.as_secs(),
                        doing.lock().unwrap_or_else(|e| e.into_inner())
                    ),
                })
            })
        },
        |why| {
            Err(FetchError {
                kind: FetchErrorKind::Other,
                message: format!("the browser fetch failed inside netweir: {why}"),
            })
        },
    )
    .await;
    let settings = &shared.settings;
    let outcome = rendered.as_ref().ok().map(|(r, ..)| classify(r));
    let mut state = shared.lock();
    state.stats.in_flight -= 1;
    if let Some(host) = state.hosts.get_mut(&q.host) {
        host.in_flight -= 1;
        if settings.throttle
            && let Ok((r, _, _, latency, _)) = &rendered
        {
            host.delay = adjust_delay(settings, host.delay, *latency, Some(r.status));
        } else if !settings.throttle && matches!(outcome, Some(Outcome::Ok)) {
            // As over HTTP: a good response halves a delay a block or a 429
            // added, with no throttle to bring it back down.
            host.delay = (host.delay / 2).max(settings.min_delay);
        }
    }
    if let Some(outcome) = &outcome {
        state.stats.browser_fetches += 1;
        breaker(
            &shared,
            &mut state,
            &q.host,
            matches!(outcome, Outcome::Blocked { .. }),
        );
    }
    state.make_ready(&q.host);
    let refusal = *refused.lock().unwrap_or_else(|e| e.into_inner());
    // Pages to close once the lock is released.
    let mut discard = None;
    let event = match (rendered, outcome) {
        (Ok((response, page, ..)), Some(Outcome::Blocked { vendor, kind })) => {
            discard = Some(page);
            state.stats.blocked += 1;
            slow_down(settings, &mut state, &q.host, None, true);
            Some(Event::Blocked {
                id: q.request.id,
                vendor,
                kind,
                response,
            })
        }
        // A server error is tried again, as the HTTP client would, unless
        // this is a blocked request's one go in Chrome.
        (Ok((response, page, ..)), Some(Outcome::HttpError(status)))
            if q.blocked.is_none()
                && q.attempts < settings.retries
                && RETRY_STATUSES.contains(&status) =>
        {
            discard = Some(page);
            drop(response);
            retry(settings, &mut state, q);
            None
        }
        // Throttled (a 429, or a 503 with Retry-After): slowed down and
        // tried again, as over HTTP.
        (Ok((response, page, ..)), Some(Outcome::Throttled { retry_after })) => {
            state.stats.throttled += 1;
            slow_down(
                settings,
                &mut state,
                &q.host,
                retry_after,
                !settings.throttle,
            );
            if q.blocked.is_none() && q.attempts < settings.retries {
                discard = Some(page);
                drop(response);
                retry(settings, &mut state, q);
                None
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: Some(page),
                })
            }
        }
        (Ok((response, page, cookies, _, version)), outcome) => {
            // Chrome follows redirects itself: where it ended up counts as
            // seen, as a redirect the HTTP client followed would.
            if let Some(fp) = fingerprint("GET", &response.url)
                && state.seen.insert(fp)
                && let Some(cp) = &shared.checkpoint
            {
                cp.seen(fp);
            }
            if q.blocked.is_some() && matches!(outcome, Some(Outcome::Ok)) {
                state.stats.browser_unblocked += 1;
                if let Some(message) = hand_back(&shared, &mut state, &q.host, &cookies, &version) {
                    shared.push_event(&mut state, Event::Warning { message });
                }
            }
            // Chrome may have landed elsewhere: its page is judged by the
            // site it came from.
            let landed =
                origin_and_path(&response.url).unwrap_or((q.origin.clone(), q.path.clone()));
            if settings.obey_tdmrep && page_reserved(&state, &landed.0, &landed.1, &response) {
                discard = Some(page);
                Some(Event::Dropped {
                    id: q.request.id,
                    reason: DropReason::TdmReserved,
                })
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: Some(page),
                })
            }
        }
        (Err(_), _) if refusal.is_some() => Some(Event::Dropped {
            id: q.request.id,
            reason: refusal.expect("checked"),
        }),
        (Err(error), _) if q.attempts < settings.retries && transient(&error) => {
            retry(settings, &mut state, q);
            None
        }
        (Err(error), _) => match q.blocked.take() {
            // Chrome didn't get anywhere: it's still the block it was. Say
            // why once, or escalation would fail without a word.
            Some(blocked) => {
                if !state.warned_browser {
                    state.warned_browser = true;
                    let message = format!(
                        "a blocked request couldn't be tried in Chrome, so on_block gets it: {}",
                        error.message
                    );
                    shared.push_event(&mut state, Event::Warning { message });
                }
                let (vendor, kind, response) = *blocked;
                Some(Event::Blocked {
                    id: q.request.id,
                    vendor,
                    kind,
                    response,
                })
            }
            None => Some(Event::Failed {
                id: q.request.id,
                error,
            }),
        },
    };
    if let Some(event) = event {
        shared.push_event(&mut state, event);
    }
    drop(state);
    drop(discard);
    shared.schedule.notify_one();
}

/// Loads `url` in a new Chrome page and reads it back. A challenge page is
/// given time to pass and move on. With `cookies`, also returns the
/// page's cookies.
async fn render(
    shared: Arc<Shared>,
    url: String,
    cookies: bool,
    permit: OwnedSemaphorePermit,
    refused: Refusal,
    doing: Doing,
) -> Result<Rendered, FetchError> {
    let now = |what: &'static str| *doing.lock().unwrap_or_else(|e| e.into_inner()) = what;
    now("waiting for Chrome to start");
    let browser = {
        let mut slot = shared.browser.lock().await;
        let start = match &*slot {
            None => true,
            Some(Ok(b)) => b.is_closed(),
            Some(Err((_, when))) => when.elapsed() > Duration::from_secs(60),
        };
        if start {
            let launched = Browser::launch(browser_launch(&shared)).await;
            *slot = Some(launched.map_err(|e| (e.to_string(), Instant::now())));
        }
        slot.clone().expect("set above").map_err(|(e, _)| e)
    }
    .map_err(|e| FetchError {
        kind: FetchErrorKind::Other,
        message: format!("can't start Chrome: {e}"),
    })?;
    let guarded = shared.settings.obey_robots || shared.settings.obey_tdmrep;
    now("opening a page");
    let page = if guarded {
        // Chrome follows redirects, and the page's scripts navigate, by
        // itself; each document is checked against its site's robots.txt
        // and TDMRep before Chrome asks for it, as the HTTP client checks
        // each hop it follows.
        let guarding = shared.clone();
        let refusing = refused.clone();
        let guard: netweir_browser::Guard = Arc::new(move |url: String| {
            let (shared, refused) = (guarding.clone(), refusing.clone());
            Box::pin(async move {
                let refusal = refusal_of(&shared, &url).await;
                if refusal.is_some() {
                    *refused.lock().unwrap_or_else(|e| e.into_inner()) = refusal;
                }
                refusal.is_none()
            })
        });
        browser.new_guarded_page(guard).await
    } else {
        browser.new_page().await
    }
    .map_err(browser_error)?;
    let live = LivePage {
        inner: Arc::new(Live {
            page: page.clone(),
            permit: Mutex::new(Some(permit)),
            runtime: shared.runtime.clone(),
            shared: Arc::downgrade(&shared),
        }),
    };
    let timeout = shared.options.timeout;
    let started = Instant::now();
    // A guard may have to fetch a site's robots.txt or tdmrep.json, each
    // with the request timeout, before the page itself can load.
    let load_limit = if guarded {
        timeout.saturating_mul(3)
    } else {
        timeout
    };
    now("loading the page");
    let mut document = page
        .goto(&url, WaitUntil::Load, Some(load_limit))
        .await
        .map_err(browser_error)?;
    let latency = started.elapsed();
    let deadline = Instant::now() + CHALLENGE_WAIT.min(timeout);
    let response = loop {
        // A script may already have moved the page on (a challenge that
        // passes at once does): read the newest document, once loaded.
        if page.response().loader != document.loader {
            now("loading the page a script moved on to");
            let left = deadline.saturating_duration_since(Instant::now());
            match page
                .wait_for_navigation(&document, WaitUntil::Load, Some(left))
                .await
            {
                Ok(next) => document = next,
                Err(netweir_browser::Error::Timeout(_)) => {}
                Err(e) => return Err(browser_error(e)),
            }
        }
        now("reading the page");
        let response = rendered(&document, content(&page).await?);
        let challenged = matches!(
            classify(&response),
            Outcome::Blocked {
                kind: BlockKind::Challenge,
                ..
            }
        );
        let left = deadline.saturating_duration_since(Instant::now());
        if !challenged || left.is_zero() {
            break response;
        }
        now("waiting for a challenge to pass");
        match page
            .wait_for_navigation(&document, WaitUntil::Load, Some(left))
            .await
        {
            Ok(next) => document = next,
            // Out of time. One last look, for a challenge that swaps the
            // page's content without navigating.
            Err(netweir_browser::Error::Timeout(_)) => {
                now("reading the page");
                break rendered(&page.response(), content(&page).await?);
            }
            Err(e) => return Err(browser_error(e)),
        }
    };
    // A navigation that was refused leaves Chrome's error page showing,
    // which isn't the page.
    if refused.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        && (response.status == 0 || response.url.starts_with("chrome-error://"))
    {
        return Err(FetchError {
            kind: FetchErrorKind::Other,
            message: "Chrome was refused the page it was going to".into(),
        });
    }
    now("reading its cookies");
    let cookies = if cookies {
        page.cookies().await.map_err(browser_error)?
    } else {
        Vec::new()
    };
    Ok((
        response,
        live,
        cookies,
        latency,
        browser.version().to_string(),
    ))
}

/// Why a document Chrome is about to fetch may not be: robots.txt or
/// TDMRep, if the crawl obeys them.
type Refusal = Arc<Mutex<Option<DropReason>>>;

/// What a Chrome fetch is doing, in words that finish "still ...".
type Doing = Arc<Mutex<&'static str>>;

/// A URL's origin and path (with its query), as the scheduler keys them.
fn origin_and_path(url: &str) -> Option<(String, String)> {
    let parsed = Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let path = match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().to_string(),
    };
    Some((parsed.origin().ascii_serialization(), path))
}

/// Why the crawl may not fetch `url`, if it may not: its site's robots.txt
/// or TDMRep file, each fetched first, as the scheduler would, if it isn't
/// known yet.
async fn refusal_of(shared: &Arc<Shared>, url: &str) -> Option<DropReason> {
    let settings = &shared.settings;
    let (origin, path) = origin_and_path(url)?;
    let host = Url::parse(url).ok()?.host_str()?.to_string();
    if settings.obey_robots {
        loop {
            let fetch = {
                let mut state = shared.lock();
                let o = state.origins.entry(origin.clone()).or_default();
                match &o.robots {
                    Gate::Ready(r) if r.allowed(&settings.robots_agent, &path) => break,
                    Gate::Ready(_) => return Some(DropReason::Robots),
                    Gate::Fetching => None,
                    Gate::Unknown => {
                        o.robots = Gate::Fetching;
                        state.gate_fetches += 1;
                        Some(gate_route(shared, &mut state, &origin, &host, None))
                    }
                }
            };
            if let Some(route) = fetch {
                check_robots(shared.clone(), origin.clone(), route).await;
            } else {
                // Known limit: another fetch of it is under way; polled, as
                // this is one request's wait, not the scheduler's.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    if settings.obey_tdmrep {
        loop {
            let fetch = {
                let mut state = shared.lock();
                let o = state.origins.entry(origin.clone()).or_default();
                match &o.tdm {
                    Gate::Ready(Some(f)) if f.reservation(&path).reserved == Some(true) => {
                        return Some(DropReason::TdmReserved);
                    }
                    Gate::Ready(_) => break,
                    Gate::Fetching => None,
                    Gate::Unknown => {
                        o.tdm = Gate::Fetching;
                        state.gate_fetches += 1;
                        Some(gate_route(shared, &mut state, &origin, &host, None))
                    }
                }
            };
            if let Some(route) = fetch {
                check_tdm(shared.clone(), origin.clone(), route).await;
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    None
}

/// The page's HTML, read again if a script moved the page on mid-read
/// (Chrome says the isolated world's context is gone).
async fn content(page: &netweir_browser::Page) -> Result<String, FetchError> {
    let mut tries = 0;
    loop {
        match page.content().await {
            Err(netweir_browser::Error::Protocol { message, .. })
                if message.contains("context") && tries < 3 =>
            {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            other => return other.map_err(browser_error),
        }
    }
}

/// How to start Chrome for this crawl: through the crawl's proxy, login
/// and all, so the cookies Chrome earns come from the address the HTTP
/// client uses. With per_request or per_host rotation there's no crawl
/// proxy, and Chrome takes the next healthy one of `proxies`: it has one
/// proxy for all its pages, so rotation stops at its door.
fn browser_launch(shared: &Shared) -> LaunchOptions {
    let mut launch = shared.settings.browser_launch.clone();
    let mut state = shared.lock();
    if launch.proxy.is_none() {
        launch.proxy = match shared.settings.proxy_rotation {
            ProxyRotation::OnBlock => shared.options.proxy.clone(),
            ProxyRotation::PerRequest | ProxyRotation::PerHost => {
                let picked = pick_where(shared, &mut state, None, chrome_can_use);
                if picked.is_none() && !state.warned_chrome_proxy {
                    state.warned_chrome_proxy = true;
                    let message = "Chrome can't log in to a SOCKS proxy, and every one of \
                                   proxies is one with a login, so Chrome goes out without a proxy"
                        .to_string();
                    shared.push_event(&mut state, Event::Warning { message });
                }
                picked
            }
        };
    }
    state.chrome_proxy = launch.proxy.clone();
    drop(state);
    if launch.brands.is_none() {
        launch.brands = Some(Arc::new(Profile::chrome_brands));
    }
    launch
}

/// Whether Chrome can go through `proxy`: it can't log in to a SOCKS one.
fn chrome_can_use(proxy: &str) -> bool {
    let socks = proxy.to_ascii_lowercase().starts_with("socks");
    let login = proxy
        .split_once("://")
        .is_some_and(|(_, rest)| rest.contains('@'));
    !(socks && login)
}

/// A Response for a page Chrome rendered: the document's status and
/// headers, and the HTML as it is now. The body is UTF-8 whatever the
/// server sent, so the content type says so, and nothing about the body's
/// original encoding or length still applies.
fn rendered(document: &netweir_browser::Response, html: String) -> Response {
    let mut headers: Vec<(String, String)> = document
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "content-encoding" | "content-length" | "transfer-encoding"
            )
        })
        .collect();
    let mime = headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .and_then(|(_, v)| v.split(';').next())
        .map_or("text/html", str::trim)
        .to_string();
    headers.retain(|(k, _)| k != "content-type");
    headers.push(("content-type".into(), format!("{mime}; charset=utf-8")));
    Response {
        url: document.url.clone(),
        status: document.status,
        version: "browser",
        headers,
        body: html.into(),
    }
}

fn browser_error(e: netweir_browser::Error) -> FetchError {
    use netweir_browser::Error;
    let kind = match &e {
        Error::Timeout(_) => FetchErrorKind::Timeout,
        Error::Navigation(m) if m.contains("TIMED_OUT") => FetchErrorKind::Timeout,
        Error::Navigation(m) if m.contains("CERT") || m.contains("SSL") => FetchErrorKind::Tls,
        Error::Navigation(m)
            if [
                "CONNECTION",
                "NAME_NOT_RESOLVED",
                "ADDRESS",
                "INTERNET_DISCONNECTED",
                "PROXY",
            ]
            .iter()
            .any(|n| m.contains(n)) =>
        {
            FetchErrorKind::Connect
        }
        _ => FetchErrorKind::Other,
    };
    FetchError {
        kind,
        message: format!("in Chrome: {e}"),
    }
}

/// Gives the host an HTTP session holding the cookies Chrome earned, so
/// its next requests don't need Chrome. A site may tie those cookies to
/// the browser they were issued to, so the session looks like the same
/// Chrome when netweir has its profile. When it doesn't, the session keeps
/// the crawl's profile and a warning (returned, once) says so.
fn hand_back(
    shared: &Shared,
    state: &mut State,
    host: &str,
    cookies: &[Cookie],
    version: &str,
) -> Option<String> {
    let major = version
        .trim_start_matches("Chrome/")
        .split('.')
        .next()
        .unwrap_or_default()
        .to_string();
    let mut options = shared.options.clone();
    let mut warning = None;
    if !options
        .profile
        .name
        .starts_with(&format!("chrome-{major}-"))
    {
        match Profile::for_chrome(&major) {
            Some(profile) => options.profile = profile,
            None if !state.warned_profile => {
                state.warned_profile = true;
                warning = Some(format!(
                    "the installed {version} has no matching netweir profile, so cookies it earned \
                     go to requests that look like {}; a site may not accept them",
                    options.profile.name
                ));
            }
            None => {}
        }
    }
    // The cookies are tied to the address Chrome went out from.
    options.proxy = state.chrome_proxy.clone();
    let pooled = state
        .chrome_proxy
        .clone()
        .filter(|p| state.proxies.pooled(p));
    if let (Ok(fetcher), Some(h)) = (Fetcher::new(options.clone()), state.hosts.get_mut(host)) {
        fetcher.add_cookies(cookies);
        h.session = Some(fetcher);
        h.options = Some(options);
        h.proxy = pooled;
        h.via.clear();
        state.stats.sessions_replaced += 1;
    }
    warning
}

/// Server errors worth another try: timeouts and overloaded or unreachable
/// upstreams (522 and 524 are Cloudflare's).
const RETRY_STATUSES: [u16; 7] = [408, 500, 502, 503, 504, 522, 524];

/// Network failures that may pass on another try.
fn transient(error: &FetchError) -> bool {
    matches!(
        error.kind,
        FetchErrorKind::Timeout
            | FetchErrorKind::Connect
            | FetchErrorKind::Body
            | FetchErrorKind::Other
    )
}

/// Puts `q` back on its host's queue, after a backoff.
fn retry(settings: &CrawlSettings, state: &mut State, mut q: Queued) {
    q.attempts += 1;
    state.stats.retries += 1;
    let cap = settings
        .backoff_base
        .saturating_mul(1u32 << q.attempts.min(20))
        .min(settings.backoff_max);
    let wait = cap.mul_f64(random_fraction(q.seq));
    requeue(state, q, wait);
}

/// Puts `q` back on its host's queue, to start no sooner than `wait`.
fn requeue(state: &mut State, mut q: Queued, wait: Duration) {
    let host = q.host.clone();
    state.seq += 1;
    q.seq = state.seq;
    if let Some(h) = state.hosts.get_mut(&host) {
        h.next_at = h.next_at.max(Instant::now() + wait);
        h.queue.push(q);
        state.stats.queued += 1;
    }
    state.make_ready(&host);
}

/// A number in [0, 1) that differs per call; for jitter, not secrets.
fn random_fraction(salt: u64) -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(salt);
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Blocks and throttling count against a host's pace: its delay doubles
/// (to at least half a second), and a Retry-After pushes its next request
/// back.
fn slow_down(
    settings: &CrawlSettings,
    state: &mut State,
    host: &str,
    wait: Option<Duration>,
    double: bool,
) {
    let Some(h) = state.hosts.get_mut(host) else {
        return;
    };
    if double {
        h.delay = h
            .delay
            .saturating_mul(2)
            .max(Duration::from_millis(500))
            .min(settings.max_delay);
    }
    // From now, not from the host's next turn, which was set before.
    let wait = wait
        .unwrap_or_default()
        .min(settings.max_delay)
        .max(h.delay);
    h.next_at = h.next_at.max(Instant::now() + wait);
}

/// Replaces the host's session: a fresh cookie jar, through the next
/// healthy proxy (another than the host had, if there is one) when there
/// are proxies. With per_request rotation, the host's cookies leave the
/// crawl's jar instead, and its requests go back to the rotation.
fn new_session(shared: &Shared, state: &mut State, host: &str) -> Result<(), FetchError> {
    new_session_through(shared, state, host, None)
}

/// `new_session`, through `proxy` when it's given.
fn new_session_through(
    shared: &Shared,
    state: &mut State,
    host: &str,
    proxy: Option<String>,
) -> Result<(), FetchError> {
    let mode = shared.settings.proxy_rotation;
    let Some(h) = state.hosts.get_mut(host) else {
        return Ok(());
    };
    if mode == ProxyRotation::PerRequest {
        shared.fetcher.forget(host);
        h.session = None;
        h.proxy = None;
        h.options = None;
        h.via.clear();
        state.stats.sessions_replaced += 1;
        return Ok(());
    }
    // A host's first session, with per_host, replaces nothing.
    let replacing = h.session.is_some() || mode == ProxyRotation::OnBlock;
    let current = h.proxy.clone();
    let proxy = proxy.or_else(|| pick(shared, state, current.as_deref()));
    let mut options = shared.options.clone();
    if proxy.is_some() {
        options.proxy = proxy.clone();
    }
    let fetcher = Fetcher::new(options.clone())?;
    if let Some(h) = state.hosts.get_mut(host) {
        h.session = Some(fetcher);
        h.proxy = proxy;
        h.options = Some(options);
        h.via.clear();
    }
    if replacing {
        state.stats.sessions_replaced += 1;
    }
    Ok(())
}

/// The session and proxy a request to host `name` goes out with; `wanted`
/// is the request's own proxy. A host whose proxy is benched moves to a
/// healthy one first, if there is one, and with per_host a host's first
/// request gets it its proxy.
fn route(shared: &Shared, state: &mut State, name: &str, wanted: Option<&str>) -> Route {
    let mode = shared.settings.proxy_rotation;
    let Some(h) = state.hosts.get(name) else {
        // A site Chrome went to on its own: no session to keep to.
        return match mode {
            ProxyRotation::OnBlock => (Ok(shared.fetcher.clone()), None),
            _ => rotate(shared, state),
        };
    };
    let now = Instant::now();
    let benched = h
        .proxy
        .as_deref()
        .is_some_and(|p| state.proxies.benched(p, now));
    // Off a benched proxy: to a healthy one, or with every one benched, to
    // the one back soonest, if that's another.
    let moving = benched
        && (state.proxies.any_healthy(now) || state.proxies.soonest() != h.proxy.as_deref());
    // A per_host site gets its proxy when a request first takes the
    // rotation; one whose requests name their own never does.
    let unassigned = mode == ProxyRotation::PerHost && h.session.is_none() && wanted.is_none();
    if (moving || unassigned)
        && let Err(e) = new_session(shared, state, name)
    {
        return (Err(e), None);
    }
    let h = state.hosts.get_mut(name).expect("looked up above");
    if let Some(proxy) = wanted {
        let fetcher = match (h.session.clone(), h.via.get(proxy)) {
            (Some(_), Some(fetcher)) => Ok(fetcher),
            (Some(session), None) => {
                let options = h.options.clone().unwrap_or_else(|| shared.options.clone());
                let made = Fetcher::sharing_cookies(with_proxy(options, proxy), &session);
                if let Ok(fetcher) = &made {
                    h.via.insert(proxy, fetcher.clone());
                }
                made
            }
            (None, _) => through(shared, state, proxy),
        };
        // An empty proxy is no proxy at all, which has no stats.
        return (fetcher, Some(proxy.to_string()).filter(|p| !p.is_empty()));
    }
    if let Some(session) = &h.session {
        return (Ok(session.clone()), h.proxy.clone());
    }
    match mode {
        ProxyRotation::PerRequest => rotate(shared, state),
        // Never without a session: it would go out with no proxy at all.
        ProxyRotation::PerHost => (
            Err(FetchError::invalid(
                "the host has no session through a proxy",
            )),
            None,
        ),
        ProxyRotation::OnBlock => (Ok(shared.fetcher.clone()), None),
    }
}

/// The crawl's cookies, through the next healthy proxy.
fn rotate(shared: &Shared, state: &mut State) -> Route {
    match pick(shared, state, None) {
        Some(proxy) => (through(shared, state, &proxy), Some(proxy)),
        None => (
            Err(FetchError::invalid("no proxies to rotate through")),
            None,
        ),
    }
}

/// The crawl's cookies, through `proxy`.
fn through(shared: &Shared, state: &mut State, proxy: &str) -> Result<Fetcher, FetchError> {
    if let Some(fetcher) = state.via.get(proxy) {
        return Ok(fetcher);
    }
    let options = with_proxy(shared.options.clone(), proxy);
    let fetcher = Fetcher::sharing_cookies(options, &shared.fetcher)?;
    state.via.insert(proxy, fetcher.clone());
    Ok(fetcher)
}

/// `options` through `proxy`; an empty one is none at all, not even the
/// crawl's own.
fn with_proxy(mut options: FetchOptions, proxy: &str) -> FetchOptions {
    options.proxy = Some(proxy.to_string()).filter(|p| !p.is_empty());
    options
}

/// The next healthy proxy in turn, another than `avoid` if there is one.
/// When every proxy is benched, the one back soonest, and a warning (once,
/// until one is healthy again): the crawl slows, but doesn't stall. None
/// without proxies.
fn pick(shared: &Shared, state: &mut State, avoid: Option<&str>) -> Option<String> {
    pick_where(shared, state, avoid, |_| true)
}

/// `pick`, among the proxies `usable` takes.
fn pick_where(
    shared: &Shared,
    state: &mut State,
    avoid: Option<&str>,
    usable: impl Fn(&str) -> bool,
) -> Option<String> {
    let (url, warn) = match state.proxies.pick(Instant::now(), avoid, usable) {
        Picked::Healthy(url) => return Some(url),
        Picked::Soonest(url, warn) => (url, warn),
        Picked::None => return None,
    };
    if warn {
        let message = format!(
            "every proxy is benched; requests go through {}, the first back, until one is healthy",
            masked(&url)
        );
        shared.push_event(state, Event::Warning { message });
    }
    Some(url)
}

/// Counts a request through `proxy`. One of the pool that has failed
/// `proxy_failures` times in a row is benched for `proxy_cooldown`, twice
/// that each time after, until a success clears its record.
fn record(shared: &Shared, state: &mut State, proxy: &str, failed: bool) {
    let settings = &shared.settings;
    let now = Instant::now();
    let pool = &mut state.proxies;
    let k = pool.entry(proxy);
    let pooled = k < pool.pooled;
    let e = &mut pool.entries[k];
    e.stats.requests += 1;
    if !failed {
        e.run = 0;
        e.level = 0;
        e.until = None;
        return;
    }
    e.stats.failures += 1;
    e.run += 1;
    // One back from the bench goes straight back on it if it fails again.
    if !pooled || e.run < settings.proxy_failures || e.benched(now) {
        return;
    }
    let cooldown = settings
        .proxy_cooldown
        .saturating_mul(1 << e.level.min(20))
        .min(MAX_COOLDOWN.max(settings.proxy_cooldown));
    e.level += 1;
    e.until = Some(now + cooldown);
    e.stats.benched += 1;
    let failed = match e.run {
        1 => "once".to_string(),
        n => format!("{n} times in a row"),
    };
    let message = format!(
        "proxy {} failed {failed}; benched for {cooldown:?}",
        masked(proxy)
    );
    state.stats.proxies_benched += 1;
    shared.push_event(state, Event::Warning { message });
}

/// Whether a failed request counts against the proxy it went through;
/// see `FetchError::proxy_at_fault`.
fn proxy_trouble(error: &FetchError) -> bool {
    error.proxy_at_fault()
}

/// Responses the breaker needs to have seen from a host before it judges.
const BREAKER_MIN_SEEN: usize = 10;

/// Records whether the host's latest response was a block, and pauses the
/// host when too many of its recent ones were.
fn breaker(shared: &Shared, state: &mut State, host: &str, blocked: bool) {
    let settings = &shared.settings;
    let Some(h) = state.hosts.get_mut(host) else {
        return;
    };
    h.recent.push_back(blocked);
    while h.recent.len() > settings.breaker_window {
        h.recent.pop_front();
    }
    let blocks = h.recent.iter().filter(|b| **b).count();
    // It judges once it has seen a few responses, not only once the window
    // is full: a site that blocks from its first answer pauses quickly.
    let enough = settings.breaker_window.min(BREAKER_MIN_SEEN);
    if settings.breaker_window == 0
        || h.recent.len() < enough
        || blocks as f64 <= settings.breaker_ratio * h.recent.len() as f64
    {
        return;
    }
    h.recent.clear();
    h.next_at = h.next_at.max(Instant::now() + settings.breaker_pause);
    state.stats.breaker_trips += 1;
    shared.push_event(
        state,
        Event::Paused {
            host: host.to_string(),
            pause: settings.breaker_pause,
        },
    );
}

/// The file's word for this path, overridden by the response's headers and
/// then its `<meta>`, as TDMRep orders them.
fn page_reserved(state: &State, origin: &str, path: &str, response: &Response) -> bool {
    let from_file = match state.origins.get(origin).map(|o| &o.tdm) {
        Some(Gate::Ready(Some(f))) => f.reservation(path),
        _ => Reservation::default(),
    };
    let headers = tdmrep::from_headers(
        response
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    let html = tdmrep::from_html(&response.body);
    from_file
        .overridden_by(headers)
        .overridden_by(html)
        .reserved
        == Some(true)
}

/// AutoThrottle: aim for `latency / target_concurrency` between requests,
/// moving halfway there each response. Only a successful response may lower
/// the delay; a 429 doubles it.
fn adjust_delay(
    s: &CrawlSettings,
    current: Duration,
    latency: Duration,
    status: Option<u16>,
) -> Duration {
    let target = latency.div_f64(s.target_concurrency.max(0.01));
    let next = match status {
        Some(429) => current.saturating_mul(2).max(Duration::from_millis(500)),
        Some(200) => (current + target) / 2,
        _ => current.max((current + target) / 2),
    };
    next.clamp(s.min_delay, s.max_delay)
}

async fn check_robots(shared: Arc<Shared>, origin: String, route: Route) {
    let url = format!("{origin}/robots.txt");
    let Some(got) = gate_get(&shared, &origin, url, route, |o| &mut o.robots).await else {
        return;
    };
    let robots = guarded(
        async move {
            match got {
                // The parser reads at most robots::MAX_BYTES of it.
                Ok(r) if (200..300).contains(&r.status) => Robots::parse(&r.text()),
                // A proxy wanting a login isn't the site's answer: as good
                // as no answer, like a network error.
                Ok(r) if r.status == 407 => Robots::disallow_all(),
                // RFC 9309: an unavailable file (4xx) means no rules.
                Ok(r) if (400..500).contains(&r.status) => Robots::allow_all(),
                // An unreachable file (5xx, or no answer) means everything
                // is off limits for now.
                _ => Robots::disallow_all(),
            }
        },
        // A file netweir can't read is treated as unreachable.
        |_| Robots::disallow_all(),
    )
    .await;
    let mut state = shared.lock();
    state.origins.entry(origin.clone()).or_default().robots = Gate::Ready(robots);
    gate_done(&shared, state, &origin);
}

async fn check_tdm(shared: Arc<Shared>, origin: String, route: Route) {
    let url = format!("{origin}/.well-known/tdmrep.json");
    let Some(got) = gate_get(&shared, &origin, url, route, |o| &mut o.tdm).await else {
        return;
    };
    let file = guarded(
        async move {
            match got {
                Ok(r) if r.status == 200 => TdmFile::parse(&r.text()),
                // Anything else, a network error or a proxy's 407 among
                // them, is no file.
                _ => None,
            }
        },
        |_| None,
    )
    .await;
    let mut state = shared.lock();
    state.origins.entry(origin.clone()).or_default().tdm = Gate::Ready(file);
    gate_done(&shared, state, &origin);
}

/// GETs a robots.txt or tdmrep.json, and counts it toward its proxy's
/// health. None if a proxy of the pool failed and there's a healthy one
/// that hasn't failed this origin yet: the file is then fetched again
/// through it, whatever `retries` says, since a dead proxy mustn't decide
/// what a whole site allows. A host with a session of its own moves to
/// that proxy, as its pages would have gone through the dead one too.
async fn gate_get<T>(
    shared: &Arc<Shared>,
    origin: &str,
    url: String,
    (fetcher, proxy): Route,
    gate: impl FnOnce(&mut Origin) -> &mut Gate<T>,
) -> Option<Result<Response, FetchError>> {
    let got = match fetcher {
        Ok(fetcher) => {
            guarded(async move { fetcher.get(&url, &[]).await }, |why| {
                Err(FetchError {
                    kind: FetchErrorKind::Other,
                    message: format!("the fetch failed inside netweir: {why}"),
                })
            })
            .await
        }
        Err(e) => Err(e),
    };
    let Some(proxy) = proxy else {
        return Some(got);
    };
    let failed = match &got {
        Err(e) => proxy_trouble(e),
        Ok(r) => r.status == 407,
    };
    let mut state = shared.lock();
    record(shared, &mut state, &proxy, failed);
    if !failed || !state.proxies.pooled(&proxy) {
        return Some(got);
    }
    let o = state.origins.entry(origin.to_string()).or_default();
    o.failed_proxies.push(proxy);
    let failed_here = o.failed_proxies.clone();
    let next = state
        .proxies
        .choose(Instant::now(), |p| !failed_here.iter().any(|f| f == p))
        .filter(|(_, healthy)| *healthy)
        .map(|(k, _)| state.proxies.entries[k].url.clone());
    let Some(next) = next else {
        return Some(got);
    };
    let host = Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_default();
    let has_session = state.hosts.get(&host).is_some_and(|h| h.session.is_some());
    if has_session && shared.settings.proxy_rotation != ProxyRotation::PerRequest {
        // Moving it may fail only as building any client may; the next
        // try then goes as the host's requests would.
        let _ = new_session_through(shared, &mut state, &host, Some(next));
    } else {
        state
            .origins
            .entry(origin.to_string())
            .or_default()
            .retry_through = Some(next);
    }
    *gate(state.origins.entry(origin.to_string()).or_default()) = Gate::Unknown;
    gate_done(shared, state, origin);
    None
}

/// The way a robots.txt or tdmrep.json fetch for `origin`, on behalf of
/// host `name`, goes: through the proxy a failed try picked, or as the
/// request waiting for it would (`wanted` being its own proxy).
fn gate_route(
    shared: &Shared,
    state: &mut State,
    origin: &str,
    name: &str,
    wanted: Option<&str>,
) -> Route {
    let again = state
        .origins
        .get_mut(origin)
        .and_then(|o| o.retry_through.take());
    match again {
        Some(proxy) => (through(shared, state, &proxy), Some(proxy)),
        None => route(shared, state, name, wanted),
    }
}

/// A gate fetch finished: the hosts waiting on it may go on.
fn gate_done(shared: &Shared, mut state: std::sync::MutexGuard<'_, State>, origin: &str) {
    state.gate_fetches -= 1;
    let waiting = state
        .origins
        .get_mut(origin)
        .map(|o| std::mem::take(&mut o.waiting))
        .unwrap_or_default();
    for host in waiting {
        state.make_ready(&host);
    }
    drop(state);
    shared.schedule.notify_one();
    shared.events.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxies_requests_name_beyond_the_cap_are_counted_together() {
        let mut pool = Pool::new(&["http://pool:1".to_string()]);
        for i in 0..MAX_OTHER_PROXIES + 50 {
            pool.entry(&format!("http://u{i}:p@gateway:8000"));
        }
        assert_eq!(pool.entries.len(), 1 + MAX_OTHER_PROXIES + 1);
        assert_eq!(pool.entries.last().unwrap().url, OTHER_PROXIES);
        let k = pool.entry("http://late:p@gateway:8000");
        assert_eq!(pool.entries[k].url, OTHER_PROXIES);
        assert_eq!(pool.entry("http://u3:p@gateway:8000"), 4, "still its own");
    }

    #[test]
    fn the_client_maps_keep_their_most_recently_used() {
        let mut lru = Lru::new(2);
        lru.insert("a", 1);
        lru.insert("b", 2);
        assert_eq!(lru.get("a"), Some(1));
        lru.insert("c", 3);
        assert_eq!(
            (lru.get("a"), lru.get("b"), lru.get("c")),
            (Some(1), None, Some(3))
        );
    }

    #[test]
    fn a_pool_exhausted_only_for_chrome_isnt_called_exhausted() {
        let mut pool = Pool::new(&[
            "socks5://ann:secret@10.0.0.1:1080".to_string(),
            "http://10.0.0.2:8080".to_string(),
        ]);
        let now = Instant::now();
        pool.entries[1].until = Some(now + Duration::from_secs(60));
        // The one Chrome can use is benched; the other is healthy.
        assert_eq!(
            pool.pick(now, None, chrome_can_use),
            Picked::Soonest("http://10.0.0.2:8080".into(), false)
        );
        assert!(!pool.warned_exhausted);
        // With every proxy benched, it is exhausted, and says so once.
        pool.entries[0].until = Some(now + Duration::from_secs(90));
        assert_eq!(
            pool.pick(now, None, |_| true),
            Picked::Soonest("http://10.0.0.2:8080".into(), true)
        );
        assert_eq!(
            pool.pick(now, None, |_| true),
            Picked::Soonest("http://10.0.0.2:8080".into(), false)
        );
    }

    #[test]
    fn chrome_is_given_no_socks_proxy_with_a_login() {
        assert!(!chrome_can_use("socks5://ann:secret@10.0.0.1:1080"));
        assert!(chrome_can_use("socks5://10.0.0.1:1080"));
        assert!(chrome_can_use("http://ann:secret@10.0.0.1:8080"));
        let pool = Pool::new(&[
            "socks5://ann:secret@10.0.0.1:1080".to_string(),
            "http://10.0.0.2:8080".to_string(),
        ]);
        let now = Instant::now();
        assert_eq!(pool.choose(now, chrome_can_use), Some((1, true)));
        let socks = Pool::new(&["socks5h://ann:secret@10.0.0.1:1080".to_string()]);
        assert_eq!(socks.choose(now, chrome_can_use), None);
    }

    #[test]
    fn throttle_moves_halfway_to_the_target_and_only_success_lowers_it() {
        let s = CrawlSettings {
            target_concurrency: 2.0,
            ..CrawlSettings::default()
        };
        let ms = Duration::from_millis;
        assert_eq!(adjust_delay(&s, ms(1000), ms(400), Some(200)), ms(600));
        assert_eq!(
            adjust_delay(&s, ms(1000), ms(400), Some(500)),
            ms(1000),
            "a failure can't lower it"
        );
        assert_eq!(
            adjust_delay(&s, ms(100), ms(4000), Some(404)),
            ms(1050),
            "but can raise it"
        );
        assert_eq!(adjust_delay(&s, ms(300), ms(10), Some(429)), ms(600));
        assert_eq!(
            adjust_delay(&s, ms(50_000), ms(200_000), Some(200)),
            ms(60_000),
            "clamped to the max"
        );
        assert_eq!(
            adjust_delay(&s, ms(100), ms(10), None),
            ms(100),
            "a network error can't lower it"
        );
    }

    #[tokio::test]
    async fn a_panic_in_guarded_work_becomes_the_fallback() {
        let value = guarded(async { panic!("parser bug") }, |why| {
            assert!(why.contains("panic"), "{why}");
            7
        })
        .await;
        assert_eq!(value, 7);
        assert_eq!(guarded(async { 1 }, |_| 2).await, 1);
    }

    #[test]
    fn queue_order_is_priority_then_arrival() {
        let q = |priority, seq| Queued {
            priority,
            seq,
            request: CrawlRequest {
                id: seq,
                url: String::new(),
                priority,
                headers: vec![],
                dont_filter: false,
                depth: 0,
                browser: false,
                outgoing: Outgoing::navigate(),
                retry_post: false,
                proxy: None,
            },
            host: String::new(),
            origin: String::new(),
            path: String::new(),
            hops: 0,
            attempts: 0,
            in_browser: false,
            keep: None,
            blocked: None,
        };
        let mut heap: BinaryHeap<Queued> =
            [q(0, 1), q(5, 2), q(0, 3), q(5, 4)].into_iter().collect();
        let order: Vec<u64> = std::iter::from_fn(|| heap.pop().map(|q| q.seq)).collect();
        assert_eq!(order, [2, 4, 1, 3]);
    }
}
