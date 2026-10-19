//! The cookie jar: RFC 6265bis storing and sending, as Chrome applies it
//! where the draft leaves room (SameSite, partitioned cookies, prefixes).
//!
//! Unlike wreq's jar, this one knows which host each host-only cookie
//! belongs to, so a jar can be read back whole, saved and loaded.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use url::Url;

pub use netweir_browser::Cookie;

/// What the jar needs to know of a request to decide which cookies it
/// carries and which its response may set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    /// The request goes to another site than the page it comes from (or,
    /// after redirects, than any page on the way).
    pub cross_site: bool,
    /// A top-level navigation: typing an address, following a link,
    /// submitting a form, and their redirects.
    pub navigation: bool,
    /// GET, HEAD, OPTIONS or TRACE.
    pub safe_method: bool,
    /// The site of the top-level page (`https://shop.example`), which
    /// partitioned cookies are kept under.
    pub top_level_site: Option<String>,
}

impl Context {
    /// Typing `url`'s address.
    pub fn typed(url: &Url) -> Context {
        Context {
            cross_site: false,
            navigation: true,
            safe_method: true,
            top_level_site: Some(site_of(url)),
        }
    }
}

/// A URL's site: its scheme and registrable domain (`https://example.co.uk`),
/// or its scheme and host when it's an address or has no registrable
/// domain.
pub fn site_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let registrable = match url.host() {
        Some(url::Host::Domain(_)) => psl::domain_str(&host).unwrap_or(&host).to_string(),
        _ => host.clone(),
    };
    format!("{}://{registrable}", url.scheme())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    fn parse(s: &str) -> Option<SameSite> {
        match s.to_ascii_lowercase().as_str() {
            "strict" => Some(SameSite::Strict),
            "lax" => Some(SameSite::Lax),
            "none" => Some(SameSite::None),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }
}

/// A cookie's Priority attribute, Chrome's own: which go first when a
/// site has too many.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Priority {
    Low,
    #[default]
    Medium,
    High,
}

impl Priority {
    /// From a Set-Cookie's attributes; Medium unless one says otherwise.
    fn of(attributes: &str) -> Priority {
        let mut priority = Priority::Medium;
        for attribute in attributes.split(';') {
            if let Some((k, v)) = attribute.split_once('=')
                && k.trim().eq_ignore_ascii_case("priority")
            {
                priority = match v.trim().to_ascii_lowercase().as_str() {
                    "low" => Priority::Low,
                    "high" => Priority::High,
                    _ => Priority::Medium,
                };
            }
        }
        priority
    }
}

/// One cookie as the jar keeps it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Stored {
    name: String,
    value: String,
    /// Canonical, without a leading dot.
    domain: String,
    host_only: bool,
    path: String,
    /// Seconds since the epoch; None for a session cookie.
    expires: Option<f64>,
    secure: bool,
    http_only: bool,
    same_site: Option<SameSite>,
    partition: Option<String>,
    /// When it was made, in seconds since the epoch, for "Lax allowing
    /// unsafe".
    created: f64,
    /// When it was last sent or set, which decides what goes first when a
    /// site or the jar holds too many.
    #[serde(default)]
    accessed: f64,
    #[serde(default)]
    priority: Priority,
    /// Creation order, which orders cookies of equal path length.
    seq: u64,
}

impl Stored {
    fn same_cookie(&self, other: &Stored) -> bool {
        self.name == other.name
            && self.domain == other.domain
            && self.host_only == other.host_only
            && self.path == other.path
            && self.partition == other.partition
    }

    fn expired(&self, now: f64) -> bool {
        self.expires.is_some_and(|e| e <= now)
    }

    fn pair(&self) -> String {
        // A cookie without a name is sent as its value alone, as browsers
        // send it.
        if self.name.is_empty() {
            self.value.clone()
        } else {
            format!("{}={}", self.name, self.value)
        }
    }

    fn exported(&self) -> Cookie {
        Cookie {
            name: self.name.clone(),
            value: self.value.clone(),
            domain: if self.host_only {
                self.domain.clone()
            } else {
                format!(".{}", self.domain)
            },
            path: self.path.clone(),
            expires: self.expires.unwrap_or(-1.0),
            http_only: self.http_only,
            secure: self.secure,
            same_site: self.same_site.map(|s| s.name().to_string()),
            partition_key: self.partition.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct Store {
    /// By site, as Chrome counts cookies: the registrable domain of each
    /// cookie's domain.
    by_key: HashMap<String, Vec<Stored>>,
    /// How many cookies there are, expired ones among them.
    count: usize,
    /// No cookie was last used before this, as of the last purge: until a
    /// month after it, a jar over its limit has nothing a purge would
    /// drop, so it isn't looked through on every change.
    earliest: f64,
    seq: u64,
    /// Bumped by every change, for whoever saves the jar.
    changes: u64,
}

/// Drops a site's cookies from over `SITE_MAX` to 150 as Chrome's
/// CookieMonster does: in rounds by priority, insecure before secure
/// (Low insecure, Low, Medium insecure, High insecure, Medium, High), each
/// least recently used first, and each round keeping the most recently
/// used of its priority (30 Low, 50 Medium, 70 High).
fn purge_site(list: &mut Vec<Stored>) {
    list.sort_by(|a, b| {
        (a.accessed, a.seq)
            .partial_cmp(&(b.accessed, b.seq))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let quota = |p: Priority| match p {
        Priority::Low => SITE_QUOTA_LOW,
        Priority::Medium => SITE_QUOTA_MEDIUM,
        Priority::High => SITE_MAX - SITE_PURGE - SITE_QUOTA_LOW - SITE_QUOTA_MEDIUM,
    };
    let mut goal = list.len() - (SITE_MAX - SITE_PURGE);
    let rounds = [
        (Priority::Low, true),
        (Priority::Low, false),
        (Priority::Medium, true),
        (Priority::High, true),
        (Priority::Medium, false),
        (Priority::High, false),
    ];
    for (priority, protect_secure) in rounds {
        if goal == 0 {
            break;
        }
        let count = list.iter().filter(|c| c.priority == priority).count();
        if count <= quota(priority) {
            continue;
        }
        let mut may_go = if protect_secure {
            let secure = list
                .iter()
                .filter(|c| c.priority == priority && c.secure)
                .count();
            count - secure.max(quota(priority))
        } else {
            count - quota(priority)
        };
        let mut i = 0;
        while goal > 0 && may_go > 0 && i < list.len() {
            let c = &list[i];
            if c.priority == priority && !(protect_secure && c.secure) {
                list.remove(i);
                goal -= 1;
                may_go -= 1;
            } else {
                i += 1;
            }
        }
    }
}

/// The site a cookie of `domain` counts toward: its registrable domain,
/// or `domain` itself for an address or a name with none.
fn key_of(domain: &str) -> &str {
    if is_ip(domain) {
        return domain;
    }
    psl::domain_str(domain).unwrap_or(domain)
}

/// The site a host or a cookie's domain (a leading dot or not) belongs to,
/// as the jar keeps cookies by it; None for something that isn't a host.
pub fn site_key(host: &str) -> Option<String> {
    canonical(host.trim_start_matches('.')).map(|h| key_of(&h).to_string())
}

/// Chrome's limits: a site keeps 180 cookies, and going over drops it to
/// 150; the jar keeps 3300, and going over drops those unsent for a
/// month until it holds 3000.
const SITE_MAX: usize = 180;
const SITE_PURGE: usize = 30;
/// How many of each priority's most recently used a site's purge keeps.
const SITE_QUOTA_LOW: usize = 30;
const SITE_QUOTA_MEDIUM: usize = 50;
const JAR_MAX: usize = 3300;
const JAR_PURGE: usize = 300;
const SAFE_FROM_GLOBAL_PURGE: f64 = 30.0 * 24.0 * 3600.0;

impl Store {
    /// Puts `cookie` in, replacing the same cookie; one with the same
    /// value keeps its place in the order, as in Chrome. Then drops what
    /// Chrome would: the site's expired cookies, and what's over the
    /// limits.
    fn put(&mut self, mut cookie: Stored, now: f64) {
        let key = key_of(&cookie.domain).to_string();
        cookie.accessed = now;
        let list = self.by_key.entry(key.clone()).or_default();
        match list.iter_mut().find(|c| c.same_cookie(&cookie)) {
            Some(old) => {
                if old.value == cookie.value {
                    cookie.seq = old.seq;
                    cookie.created = old.created;
                } else {
                    self.seq += 1;
                    cookie.seq = self.seq;
                }
                *old = cookie;
            }
            None => {
                self.seq += 1;
                cookie.seq = self.seq;
                list.push(cookie);
                self.count += 1;
            }
        }
        let before = list.len();
        list.retain(|c| !c.expired(now));
        if list.len() > SITE_MAX {
            purge_site(list);
        }
        self.count -= before - list.len();
        if self.count > JAR_MAX && self.earliest < now - SAFE_FROM_GLOBAL_PURGE {
            self.purge(now);
        }
        self.changes += 1;
    }

    /// Chrome's purge of a jar over its limit: expired cookies, then, of
    /// the least recently used it would need to drop, those unsent for a
    /// month, insecure ones before secure ones.
    fn purge(&mut self, now: f64) {
        self.drop_where(|c| c.expired(now));
        if self.count <= JAR_MAX {
            self.earliest = 0.0;
            return;
        }
        let mut goal = self.count - (JAR_MAX - JAR_PURGE);
        let safe = now - SAFE_FROM_GLOBAL_PURGE;
        let mut gone = std::collections::HashSet::new();
        for secure in [false, true] {
            let mut used: Vec<(f64, u64)> = self
                .by_key
                .values()
                .flatten()
                .filter(|c| c.secure == secure)
                .map(|c| (c.accessed, c.seq))
                .collect();
            used.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let within = goal.min(used.len());
            let old = used[..within].partition_point(|(accessed, _)| *accessed < safe);
            gone.extend(used[..old].iter().map(|(_, seq)| *seq));
            goal -= old;
            if goal == 0 {
                break;
            }
        }
        if !gone.is_empty() {
            self.drop_where(|c| gone.contains(&c.seq));
        }
        self.earliest = self
            .by_key
            .values()
            .flatten()
            .map(|c| c.accessed)
            .fold(now, f64::min);
    }

    fn delete(&mut self, cookie: &Stored) {
        if let Some(list) = self.by_key.get_mut(key_of(&cookie.domain)) {
            let before = list.len();
            list.retain(|c| !c.same_cookie(cookie));
            if list.len() != before {
                self.count -= before - list.len();
                self.changes += 1;
            }
        }
    }

    /// Drops what `gone` picks; how many went.
    fn drop_where(&mut self, mut gone: impl FnMut(&Stored) -> bool) -> usize {
        let mut n = 0;
        for list in self.by_key.values_mut() {
            let before = list.len();
            list.retain(|c| !gone(c));
            n += before - list.len();
        }
        self.by_key.retain(|_, list| !list.is_empty());
        self.count -= n;
        if n > 0 {
            self.changes += 1;
        }
        n
    }

    /// Whether an http response would replace a Secure cookie (RFC
    /// 6265bis 5.7, step 16).
    fn overlays_secure(&self, name: &str, domain: &str, path: &str, now: f64) -> bool {
        self.by_key.get(key_of(domain)).is_some_and(|list| {
            list.iter().any(|c| {
                (domain_match(&c.domain, domain) || domain_match(domain, &c.domain))
                    && c.secure
                    && c.name == name
                    && !c.expired(now)
                    && path_match(path, &c.path)
            })
        })
    }
}

/// Cookies longer than this, name and value together, are refused, as in
/// Chrome.
const MAX_SIZE: usize = 4096;

/// The furthest ahead a cookie may expire: 400 days (RFC 6265bis).
const MAX_AGE: f64 = 400.0 * 24.0 * 3600.0;

/// How long a cookie without SameSite is still sent on a cross-site
/// top-level POST: Chrome's "Lax allowing unsafe".
const LAX_ALLOWING_UNSAFE: f64 = 120.0;

/// Cookies for one visitor. Cheap to share behind an `Arc`.
#[derive(Debug, Default)]
pub struct Jar {
    store: Mutex<Store>,
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// A host as the jar keys it: lower case, IDNA, IPv6 in brackets. None
/// for something that isn't a host.
fn canonical(host: &str) -> Option<String> {
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    url::Host::parse(host).ok().map(|h| h.to_string())
}

fn is_ip(host: &str) -> bool {
    host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// RFC 6265 5.1.3: `host` is `domain` or a subdomain of it.
fn domain_match(host: &str, domain: &str) -> bool {
    host == domain
        || (!is_ip(host)
            && host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

/// RFC 6265 5.1.4.
fn path_match(request: &str, cookie: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request.as_bytes().get(cookie.len()) == Some(&b'/')))
}

/// RFC 6265 5.1.4: a URL's directory.
fn default_path(url: &Url) -> String {
    let path = url.path();
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// Gives `cookie` `url`'s host as its own when it has no domain, and
/// `url`'s directory when it has no path: a request's own cookies, tied
/// once to the URL asked for, never to where a redirect leads.
pub fn scope(cookie: &mut Cookie, url: &Url) {
    if cookie.domain.is_empty()
        && let Some(host) = url.host_str().and_then(canonical)
    {
        cookie.domain = host;
    }
    if !cookie.path.starts_with('/') {
        cookie.path = default_path(url);
    }
}

/// Whether `url` may set and be sent `Secure` cookies: https, or, as
/// Chrome counts it secure, localhost and loopback addresses.
fn trustworthy(url: &Url) -> bool {
    url.scheme() == "https"
        || match url.host() {
            Some(url::Host::Domain(d)) => {
                let d = d.trim_end_matches('.').to_ascii_lowercase();
                d == "localhost" || d.ends_with(".localhost")
            }
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        }
}

fn is_public_suffix(domain: &str) -> bool {
    !is_ip(domain) && psl::suffix_str(domain) == Some(domain)
}

impl Jar {
    pub fn new() -> Jar {
        Jar::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores what a Set-Cookie value from `url` sets, if the rules let
    /// it; whether they did.
    pub fn set_cookie(&self, header: &str, url: &Url, context: &Context) -> bool {
        // A pair without a name ("token", or "=token") is a cookie with
        // none (RFC 6265bis 5.6), which the cookie crate won't parse: it
        // reads the attributes after a stand-in name.
        let (pair, attributes) = header.split_once(';').unwrap_or((header, ""));
        let nameless = match pair.split_once('=') {
            None => Some(pair.trim()),
            Some((name, value)) if name.trim().is_empty() => Some(value.trim()),
            Some(_) => None,
        };
        let parsed = match nameless {
            None => cookie::Cookie::parse(header).map(cookie::Cookie::into_owned),
            Some(value) => cookie::Cookie::parse(format!("n={value};{attributes}")),
        };
        let Ok(parsed) = parsed else {
            return false;
        };
        let Some(host) = url.host_str().and_then(canonical) else {
            return false;
        };
        let now = now();
        let trusted = trustworthy(url);
        let (name, value) = match nameless {
            Some(value) => (String::new(), value.to_string()),
            None => (parsed.name().to_string(), parsed.value().to_string()),
        };
        let secure = parsed.secure().unwrap_or(false);
        if secure && !trusted {
            return false;
        }
        // The Domain attribute; an empty one is no attribute at all.
        let attribute = parsed.domain().filter(|d| !d.is_empty());
        let (domain, host_only) = match attribute {
            None => (host.clone(), true),
            Some(d) => {
                let Some(d) = canonical(d) else {
                    return false;
                };
                if is_ip(&host) || is_ip(&d) {
                    // An address names itself alone: a Domain of the
                    // host's own address is a host-only cookie, any other
                    // refused, as in Chrome.
                    if d != host {
                        return false;
                    }
                    (host.clone(), true)
                } else if is_public_suffix(&d) {
                    // A site that is a public suffix may still set a cookie
                    // for itself alone.
                    if d != host {
                        return false;
                    }
                    (host.clone(), true)
                } else if !domain_match(&host, &d) {
                    return false;
                } else {
                    (d, false)
                }
            }
        };
        let path = parsed
            .path()
            .filter(|p| p.starts_with('/'))
            .map_or_else(|| default_path(url), str::to_string);
        // __Host- needs Path=/ said, and no Domain attribute but the host's
        // own address (Chrome's HasValidHostPrefixAttributes); `valid`
        // checks the rest.
        if name.to_ascii_lowercase().starts_with("__host-")
            && (parsed.path() != Some("/") || (attribute.is_some() && !is_ip(&host)))
        {
            return false;
        }
        let same_site = parsed
            .same_site()
            .and_then(|s| SameSite::parse(&s.to_string()));
        // A cross-site subresource can't set a cookie that wouldn't be sent
        // back to it (unspecified counts as Lax).
        if same_site != Some(SameSite::None) && context.cross_site && !context.navigation {
            return false;
        }
        let partition = parsed.partitioned().unwrap_or(false).then(|| {
            context
                .top_level_site
                .clone()
                .unwrap_or_else(|| site_of(url))
        });
        // Max-Age wins over Expires; either is kept as a deadline.
        let expires = match parsed.max_age() {
            Some(age) => Some(now + age.as_seconds_f64()),
            None => parsed
                .expires_datetime()
                .map(|t| t.unix_timestamp_nanos() as f64 / 1e9),
        }
        .map(|e| e.min(now + MAX_AGE));
        let cookie = Stored {
            name,
            value,
            domain,
            host_only,
            path,
            expires,
            secure,
            http_only: parsed.http_only().unwrap_or(false),
            same_site,
            partition,
            created: now,
            accessed: now,
            priority: Priority::of(attributes),
            seq: 0,
        };
        if valid(&cookie).is_err() {
            return false;
        }
        let mut store = self.lock();
        if !trusted && store.overlays_secure(&cookie.name, &cookie.domain, &cookie.path, now) {
            return false;
        }
        if cookie.expired(now) {
            store.delete(&cookie);
        } else {
            store.put(cookie, now);
        }
        true
    }

    /// Adds a cookie given whole, as a browser or a caller holds it. A
    /// domain with a leading dot is the domain's and its subdomains';
    /// without one, the host's alone. With `url`, an empty domain is the
    /// URL's host's and an empty path the URL's directory; without, both
    /// are needed. One already expired removes the cookie it matches. One
    /// a Set-Cookie couldn't set either is refused, saying why (never with
    /// its value): so is one Chrome would refuse, which would fail every
    /// page the jar's cookies are handed to.
    pub fn add(&self, cookie: &Cookie, url: Option<&Url>) -> Result<(), String> {
        let now = now();
        let stored = Jar::stored(cookie, url, now)?;
        let mut store = self.lock();
        if stored.expired(now) {
            store.delete(&stored);
        } else {
            store.put(stored, now);
        }
        Ok(())
    }

    /// Whether `add` would take `cookie`, and if not, why.
    pub fn check(cookie: &Cookie, url: Option<&Url>) -> Result<(), String> {
        Jar::stored(cookie, url, now()).map(|_| ())
    }

    fn stored(cookie: &Cookie, url: Option<&Url>, now: f64) -> Result<Stored, String> {
        let (domain, host_only) = match (cookie.domain.strip_prefix('.'), url) {
            // An address is its own alone, dot or no dot, as in Chrome.
            (Some(d), _) => {
                let d = canonical(d);
                let ip = d.as_deref().is_some_and(is_ip);
                (d, ip)
            }
            (None, _) if !cookie.domain.is_empty() => (canonical(&cookie.domain), true),
            (None, Some(url)) => (url.host_str().and_then(canonical), true),
            (None, None) => return Err(format!("the cookie {:?} needs a domain", cookie.name)),
        };
        let domain = domain.ok_or_else(|| {
            format!(
                "the cookie {:?} has a domain that isn't a host: {:?}",
                cookie.name, cookie.domain
            )
        })?;
        if !host_only && is_public_suffix(&domain) {
            return Err(format!(
                "the cookie {:?} is for {:?}, a public suffix: it would go to every site under it",
                cookie.name, cookie.domain
            ));
        }
        let path = match (cookie.path.starts_with('/'), url) {
            (true, _) => cookie.path.clone(),
            (false, Some(url)) => default_path(url),
            (false, None) => "/".into(),
        };
        let stored = Stored {
            name: cookie.name.clone(),
            value: cookie.value.clone(),
            domain,
            host_only,
            path,
            expires: (cookie.expires >= 0.0).then_some(cookie.expires),
            secure: cookie.secure,
            http_only: cookie.http_only,
            same_site: cookie.same_site.as_deref().and_then(SameSite::parse),
            partition: cookie.partition_key.clone(),
            created: now,
            accessed: now,
            priority: Priority::Medium,
            seq: 0,
        };
        valid(&stored)?;
        Ok(stored)
    }

    /// The cookies of `host`'s site (its registrable domain, subdomains
    /// and all), oldest first: those a page there could be sent, without
    /// looking through every other site's.
    pub fn of_site(&self, host: &str) -> Vec<Cookie> {
        let Some(key) = site_key(host) else {
            return Vec::new();
        };
        let now = now();
        let store = self.lock();
        let mut found: Vec<&Stored> = store
            .by_key
            .get(&key)
            .into_iter()
            .flatten()
            .filter(|c| !c.expired(now))
            .collect();
        found.sort_by_key(|c| c.seq);
        found.into_iter().map(Stored::exported).collect()
    }

    /// Every cookie, oldest first.
    pub fn all(&self) -> Vec<Cookie> {
        let now = now();
        let store = self.lock();
        let mut all: Vec<&Stored> = store
            .by_key
            .values()
            .flatten()
            .filter(|c| !c.expired(now))
            .collect();
        all.sort_by_key(|c| c.seq);
        all.into_iter().map(Stored::exported).collect()
    }

    pub fn len(&self) -> usize {
        let now = now();
        self.lock()
            .by_key
            .values()
            .flatten()
            .filter(|c| !c.expired(now))
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The cookies named `name`, of `domain` (a host's own and the
    /// domain's, or with a leading dot only the domain's) and `path` when
    /// given, oldest first.
    pub fn find(&self, name: &str, domain: Option<&str>, path: Option<&str>) -> Vec<Cookie> {
        let now = now();
        let wanted = Wanted::new(domain);
        let store = self.lock();
        let mut found: Vec<&Stored> = store
            .by_key
            .values()
            .flatten()
            .filter(|c| {
                c.name == name
                    && !c.expired(now)
                    && wanted.takes(c)
                    && path.is_none_or(|p| p == c.path)
            })
            .collect();
        found.sort_by_key(|c| c.seq);
        found.into_iter().map(Stored::exported).collect()
    }

    /// Removes the cookies `find` would find; how many.
    pub fn remove(&self, name: &str, domain: Option<&str>, path: Option<&str>) -> usize {
        let wanted = Wanted::new(domain);
        self.lock()
            .drop_where(|c| c.name == name && wanted.takes(c) && path.is_none_or(|p| p == c.path))
    }

    /// Removes every cookie, or with `domain`, those of the domain and its
    /// subdomains; how many.
    pub fn clear(&self, domain: Option<&str>) -> usize {
        let domain = domain.and_then(|d| canonical(d.trim_start_matches('.')));
        self.lock()
            .drop_where(|c| domain.as_ref().is_none_or(|d| domain_match(&c.domain, d)))
    }

    /// Removes every cookie that would be sent to `host`, its parent
    /// domains' among them, as a new visitor would start without them.
    pub fn forget(&self, host: &str) {
        let Some(host) = canonical(host) else {
            return;
        };
        self.lock().drop_where(|c| {
            if c.host_only {
                c.domain == host
            } else {
                domain_match(&host, &c.domain)
            }
        });
    }

    /// The cookie pairs (`name=value`) a request to `url` carries, in the
    /// order the browser sends them: longest path first, then oldest first,
    /// or with `newest_first` (Safari) newest first.
    pub fn header(&self, url: &Url, context: &Context, newest_first: bool) -> Vec<String> {
        let Some(host) = url.host_str().and_then(canonical) else {
            return Vec::new();
        };
        let now = now();
        let trusted = trustworthy(url);
        let path = url.path();
        let mut store = self.lock();
        let Some(list) = store.by_key.get_mut(key_of(&host)) else {
            return Vec::new();
        };
        let mut sent: Vec<&mut Stored> = list
            .iter_mut()
            .filter(|c| {
                (if c.host_only {
                    c.domain == host
                } else {
                    domain_match(&host, &c.domain)
                }) && !c.expired(now)
                    && path_match(path, &c.path)
                    && (!c.secure || trusted)
                    && c.partition
                        .as_ref()
                        .is_none_or(|p| context.top_level_site.as_ref() == Some(p))
                    && same_site_allows(c, context, now)
            })
            .collect();
        sent.sort_by(|a, b| {
            b.path.len().cmp(&a.path.len()).then(if newest_first {
                b.seq.cmp(&a.seq)
            } else {
                a.seq.cmp(&b.seq)
            })
        });
        sent.into_iter()
            .map(|c| {
                c.accessed = now;
                c.pair()
            })
            .collect()
    }

    /// A count that changes whenever the jar does.
    pub fn changes(&self) -> u64 {
        self.lock().changes
    }

    /// The jar as JSON, everything kept, for `restore`. Expired cookies
    /// are dropped from the jar on the way. Written after the jar is let
    /// go, so requests aren't held up by a big one.
    pub fn snapshot(&self) -> String {
        let now = now();
        let mut store = self.lock();
        // Nothing anyone can see changes, so this isn't a change to save.
        let changes = store.changes;
        store.drop_where(|c| c.expired(now));
        store.changes = changes;
        let mut all: Vec<Stored> = store.by_key.values().flatten().cloned().collect();
        let seq = store.seq;
        drop(store);
        all.sort_by_key(|c| c.seq);
        serde_json::json!({"seq": seq, "cookies": all}).to_string()
    }

    /// Replaces what the jar holds with a `snapshot`.
    pub fn restore(&self, snapshot: &str) -> Result<(), String> {
        #[derive(serde::Deserialize)]
        struct Snapshot {
            seq: u64,
            cookies: Vec<Stored>,
        }
        let read: Snapshot = serde_json::from_str(snapshot)
            .map_err(|e| format!("a cookie jar that won't read: {e}"))?;
        let mut store = self.lock();
        store.by_key.clear();
        store.count = read.cookies.len();
        store.earliest = 0.0;
        for mut c in read.cookies {
            if c.accessed == 0.0 {
                c.accessed = c.created;
            }
            store
                .by_key
                .entry(key_of(&c.domain).to_string())
                .or_default()
                .push(c);
        }
        store.seq = read.seq;
        store.changes += 1;
        Ok(())
    }
}

/// The rules a cookie keeps however it comes, from a Set-Cookie or given
/// whole, as Chrome keeps them: what's wrong, if anything, never with
/// the cookie's value in it.
fn valid(c: &Stored) -> Result<(), String> {
    let name = &c.name;
    let refuse = |why: &str| Err(format!("the cookie {name:?} {why}"));
    if name.is_empty() && c.value.is_empty() {
        return Err("a cookie needs a name or a value".into());
    }
    if name.len() + c.value.len() > MAX_SIZE {
        return refuse("is too large");
    }
    // Control characters, a tab among them, and what would end the pair.
    let broken = |s: &str| s.chars().any(|ch| ch.is_ascii_control() || ch == ';');
    if broken(name) || name.contains('=') {
        return refuse("has a name with '=', ';' or a control character in it");
    }
    if broken(&c.value) {
        return refuse("has a value with ';' or a control character in it");
    }
    if c.path.chars().any(|ch| ch.is_ascii_control()) {
        return refuse("has a path with a control character in it");
    }
    let lower = c.value.to_ascii_lowercase();
    if name.is_empty() {
        // Sent as its value alone, it would read as another cookie, or as
        // a prefixed one it isn't.
        if c.value.contains('=') {
            return Err("a cookie without a name can't have '=' in its value".into());
        }
        if ["__secure-", "__host-", "__http-"]
            .iter()
            .any(|p| lower.starts_with(p))
        {
            return Err(
                "a cookie without a name can't have a value starting __Secure-, \
                        __Host- or __Http-"
                    .into(),
            );
        }
    }
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("__secure-") && !c.secure {
        return refuse("needs to be secure, as its name starts __Secure-");
    }
    // __Http- and __Host-Http- (Chrome 155 holds to them too) also need
    // HttpOnly: no page's script may have set them.
    let http = lower.starts_with("__http-") || lower.starts_with("__host-http-");
    if http && (!c.secure || !c.http_only) {
        return refuse(
            "needs to be secure and HttpOnly, as its name starts __Http- or __Host-Http-",
        );
    }
    if lower.starts_with("__host-") && (!c.secure || !c.host_only || c.path != "/") {
        return refuse(
            "needs to be secure, its host's alone and for the path /, as its name starts __Host-",
        );
    }
    if c.same_site == Some(SameSite::None) && !c.secure {
        return refuse("needs to be secure to be SameSite=None");
    }
    if c.partition.is_some() && !c.secure {
        return refuse("needs to be secure to be partitioned");
    }
    Ok(())
}

/// Whether SameSite lets `cookie` go with a request in `context`, as
/// Chrome decides it.
fn same_site_allows(cookie: &Stored, context: &Context, now: f64) -> bool {
    if !context.cross_site {
        return true;
    }
    match cookie.same_site {
        Some(SameSite::None) => true,
        Some(SameSite::Strict) => false,
        Some(SameSite::Lax) => context.navigation && context.safe_method,
        // Lax by default, and for its first two minutes on a top-level
        // POST too.
        None => {
            context.navigation
                && (context.safe_method || now - cookie.created < LAX_ALLOWING_UNSAFE)
        }
    }
}

/// Which cookies a domain given to `find`, `remove` or `clear` stands for.
struct Wanted {
    domain: Option<String>,
    /// Given with a leading dot: only the domain's own cookies.
    dotted: bool,
}

impl Wanted {
    fn new(domain: Option<&str>) -> Wanted {
        Wanted {
            domain: domain.map(|d| {
                canonical(d.trim_start_matches('.'))
                    .unwrap_or_else(|| d.trim_start_matches('.').to_ascii_lowercase())
            }),
            dotted: domain.is_some_and(|d| d.starts_with('.')),
        }
    }

    fn takes(&self, c: &Stored) -> bool {
        match &self.domain {
            None => true,
            Some(d) => c.domain == *d && (!self.dotted || !c.host_only),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn set(jar: &Jar, at: &str, header: &str) -> bool {
        jar.set_cookie(header, &url(at), &Context::typed(&url(at)))
    }

    fn sent(jar: &Jar, to: &str) -> Vec<String> {
        jar.header(&url(to), &Context::typed(&url(to)), false)
    }

    /// A request from a page on `from` to `to`.
    fn from(from: &str, to: &str, navigation: bool, method: &str) -> Context {
        let (a, b) = (url(from), url(to));
        Context {
            cross_site: site_of(&a) != site_of(&b),
            navigation,
            safe_method: method == "GET",
            top_level_site: Some(site_of(if navigation { &b } else { &a })),
        }
    }

    #[test]
    fn host_only_cookies_stay_on_their_host_and_domain_cookies_reach_subdomains() {
        let jar = Jar::new();
        assert!(set(&jar, "https://shop.example/", "own=1"));
        assert!(set(
            &jar,
            "https://shop.example/",
            "wide=2; Domain=shop.example"
        ));
        assert_eq!(sent(&jar, "https://shop.example/"), ["own=1", "wide=2"]);
        assert_eq!(sent(&jar, "https://api.shop.example/"), ["wide=2"]);
        assert!(sent(&jar, "https://other.example/").is_empty());
        assert!(sent(&jar, "https://notshop.example/").is_empty());
    }

    #[test]
    fn a_host_only_and_a_domain_cookie_of_one_name_are_two_cookies() {
        let jar = Jar::new();
        set(&jar, "https://shop.example/", "id=host");
        set(
            &jar,
            "https://shop.example/",
            "id=domain; Domain=shop.example",
        );
        assert_eq!(
            sent(&jar, "https://shop.example/"),
            ["id=host", "id=domain"]
        );
        assert_eq!(jar.len(), 2);
    }

    #[test]
    fn public_suffixes_and_other_sites_cant_be_given_cookies() {
        let jar = Jar::new();
        assert!(!set(&jar, "https://shop.example.com/", "a=1; Domain=com"));
        assert!(!set(&jar, "https://me.github.io/", "a=1; Domain=github.io"));
        assert!(!set(
            &jar,
            "https://shop.example/",
            "a=1; Domain=other.example"
        ));
        assert!(!set(
            &jar,
            "https://shop.example/",
            "a=1; Domain=api.shop.example"
        ));
        // A site that is itself a public suffix may set one for itself.
        assert!(set(&jar, "https://github.io/", "a=1; Domain=github.io"));
        assert_eq!(sent(&jar, "https://github.io/"), ["a=1"]);
        assert!(sent(&jar, "https://me.github.io/").is_empty());
    }

    #[test]
    fn paths_default_to_the_directory_and_match_on_segments() {
        let jar = Jar::new();
        set(&jar, "https://e.com/docs/page", "dir=1");
        set(&jar, "https://e.com/", "deep=2; Path=/docs/api");
        assert_eq!(jar.find("dir", None, None)[0].path, "/docs");
        assert_eq!(sent(&jar, "https://e.com/docs/api/x"), ["deep=2", "dir=1"]);
        assert_eq!(sent(&jar, "https://e.com/docs"), ["dir=1"]);
        assert!(sent(&jar, "https://e.com/docsx").is_empty());
    }

    #[test]
    fn secure_cookies_go_only_over_https_and_http_cant_set_or_replace_them() {
        let jar = Jar::new();
        assert!(!set(&jar, "http://e.com/", "s=1; Secure"));
        assert!(set(&jar, "https://e.com/", "s=1; Secure"));
        assert!(sent(&jar, "http://e.com/").is_empty());
        assert!(!set(&jar, "http://e.com/", "s=2"));
        assert_eq!(sent(&jar, "https://e.com/"), ["s=1"]);
    }

    #[test]
    fn prefixed_names_keep_their_promises() {
        let jar = Jar::new();
        assert!(!set(&jar, "https://e.com/", "__Secure-a=1"));
        assert!(set(&jar, "https://e.com/", "__Secure-a=1; Secure"));
        assert!(!set(
            &jar,
            "https://e.com/",
            "__Host-b=1; Secure; Domain=e.com; Path=/"
        ));
        assert!(!set(&jar, "https://e.com/x/y", "__Host-b=1; Secure"));
        // Path=/ must be said, as in Chrome, even where it's the default.
        assert!(!set(&jar, "https://e.com/", "__Host-b=1; Secure"));
        assert!(set(&jar, "https://e.com/", "__Host-b=1; Secure; Path=/"));
        assert!(!set(&jar, "https://e.com/", "c=1; SameSite=None"));
    }

    #[test]
    fn max_age_wins_and_a_past_expiry_deletes() {
        let jar = Jar::new();
        set(
            &jar,
            "https://e.com/",
            "a=1; Max-Age=60; Expires=Thu, 01 Jan 1970 00:00:01 GMT",
        );
        let a = &jar.find("a", None, None)[0];
        assert!((a.expires - now() - 60.0).abs() < 5.0);
        set(&jar, "https://e.com/", "a=1; Max-Age=0");
        assert!(jar.is_empty());
        set(
            &jar,
            "https://e.com/",
            "b=1; Expires=Fri, 01 Jan 2100 00:00:00 GMT",
        );
        // Capped at 400 days.
        let b = &jar.find("b", None, None)[0];
        assert!(b.expires <= now() + MAX_AGE + 1.0);
        set(
            &jar,
            "https://e.com/",
            "b=1; Expires=Thu, 01 Jan 1970 00:00:01 GMT",
        );
        assert!(jar.is_empty());
    }

    #[test]
    fn order_is_longest_path_then_oldest_or_for_safari_newest() {
        let jar = Jar::new();
        set(&jar, "https://e.com/", "first=1");
        set(&jar, "https://e.com/", "deep=1; Path=/a");
        set(&jar, "https://e.com/", "second=1");
        assert_eq!(
            sent(&jar, "https://e.com/a/b"),
            ["deep=1", "first=1", "second=1"]
        );
        let u = url("https://e.com/a/b");
        assert_eq!(
            jar.header(&u, &Context::typed(&u), true),
            ["deep=1", "second=1", "first=1"]
        );
        // A new value is a newer cookie; the same value keeps its place.
        set(&jar, "https://e.com/", "first=1");
        assert_eq!(sent(&jar, "https://e.com/"), ["first=1", "second=1"]);
        set(&jar, "https://e.com/", "first=2");
        assert_eq!(sent(&jar, "https://e.com/"), ["second=1", "first=2"]);
    }

    #[test]
    fn same_site_cookies_stay_off_cross_site_requests_as_in_chrome() {
        let jar = Jar::new();
        for c in [
            "strict=1; SameSite=Strict",
            "lax=1; SameSite=Lax",
            "plain=1",
        ] {
            set(&jar, "https://shop.example/", c);
        }
        set(
            &jar,
            "https://shop.example/",
            "none=1; SameSite=None; Secure",
        );
        let to = url("https://shop.example/");
        let all = ["strict=1", "lax=1", "plain=1", "none=1"];
        // From the same site (another subdomain), everything.
        let same = from(
            "https://www.shop.example/",
            "https://shop.example/",
            false,
            "POST",
        );
        assert_eq!(jar.header(&to, &same, false), all);
        // A link from elsewhere: no Strict.
        let link = from(
            "https://elsewhere.example/",
            "https://shop.example/",
            true,
            "GET",
        );
        assert_eq!(
            jar.header(&to, &link, false),
            ["lax=1", "plain=1", "none=1"]
        );
        // A script's request from elsewhere: only None.
        let script = from(
            "https://elsewhere.example/",
            "https://shop.example/",
            false,
            "GET",
        );
        assert_eq!(jar.header(&to, &script, false), ["none=1"]);
        // A form POST from elsewhere: None, and a fresh cookie without
        // SameSite (Lax allowing unsafe).
        let post = from(
            "https://elsewhere.example/",
            "https://shop.example/",
            true,
            "POST",
        );
        assert_eq!(jar.header(&to, &post, false), ["plain=1", "none=1"]);
        // A cross-site script's response can't set a Lax cookie.
        assert!(!jar.set_cookie("x=1", &to, &script));
        assert!(jar.set_cookie("x=1; SameSite=None; Secure", &to, &script));
    }

    #[test]
    fn partitioned_cookies_are_kept_under_their_top_level_site() {
        let jar = Jar::new();
        let widget = url("https://widget.example/api");
        let on_a = from(
            "https://a.example/",
            "https://widget.example/api",
            false,
            "GET",
        );
        let on_b = from(
            "https://b.example/",
            "https://widget.example/api",
            false,
            "GET",
        );
        assert!(!jar.set_cookie("p=1; Partitioned; SameSite=None", &widget, &on_a));
        assert!(jar.set_cookie("p=1; Partitioned; SameSite=None; Secure", &widget, &on_a));
        assert_eq!(jar.header(&widget, &on_a, false), ["p=1"]);
        assert!(jar.header(&widget, &on_b, false).is_empty());
        assert_eq!(
            jar.all()[0].partition_key.as_deref(),
            Some("https://a.example")
        );
    }

    #[test]
    fn exported_cookies_import_with_their_scope() {
        let jar = Jar::new();
        set(
            &jar,
            "https://shop.example/a/b",
            "own=1; HttpOnly; SameSite=Lax",
        );
        set(
            &jar,
            "https://shop.example/",
            "wide=2; Domain=shop.example; Max-Age=600",
        );
        let all = jar.all();
        assert_eq!(all[0].domain, "shop.example");
        assert_eq!(all[0].path, "/a");
        assert!(all[0].http_only);
        assert_eq!(all[0].same_site.as_deref(), Some("Lax"));
        assert_eq!(all[0].expires, -1.0);
        assert_eq!(all[1].domain, ".shop.example");
        let copy = Jar::new();
        for c in &all {
            copy.add(c, None).unwrap();
        }
        assert_eq!(copy.all(), all);
        assert_eq!(sent(&copy, "https://api.shop.example/a"), ["wide=2"]);
    }

    #[test]
    fn a_cookie_given_without_domain_or_path_takes_the_urls() {
        let jar = Jar::new();
        let blank = Cookie {
            name: "k".into(),
            value: "v".into(),
            domain: String::new(),
            path: String::new(),
            expires: -1.0,
            http_only: false,
            secure: false,
            same_site: None,
            partition_key: None,
        };
        assert!(jar.add(&blank, None).is_err());
        jar.add(&blank, Some(&url("https://Shop.Example/list/page")))
            .unwrap();
        let c = &jar.all()[0];
        assert_eq!(
            (c.domain.as_str(), c.path.as_str()),
            ("shop.example", "/list")
        );
    }

    #[test]
    fn find_remove_clear_and_forget() {
        let jar = Jar::new();
        set(&jar, "https://shop.example/", "id=host");
        set(
            &jar,
            "https://shop.example/",
            "id=wide; Domain=shop.example",
        );
        set(&jar, "https://api.shop.example/", "api=1");
        set(&jar, "https://other.example/", "o=1");
        assert_eq!(jar.find("id", Some("shop.example"), None).len(), 2);
        assert_eq!(jar.find("id", Some(".shop.example"), None)[0].value, "wide");
        assert_eq!(jar.remove("id", Some(".shop.example"), Some("/")), 1);
        assert_eq!(jar.find("id", None, None)[0].value, "host");
        set(
            &jar,
            "https://shop.example/",
            "id=wide; Domain=shop.example",
        );
        // A new visitor at api.shop.example: its own cookies and its parent
        // domain's go, not the parent host's own nor another site's.
        jar.forget("api.shop.example");
        let left: Vec<String> = jar.all().iter().map(|c| c.value.clone()).collect();
        assert_eq!(left, ["host", "1"]);
        assert_eq!(jar.clear(Some("shop.example")), 1);
        assert_eq!(jar.clear(None), 1);
        assert!(jar.is_empty());
    }

    #[test]
    fn a_snapshot_restores_everything_and_changes_are_counted() {
        let jar = Jar::new();
        let before = jar.changes();
        set(&jar, "https://e.com/", "a=1");
        set(
            &jar,
            "https://e.com/",
            "b=2; Domain=e.com; Max-Age=600; Secure",
        );
        assert!(jar.changes() > before);
        let copy = Jar::new();
        copy.restore(&jar.snapshot()).unwrap();
        assert_eq!(copy.all(), jar.all());
        // New cookies still come after the restored ones.
        set(&copy, "https://e.com/", "c=3");
        assert_eq!(sent(&copy, "https://e.com/"), ["a=1", "b=2", "c=3"]);
        assert!(copy.restore("not json").is_err());
        // Expiry times come back to the last bit (one a CI run caught
        // coming back an ulp off).
        let t = 1792057149.6901367_f64;
        assert_eq!(
            serde_json::from_str::<f64>(&serde_json::to_string(&t).unwrap()).unwrap(),
            t
        );
    }

    /// What the jar holds, expired or not, by name.
    fn held(jar: &Jar) -> Vec<String> {
        let mut names: Vec<String> = jar
            .lock()
            .by_key
            .values()
            .flatten()
            .map(|c| c.name.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_site_keeps_180_cookies_losing_the_least_used_insecure_ones_first() {
        let jar = Jar::new();
        for i in 0..10 {
            set(&jar, "https://shop.example/", &format!("s{i}=1; Secure"));
        }
        // On a subdomain, for the whole site: one site's all the same.
        for i in 0..170 {
            set(
                &jar,
                "https://www.shop.example/",
                &format!("n{i}=1; Domain=shop.example; Path=/p{i}"),
            );
        }
        assert_eq!(jar.len(), 180);
        // Sent, so no longer the least recently used.
        assert_eq!(sent(&jar, "https://www.shop.example/p0"), ["n0=1"]);
        set(
            &jar,
            "https://www.shop.example/",
            "n170=1; Domain=shop.example; Path=/p170",
        );
        assert_eq!(jar.len(), 150);
        let left = held(&jar);
        for kept in ["s0", "s9", "n0", "n32", "n170"] {
            assert!(left.contains(&kept.to_string()), "{kept} went");
        }
        for gone in ["n1", "n31"] {
            assert!(!left.contains(&gone.to_string()), "{gone} stayed");
        }
        // Another site is counted apart.
        set(&jar, "https://other.example/", "o=1");
        assert_eq!(jar.len(), 151);
    }

    /// A snapshot of `n` cookies over sites of 100, made and last sent
    /// at `at` but for the first site's, sent now.
    fn many(n: usize, at: f64) -> String {
        let cookies: Vec<Stored> = (0..n)
            .map(|i| Stored {
                name: format!("c{i}"),
                value: "1".into(),
                domain: format!("site{}.example", i / 100),
                host_only: true,
                path: "/".into(),
                expires: None,
                secure: false,
                http_only: false,
                same_site: None,
                partition: None,
                created: at,
                accessed: if i < 100 { now() } else { at },
                priority: Priority::Medium,
                seq: i as u64,
            })
            .collect();
        serde_json::json!({"seq": n, "cookies": cookies}).to_string()
    }

    #[test]
    fn past_3300_cookies_those_unsent_for_a_month_go_oldest_first() {
        let jar = Jar::new();
        jar.restore(&many(3400, now() - 31.0 * 86400.0)).unwrap();
        set(&jar, "https://new.example/", "a=1");
        assert_eq!(jar.len(), 3000);
        let left = held(&jar);
        assert!(left.contains(&"a".to_string()));
        assert!(left.contains(&"c0".to_string()) && left.contains(&"c99".to_string()));
        assert!(!left.contains(&"c100".to_string()));
        // Cookies sent within the month all stay, as in Chrome.
        let jar = Jar::new();
        jar.restore(&many(3400, now() - 86400.0)).unwrap();
        set(&jar, "https://new.example/", "a=1");
        assert_eq!(jar.len(), 3401);
    }

    #[test]
    fn expired_cookies_are_dropped_not_kept() {
        let jar = Jar::new();
        set(&jar, "https://e.com/", "a=1; Max-Age=600");
        set(&jar, "https://f.com/", "b=1; Max-Age=600");
        for list in jar.lock().by_key.values_mut() {
            list[0].expires = Some(now() - 1.0);
        }
        set(&jar, "https://e.com/", "c=1");
        assert_eq!(held(&jar), ["b", "c"]);
        jar.snapshot();
        assert_eq!(held(&jar), ["c"]);
    }

    #[test]
    fn a_cookie_given_whole_cant_be_for_a_public_suffix() {
        let jar = Jar::new();
        let wide = |domain: &str| Cookie {
            name: "a".into(),
            value: "1".into(),
            domain: domain.into(),
            path: "/".into(),
            expires: -1.0,
            http_only: false,
            secure: false,
            same_site: None,
            partition_key: None,
        };
        assert!(jar.add(&wide(".com"), None).is_err());
        assert!(jar.add(&wide(".github.io"), None).is_err());
        // A host that is a public suffix may have one of its own.
        jar.add(&wide("github.io"), None).unwrap();
        jar.add(&wide(".me.github.io"), None).unwrap();
        assert_eq!(jar.len(), 2);
    }

    #[test]
    fn localhost_is_secure_as_chrome_has_it() {
        let jar = Jar::new();
        for at in [
            "http://localhost:8080/",
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://shop.localhost/",
        ] {
            assert!(set(&jar, at, "s=1; Secure"), "{at}");
            assert_eq!(sent(&jar, at), ["s=1"], "{at}");
        }
        assert!(!set(&jar, "http://localhost.example/", "s=1; Secure"));
        assert!(!set(&jar, "http://10.0.0.1/", "s=1; Secure"));
    }

    #[test]
    fn a_set_cookie_without_a_name_is_a_cookie_with_none_as_in_chrome() {
        let jar = Jar::new();
        assert!(set(&jar, "https://e.com/", "nameless; Path=/"));
        assert!(set(&jar, "https://e.com/", "=val2; Path=/b"));
        assert_eq!(sent(&jar, "https://e.com/b"), ["val2", "nameless"]);
        let c = &jar.all()[0];
        assert_eq!((c.name.as_str(), c.value.as_str()), ("", "nameless"));
        // One replaces the other, as one cookie with no name.
        assert!(set(&jar, "https://e.com/", "other; Path=/"));
        assert_eq!(sent(&jar, "https://e.com/"), ["other"]);
        // A value that looks like a prefixed name, or like a pair, is
        // refused, as Chrome refuses them.
        assert!(!set(&jar, "https://e.com/", "__Secure-x; Secure"));
        assert!(!set(&jar, "https://e.com/", "=__Host-x; Secure; Path=/"));
        for refused in [
            "=x=y; Path=/a",
            "=__Secure-z; Path=/c",
            " __Host-q; Path=/d",
            "=  __secure-w; Path=/e",
        ] {
            assert!(!set(&jar, "https://e.com/", refused), "{refused}");
        }
        assert!(set(&jar, "https://e.com/", "plain2; Path=/b"));
        assert!(!set(&jar, "https://e.com/", ""));
        assert!(!set(&jar, "https://e.com/", "; Path=/"));
    }

    #[test]
    fn control_characters_in_a_name_or_value_are_refused_as_in_chrome() {
        let jar = Jar::new();
        for refused in [
            "t1=a\tb",
            "t\t5=1",
            "u=a\u{1}b",
            "v=1; Path=/x\ty",
            "w=1\u{7f}",
        ] {
            assert!(!set(&jar, "https://e.com/", refused), "{refused:?}");
        }
        // A tab at either end is only space.
        assert!(set(&jar, "https://e.com/", "t2=\ta"));
        assert_eq!(sent(&jar, "https://e.com/"), ["t2=a"]);
    }

    #[test]
    fn a_cookie_given_whole_keeps_the_rules_a_set_cookie_does() {
        let jar = Jar::new();
        let cookie = |name: &str, value: &str, domain: &str, secure: bool| Cookie {
            name: name.into(),
            value: value.into(),
            domain: domain.into(),
            path: "/".into(),
            expires: -1.0,
            http_only: false,
            secure,
            same_site: None,
            partition_key: None,
        };
        let refused = [
            cookie("__Host-x", "1", "e.com", false),
            cookie("__Secure-x", "1", "e.com", false),
            cookie("__Host-y", "1", ".e.com", true),
            Cookie {
                path: "/a".into(),
                ..cookie("__Host-z", "1", "e.com", true)
            },
            Cookie {
                same_site: Some("None".into()),
                ..cookie("ssn", "1", "e.com", false)
            },
            Cookie {
                partition_key: Some("https://top.com".into()),
                ..cookie("p", "1", "e.com", false)
            },
            cookie("", "a=b", "e.com", false),
            cookie("", "__Secure-a", "e.com", true),
            cookie("x", "a\tb", "e.com", false),
            cookie("x\ty", "1", "e.com", false),
            cookie("a=b", "1", "e.com", false),
            cookie("x", "a;b", "e.com", false),
        ];
        for c in &refused {
            let error = jar.add(c, None).unwrap_err();
            assert_eq!(Jar::check(c, None), Err(error));
        }
        assert!(jar.is_empty());
        for c in [
            cookie("__Host-ok", "1", "e.com", true),
            cookie("", "plain", "e.com", false),
            cookie("sp", "a b", "e.com", false),
        ] {
            assert_eq!(Jar::check(&c, None), Ok(()));
            jar.add(&c, None).unwrap();
        }
        assert_eq!(jar.len(), 3);
    }

    #[test]
    fn a_full_site_drops_cookies_in_chromes_order_by_priority() {
        let jar = Jar::new();
        let fill = |jar: &Jar, n: usize, prefix: &str, attributes: &str| {
            for i in 0..n {
                set(
                    jar,
                    "https://shop.example/",
                    &format!("{prefix}{i}=1; {attributes}"),
                );
            }
        };
        // Low-priority cookies go first, newest or not, down to the 30
        // most recently used; then Medium ones; High ones last of all.
        fill(&jar, 20, "h", "Priority=High");
        fill(&jar, 120, "m", "");
        fill(&jar, 40, "l", "Priority=LOW");
        set(&jar, "https://shop.example/", "m120=1");
        assert_eq!(jar.len(), 150);
        let left = held(&jar);
        let count = |p: &str| left.iter().filter(|n| n.starts_with(p)).count();
        assert_eq!((count("h"), count("m"), count("l")), (20, 100, 30));
        assert!(!left.contains(&"l9".to_string()) && left.contains(&"l10".to_string()));
        assert!(!left.contains(&"m20".to_string()) && left.contains(&"m21".to_string()));
        assert!(left.contains(&"m120".to_string()));
    }

    #[test]
    fn a_fresh_cookie_on_a_site_full_of_secure_ones_is_kept_if_secure() {
        let jar = Jar::new();
        for i in 0..180 {
            set(&jar, "https://shop.example/", &format!("s{i}=1; Secure"));
        }
        set(&jar, "https://shop.example/", "fresh=1; Secure");
        assert_eq!(jar.len(), 150);
        assert!(held(&jar).contains(&"fresh".to_string()));
        // An insecure one goes at once, as in Chrome: insecure cookies are
        // dropped before any secure one of the same priority.
        set(&jar, "https://shop.example/", "s0=1; Secure");
        for i in 0..29 {
            set(&jar, "https://shop.example/", &format!("t{i}=1; Secure"));
        }
        assert_eq!(jar.len(), 180);
        set(&jar, "https://shop.example/", "plain=1");
        assert!(!held(&jar).contains(&"plain".to_string()));
    }

    #[test]
    fn a_domain_that_is_an_address_makes_a_host_only_cookie_as_in_chrome() {
        let jar = Jar::new();
        assert!(set(&jar, "http://127.0.0.1/", "b=2; Domain=127.0.0.1"));
        assert!(set(&jar, "http://127.0.0.1/", "c=3; Domain=.127.0.0.1"));
        assert!(!set(&jar, "http://127.0.0.1/", "d=4; Domain=127.0.0.2"));
        // The one address that may also be named as a Domain for __Host-.
        assert!(set(
            &jar,
            "https://127.0.0.1/",
            "__Host-e=5; Secure; Path=/; Domain=127.0.0.1"
        ));
        let domains: Vec<String> = jar.all().into_iter().map(|c| c.domain).collect();
        assert_eq!(domains, ["127.0.0.1", "127.0.0.1", "127.0.0.1"]);
        // Given whole with a dot, likewise.
        let given = Cookie {
            name: "f".into(),
            value: "6".into(),
            domain: ".127.0.0.1".into(),
            path: "/".into(),
            expires: -1.0,
            http_only: false,
            secure: false,
            same_site: None,
            partition_key: None,
        };
        jar.add(&given, None).unwrap();
        assert_eq!(jar.find("f", None, None)[0].domain, "127.0.0.1");
    }

    #[test]
    fn http_prefixed_names_need_http_only_as_in_chrome_155() {
        let jar = Jar::new();
        // Each as Chrome 155.0.8059 took or refused it.
        for (line, kept) in [
            ("__Http-a=1; Secure", false),
            ("__Http-b=1; Secure; HttpOnly", true),
            ("__http-e=1; Secure; HttpOnly", true),
            ("__Host-Http-c=1; Secure; Path=/", false),
            ("__Host-Http-d=1; Secure; HttpOnly; Path=/", true),
            ("__Host-Http-f=1; Secure; HttpOnly", false),
            ("__Http-g; Path=/", false),
            ("h1=__Http-x; Secure", true),
        ] {
            assert_eq!(set(&jar, "https://e.com/", line), kept, "{line}");
        }
        let given = Cookie {
            name: "__Http-z".into(),
            value: "1".into(),
            domain: "e.com".into(),
            path: "/".into(),
            expires: -1.0,
            http_only: false,
            secure: true,
            same_site: None,
            partition_key: None,
        };
        assert!(jar.add(&given, None).unwrap_err().contains("__Http-"));
    }
}
