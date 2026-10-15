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
    /// By domain: the host for a host-only cookie.
    by_domain: HashMap<String, Vec<Stored>>,
    seq: u64,
    /// Bumped by every change, for whoever saves the jar.
    changes: u64,
}

impl Store {
    /// Puts `cookie` in, replacing the same cookie; one with the same
    /// value keeps its place in the order, as in Chrome.
    fn put(&mut self, mut cookie: Stored) {
        let list = self.by_domain.entry(cookie.domain.clone()).or_default();
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
            }
        }
        self.changes += 1;
    }

    fn delete(&mut self, cookie: &Stored) {
        if let Some(list) = self.by_domain.get_mut(&cookie.domain) {
            let before = list.len();
            list.retain(|c| !c.same_cookie(cookie));
            if list.len() != before {
                self.changes += 1;
            }
        }
    }

    /// Drops what `keep` refuses; how many went.
    fn drop_where(&mut self, mut gone: impl FnMut(&Stored) -> bool) -> usize {
        let mut n = 0;
        for list in self.by_domain.values_mut() {
            let before = list.len();
            list.retain(|c| !gone(c));
            n += before - list.len();
        }
        self.by_domain.retain(|_, list| !list.is_empty());
        if n > 0 {
            self.changes += 1;
        }
        n
    }

    /// Whether an http response would replace a Secure cookie (RFC
    /// 6265bis 5.7, step 16).
    fn overlays_secure(&self, name: &str, domain: &str, path: &str, now: f64) -> bool {
        self.by_domain.iter().any(|(d, list)| {
            (domain_match(d, domain) || domain_match(domain, d))
                && list.iter().any(|c| {
                    c.secure && c.name == name && !c.expired(now) && path_match(path, &c.path)
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
        let Ok(parsed) = cookie::Cookie::parse(header) else {
            return false;
        };
        let Some(host) = url.host_str().and_then(canonical) else {
            return false;
        };
        let now = now();
        let https = url.scheme() == "https";
        let (name, value) = (parsed.name().to_string(), parsed.value().to_string());
        if name.len() + value.len() > MAX_SIZE || (name.is_empty() && value.is_empty()) {
            return false;
        }
        let secure = parsed.secure().unwrap_or(false);
        if secure && !https {
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
                if is_public_suffix(&d) {
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
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("__secure-") && !secure {
            return false;
        }
        if lower.starts_with("__host-") && (!secure || attribute.is_some() || path != "/") {
            return false;
        }
        let same_site = parsed
            .same_site()
            .and_then(|s| SameSite::parse(&s.to_string()));
        if same_site == Some(SameSite::None) && !secure {
            return false;
        }
        // A cross-site subresource can't set a cookie that wouldn't be sent
        // back to it (unspecified counts as Lax).
        if same_site != Some(SameSite::None) && context.cross_site && !context.navigation {
            return false;
        }
        let partition = if parsed.partitioned().unwrap_or(false) {
            if !secure {
                return false;
            }
            Some(
                context
                    .top_level_site
                    .clone()
                    .unwrap_or_else(|| site_of(url)),
            )
        } else {
            None
        };
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
            seq: 0,
        };
        let mut store = self.lock();
        if !https && store.overlays_secure(&cookie.name, &cookie.domain, &cookie.path, now) {
            return false;
        }
        if cookie.expired(now) {
            store.delete(&cookie);
        } else {
            store.put(cookie);
        }
        true
    }

    /// Adds a cookie given whole, as a browser or a caller holds it. A
    /// domain with a leading dot is the domain's and its subdomains';
    /// without one, the host's alone. With `url`, an empty domain is the
    /// URL's host's and an empty path the URL's directory; without, both
    /// are needed. One already expired removes the cookie it matches.
    pub fn add(&self, cookie: &Cookie, url: Option<&Url>) -> Result<(), String> {
        if cookie.name.is_empty() && cookie.value.is_empty() {
            return Err("a cookie needs a name or a value".into());
        }
        if cookie.name.len() + cookie.value.len() > MAX_SIZE {
            return Err(format!("the cookie {:?} is too large", cookie.name));
        }
        let (domain, host_only) = match (cookie.domain.strip_prefix('.'), url) {
            (Some(d), _) => (canonical(d), false),
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
        let path = match (cookie.path.starts_with('/'), url) {
            (true, _) => cookie.path.clone(),
            (false, Some(url)) => default_path(url),
            (false, None) => "/".into(),
        };
        let now = now();
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
            seq: 0,
        };
        let mut store = self.lock();
        if stored.expired(now) {
            store.delete(&stored);
        } else {
            store.put(stored);
        }
        Ok(())
    }

    /// Every cookie, oldest first.
    pub fn all(&self) -> Vec<Cookie> {
        let now = now();
        let store = self.lock();
        let mut all: Vec<&Stored> = store
            .by_domain
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
            .by_domain
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
            .by_domain
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
        let https = url.scheme() == "https";
        let path = url.path();
        let store = self.lock();
        // The host and each domain above it.
        let mut domains = vec![host.as_str()];
        if !is_ip(&host) {
            domains.extend(host.match_indices('.').map(|(i, _)| &host[i + 1..]));
        }
        let mut sent: Vec<&Stored> = domains
            .iter()
            .filter_map(|d| store.by_domain.get(*d))
            .flatten()
            .filter(|c| {
                (!c.host_only || c.domain == host)
                    && !c.expired(now)
                    && path_match(path, &c.path)
                    && (!c.secure || https)
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
        sent.into_iter().map(Stored::pair).collect()
    }

    /// A count that changes whenever the jar does.
    pub fn changes(&self) -> u64 {
        self.lock().changes
    }

    /// The jar as JSON, everything kept, for `restore`.
    pub fn snapshot(&self) -> String {
        let now = now();
        let store = self.lock();
        let mut all: Vec<&Stored> = store
            .by_domain
            .values()
            .flatten()
            .filter(|c| !c.expired(now))
            .collect();
        all.sort_by_key(|c| c.seq);
        serde_json::json!({"seq": store.seq, "cookies": all}).to_string()
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
        store.by_domain.clear();
        for c in read.cookies {
            store.by_domain.entry(c.domain.clone()).or_default().push(c);
        }
        store.seq = read.seq;
        store.changes += 1;
        Ok(())
    }
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
    }
}
