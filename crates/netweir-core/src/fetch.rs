//! Making requests that look like the profile's browser made them.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use url::Url;
use wreq::header::{HeaderName, HeaderValue, LOCATION, OrigHeaderMap, SET_COOKIE};
use wreq::{Proxy, Uri, Version, redirect};

use crate::decode::decode;
use crate::profile::{Profile, Shape};

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub profile: Profile,
    /// `http://`, `https://` or `socks5://` URL of a proxy for every request.
    pub proxy: Option<String>,
    /// Whole-request limit, connect to last body byte.
    pub timeout: Duration,
    /// Only for talking to local test servers with self-signed certificates.
    pub verify_certificates: bool,
    /// The most bytes of a response body to read, after decompression; a
    /// larger one fails with `TooLarge`. None reads any size.
    pub max_body: Option<u64>,
    /// Keep cookies: send the ones responses set. Off for a caller that
    /// manages cookies itself (Scrapy's cookie middleware), so they aren't
    /// sent twice.
    pub cookies: bool,
    /// With no `proxy`, use the system's (its environment variables, and on
    /// macOS and Windows its settings), as a browser does. Off for a caller
    /// that has already decided whether to use a proxy (Scrapy's proxy
    /// middleware), or a request it sent direct would go through one.
    pub system_proxy: bool,
}

impl FetchOptions {
    pub fn new(profile: Profile) -> FetchOptions {
        FetchOptions {
            profile,
            proxy: None,
            timeout: Duration::from_secs(30),
            verify_certificates: true,
            max_body: None,
            cookies: true,
            system_proxy: true,
        }
    }
}

/// The jar, with cookies in the order the profile's browser sends them:
/// longest path first, then oldest first (Chrome, Firefox) or newest first
/// (Safari).
struct OrderedJar {
    jar: Arc<wreq::cookie::Jar>,
    newest_first: bool,
}

impl wreq::cookie::CookieStore for OrderedJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, uri: &Uri) {
        self.jar.set_cookies(cookie_headers, uri);
    }

    fn cookies(&self, uri: &Uri, version: Version) -> wreq::cookie::Cookies {
        if !self.newest_first {
            return self.jar.cookies(uri, version);
        }
        // The jar sends longest path first, then oldest first, and only it
        // knows which is older. So take its order and reverse each run of
        // equal path length. The run boundaries come from the matching
        // cookies' paths; a cookie with the same name and value under two
        // paths reads the same either way, so give each the longest unused.
        let wreq::cookie::Cookies::Uncompressed(oldest_first) =
            self.jar.cookies(uri, Version::HTTP_2)
        else {
            return wreq::cookie::Cookies::Empty;
        };
        let mut paths: HashMap<String, Vec<usize>> = HashMap::new();
        for c in self.jar.matches(uri.clone()) {
            let pair = format!("{}={}", c.name(), c.value());
            paths
                .entry(pair)
                .or_default()
                .push(c.path().unwrap_or("/").len());
        }
        let mut sent: Vec<(usize, HeaderValue)> = Vec::with_capacity(oldest_first.len());
        for value in oldest_first {
            let longest = value.to_str().ok().and_then(|pair| {
                let lengths = paths.get_mut(pair)?;
                let i = (0..lengths.len()).max_by_key(|&i| lengths[i])?;
                Some(lengths.swap_remove(i))
            });
            match longest {
                Some(length) => sent.push((length, value)),
                // The jar changed between the two reads; this one request
                // goes out in the jar's own order.
                None => return self.jar.cookies(uri, version),
            }
        }
        if sent.is_empty() {
            return wreq::cookie::Cookies::Empty;
        }
        for run in sent.chunk_by_mut(|a, b| a.0 == b.0) {
            run.reverse();
        }
        if matches!(version, Version::HTTP_2 | Version::HTTP_3) {
            wreq::cookie::Cookies::Uncompressed(sent.into_iter().map(|(_, v)| v).collect())
        } else {
            let pairs: Vec<&str> = sent.iter().filter_map(|(_, v)| v.to_str().ok()).collect();
            HeaderValue::from_str(&pairs.join("; "))
                .map(wreq::cookie::Cookies::Compressed)
                .unwrap_or(wreq::cookie::Cookies::Empty)
        }
    }
}

/// A connection pool and cookie jar that sends every request as one
/// browser. Cheap to share: clones use the same pool.
#[derive(Clone)]
pub struct Fetcher {
    client: wreq::Client,
    /// The most bytes of a body to read.
    max_body: Option<u64>,
    /// For a script's requests, where they have a priority of their own.
    script_client: Option<wreq::Client>,
    http2_headers: Arc<Vec<(HeaderName, HeaderValue)>>,
    /// Header order for a navigation's first request, and after a redirect.
    navigation_order: Arc<Vec<String>>,
    redirect_order: Arc<Vec<String>>,
    /// https origins (`host:port`) that answered over HTTP/1.1, so get no
    /// HTTP/2-only headers.
    http1_hosts: Arc<Mutex<HashSet<String>>>,
    jar: Arc<wreq::cookie::Jar>,
    /// Whether to offer only http/1.1 to origins known to answer with it.
    remembers_http1: bool,
    /// The headers of requests other than typing an address.
    shapes: Arc<Shapes>,
}

/// A profile's captured shapes, and its name to say which is missing.
struct Shapes {
    profile: String,
    link: Option<Shape>,
    form: Option<Shape>,
    fetch: Option<Shape>,
    after_form_redirect: Option<Shape>,
}

#[derive(Debug, Clone)]
pub struct Response {
    /// The final URL, after redirects.
    pub url: String,
    pub status: u16,
    /// "HTTP/1.1", "HTTP/2" and so on, "browser" for a page Chrome
    /// rendered, or "file" for one the shell read from a file.
    pub version: &'static str,
    /// In the order received. Names are lowercase.
    pub headers: Vec<(String, String)>,
    /// Decompressed, not decoded.
    pub body: Bytes,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as text, in the encoding a browser would pick.
    pub fn text(&self) -> String {
        decode(&self.body, self.header("content-type"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchErrorKind {
    /// The options were wrong: a bad URL, proxy, header or profile.
    Invalid,
    Timeout,
    /// DNS, TCP, or the proxy refusing the connection.
    Connect,
    Tls,
    TooManyRedirects,
    /// The connection broke while reading the response.
    Body,
    /// The body was larger than the limit.
    TooLarge,
    Other,
}

#[derive(Debug, Clone)]
pub struct FetchError {
    pub kind: FetchErrorKind,
    pub message: String,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FetchError {}

impl FetchError {
    pub(crate) fn invalid(message: impl Into<String>) -> FetchError {
        FetchError {
            kind: FetchErrorKind::Invalid,
            message: message.into(),
        }
    }

    /// Whether the proxy, not the site, made the request fail: no
    /// connection to the proxy, or a tunnel the proxy wanted a login for
    /// or broke off. A tunnel it refused ("unsuccessful") isn't: that's a
    /// 502, 503 or 504 when it couldn't reach the site, and the client
    /// keeps no status to tell those from a refusal of its own. Nor is a
    /// timeout, which looks the same whether the proxy or the site was
    /// slow, nor a TLS error with the site through an open tunnel.
    // Known limit: read from the error's text, as the client's tunnel errors
    // aren't public; a wreq upgrade that rewords them needs this checked
    // (the proxy tests in tests/proxies.rs would fail).
    pub fn proxy_at_fault(&self) -> bool {
        self.message.contains("(ProxyConnect)")
            && !self.message.contains("tunnel error: unsuccessful")
            && self.kind != FetchErrorKind::Timeout
    }
}

impl From<wreq::Error> for FetchError {
    fn from(e: wreq::Error) -> FetchError {
        let kind = if e.is_timeout() {
            FetchErrorKind::Timeout
        } else if e.is_tls() || caused_by_tls(&e) {
            FetchErrorKind::Tls
        } else if e.is_connect() || e.is_proxy_connect() {
            FetchErrorKind::Connect
        } else if e.is_redirect() {
            FetchErrorKind::TooManyRedirects
        } else if e.is_body() || e.is_decode() || e.is_connection_reset() {
            FetchErrorKind::Body
        } else if e.is_builder() {
            FetchErrorKind::Invalid
        } else {
            FetchErrorKind::Other
        };
        // wreq's Display stops at the outermost error; the cause is what
        // tells a user why ("connection refused", "certificate expired").
        let mut message = e.to_string();
        let mut source = std::error::Error::source(&e);
        while let Some(s) = source {
            message.push_str(": ");
            message.push_str(&s.to_string());
            source = s.source();
        }
        FetchError { kind, message }
    }
}

/// wreq reports a failed handshake (a bad certificate, a protocol
/// mismatch) as a connect error; the BoringSSL error is further down the
/// chain.
fn caused_by_tls(e: &wreq::Error) -> bool {
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        if s.is::<btls::ssl::Error>() || s.is::<btls::error::ErrorStack>() {
            return true;
        }
        source = s.source();
    }
    false
}

/// What one request of a navigation gave.
#[derive(Debug)]
pub enum Hop {
    Done(Response),
    /// A redirect, to this absolute URL, and the request to make there;
    /// with the Set-Cookie values it carried, already in the jar.
    Redirect(String, Outgoing, Vec<String>),
}

/// What kind of request this is, which decides what the browser sends
/// with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// A navigation: typing the address, or, with a referer, following a
    /// link from that page.
    #[default]
    Navigate,
    /// Submitting a form on the referer page.
    Form,
    /// The referer page's own script sending a request with `fetch()`.
    Fetch,
}

impl Kind {
    /// "navigate", "form" or "fetch", as a checkpoint saves it.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Navigate => "navigate",
            Kind::Form => "form",
            Kind::Fetch => "fetch",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "navigate" => Some(Kind::Navigate),
            "form" => Some(Kind::Form),
            "fetch" => Some(Kind::Fetch),
            _ => None,
        }
    }
}

/// How a request's target relates to the page it comes from, as
/// Sec-Fetch-Site says it; ordered from closest to furthest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
enum Relation {
    #[default]
    SameOrigin,
    SameSite,
    CrossSite,
}

impl Relation {
    fn between(from: &Url, to: &Url) -> Relation {
        if from.origin() == to.origin() {
            Relation::SameOrigin
        } else if from.scheme() == to.scheme() && site(from) == site(to) {
            Relation::SameSite
        } else {
            Relation::CrossSite
        }
    }

    fn header(self) -> &'static str {
        match self {
            Relation::SameOrigin => "same-origin",
            Relation::SameSite => "same-site",
            Relation::CrossSite => "cross-site",
        }
    }
}

/// The body, or `TooLarge` as soon as it's known to be over `max` bytes:
/// from a Content-Length that says so, or while reading, which counts the
/// bytes after decompression, so a small compressed body that unpacks to
/// gigabytes is stopped too.
async fn read_at_most(url: &str, response: wreq::Response, max: u64) -> Result<Bytes, FetchError> {
    use http_body_util::BodyExt;
    let too_large = || FetchError {
        kind: FetchErrorKind::TooLarge,
        message: format!("{url}: the response is larger than the limit of {max} bytes"),
    };
    let declared = response
        .headers()
        .get(wreq::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    let mut stream = wreq::Body::from(response);
    while let Some(frame) = stream.frame().await {
        let Ok(chunk) = frame?.into_data() else {
            continue; // trailers
        };
        if body.len() as u64 + chunk.len() as u64 > max {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.into())
}

/// From an https page to an http URL, where browsers send no Referer.
fn downgrade(from: &Url, to: &Url) -> bool {
    from.scheme() == "https" && to.scheme() == "http"
}

/// The registrable domain (`example.co.uk` for `www.example.co.uk`), or the
/// host itself for an IP address or a name with no public suffix.
fn site(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if url
        .host()
        .is_some_and(|h| !matches!(h, url::Host::Domain(_)))
    {
        return host;
    }
    psl::domain_str(&host).map_or_else(|| host.clone(), str::to_string)
}

/// A request beyond typing an address: its method, body and kind, and the
/// page it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub method: String,
    pub kind: Kind,
    pub body: Option<Vec<u8>>,
    /// The body's Content-Type.
    pub content_type: Option<String>,
    /// The page the request comes from. A form or a script without one
    /// comes from the root of the target's own origin.
    pub referer: Option<String>,
    /// The GET a form's submission turned into when redirected.
    after_form: bool,
    /// How far from the referer the redirects so far have led.
    furthest: Relation,
    /// Repeated, body and all, to another origin by a 307 or 308: browsers
    /// then send `Origin: null`.
    tainted: bool,
    /// The origin of the first URL, once a redirect has moved on from it.
    started: Option<String>,
}

impl Default for Outgoing {
    fn default() -> Outgoing {
        Outgoing::navigate()
    }
}

impl Outgoing {
    /// Typing the address: what `Fetcher::get` sends.
    pub fn navigate() -> Outgoing {
        Outgoing {
            method: "GET".into(),
            kind: Kind::Navigate,
            body: None,
            content_type: None,
            referer: None,
            after_form: false,
            furthest: Relation::SameOrigin,
            tainted: false,
            started: None,
        }
    }

    /// Following a link on `referer`.
    pub fn link(referer: String) -> Outgoing {
        Outgoing {
            referer: Some(referer),
            ..Outgoing::navigate()
        }
    }

    /// Submitting a form: a POST of `body`, URL-encoded unless
    /// `content_type` says otherwise.
    pub fn form(body: Vec<u8>, content_type: Option<String>, referer: Option<String>) -> Outgoing {
        Outgoing {
            method: "POST".into(),
            kind: Kind::Form,
            body: Some(body),
            content_type: Some(
                content_type.unwrap_or_else(|| "application/x-www-form-urlencoded".into()),
            ),
            referer,
            ..Outgoing::navigate()
        }
    }

    /// A script's request. A body without a type is sent as text, as
    /// `fetch()` sends a string.
    pub fn fetch(
        method: &str,
        body: Option<Vec<u8>>,
        content_type: Option<String>,
        referer: Option<String>,
    ) -> Outgoing {
        let content_type = content_type.or_else(|| {
            body.is_some()
                .then(|| "text/plain;charset=UTF-8".to_string())
        });
        Outgoing {
            method: method.to_ascii_uppercase(),
            kind: Kind::Fetch,
            body,
            content_type,
            referer,
            ..Outgoing::navigate()
        }
    }

    /// The request these parts describe, as `kind`'s constructor makes it.
    /// A navigation is always a GET without a body.
    pub fn from_parts(
        method: &str,
        kind: Kind,
        body: Option<Vec<u8>>,
        content_type: Option<String>,
        referer: Option<String>,
    ) -> Outgoing {
        match kind {
            Kind::Navigate => Outgoing {
                referer,
                ..Outgoing::navigate()
            },
            Kind::Form => Outgoing {
                method: method.to_ascii_uppercase(),
                ..Outgoing::form(body.unwrap_or_default(), content_type, referer)
            },
            Kind::Fetch => Outgoing::fetch(method, body, content_type, referer),
        }
    }

    /// Repeating it can't do anything twice: not a POST or a PATCH.
    pub fn idempotent(&self) -> bool {
        !matches!(self.method.as_str(), "POST" | "PATCH")
    }

    /// Typing an address: the headers the profile lists first.
    fn typed(&self) -> bool {
        self.kind == Kind::Navigate
            && self.referer.is_none()
            && !self.after_form
            && self.method == "GET"
            && self.body.is_none()
    }

    /// The request this one has become at `url` after the redirects in
    /// `chain`, each the status that redirected and the URL it came from,
    /// in order: what `send` works out hop by hop, for a caller that
    /// follows redirects itself and asks for each hop with `once`.
    pub fn after_redirects(
        &self,
        chain: &[(u16, String)],
        url: &str,
    ) -> Result<Outgoing, FetchError> {
        let mut outgoing = self.clone();
        for (i, (status, from)) in chain.iter().enumerate() {
            let to = chain.get(i + 1).map_or(url, |(_, next)| next.as_str());
            outgoing = outgoing.redirected(*status, &parse_url(from)?, &parse_url(to)?);
        }
        Ok(outgoing)
    }

    /// The request a `status` redirect from `from` to `to` makes of this
    /// one, as browsers follow them: 303, and 301 or 302 after a POST,
    /// turn it into a GET without a body; 307 and 308 repeat it.
    fn redirected(&self, status: u16, from: &Url, to: &Url) -> Outgoing {
        let mut next = self.clone();
        next.started = Some(
            self.started
                .clone()
                .unwrap_or_else(|| from.origin().ascii_serialization()),
        );
        // A form or a script's request with no page named came from its
        // own site's root; the request the redirect makes still does.
        if next.referer.is_none() && self.kind != Kind::Navigate {
            next.referer = self.referer_url(from).map(String::from);
        }
        if (status == 303 && self.method != "HEAD")
            || (matches!(status, 301 | 302) && self.method == "POST")
        {
            next.method = "GET".into();
            next.body = None;
            next.content_type = None;
            if self.kind == Kind::Form {
                next.kind = Kind::Navigate;
                next.after_form = true;
            }
        }
        if matches!(status, 307 | 308) && next.body.is_some() && from.origin() != to.origin() {
            next.tainted = true;
        }
        if let Some(referer) = self.referer_url(from) {
            next.furthest = self.furthest.max(Relation::between(&referer, to));
        }
        next
    }

    /// The caller's headers for the request to `url`, without what a
    /// browser drops on the way there: the body's Content-Type once a
    /// redirect has dropped the body, and Authorization once one has left
    /// the first origin.
    pub(crate) fn callers_headers(
        &self,
        headers: &[(String, String)],
        url: &Url,
    ) -> Vec<(String, String)> {
        let Some(started) = &self.started else {
            return headers.to_vec();
        };
        let left = *started != url.origin().ascii_serialization();
        headers
            .iter()
            .filter(|(k, _)| {
                !(self.body.is_none() && k.eq_ignore_ascii_case("content-type"))
                    && !(left && k.eq_ignore_ascii_case("authorization"))
            })
            .cloned()
            .collect()
    }

    /// The page the request comes from, as a URL.
    fn referer_url(&self, target: &Url) -> Option<Url> {
        match &self.referer {
            Some(r) => Url::parse(r).ok(),
            None if self.kind != Kind::Navigate => {
                Url::parse(&target.origin().ascii_serialization())
                    .ok()
                    .map(|mut u| {
                        u.set_path("/");
                        u
                    })
            }
            None => None,
        }
    }
}

/// `proxy` as the client takes it, or why it can't. The client treats a
/// scheme it doesn't know as no proxy at all, which would send every
/// request out directly, so only these four get through.
pub fn check_proxy(proxy: &str) -> Result<Proxy, FetchError> {
    let scheme = proxy.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
    if !matches!(
        scheme.as_deref(),
        Some("http" | "https" | "socks5" | "socks5h")
    ) {
        return Err(FetchError::invalid(format!(
            "a proxy is an http://, https://, socks5:// or socks5h:// URL, not {:?}",
            masked(proxy)
        )));
    }
    let rest = proxy.split_once("://").map_or(proxy, |(_, rest)| rest);
    // A login with an unencoded / ? # or @ in it moves where the client
    // thinks the host starts, and it would quietly go somewhere else.
    if let Some((login, _)) = rest.rsplit_once('@')
        && login.contains(['/', '?', '#', '@', ' ', '[', ']'])
    {
        return Err(FetchError::invalid(format!(
            "the login in the proxy {} has characters the URL can't hold as they are: \
             percent-encode the username and password (/ as %2F, ? as %3F, # as %23, \
             @ as %40, a space as %20)",
            masked(proxy)
        )));
    }
    // And the host and port have to be the ones written, read back.
    let authority = rest.rsplit_once('@').map_or(rest, |(_, a)| a);
    let authority = authority.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((h, p)) => (format!("[{h}]"), p.strip_prefix(':')),
            None => (authority.to_string(), None),
        },
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (authority.to_string(), None),
        },
    };
    let read = Url::parse(proxy).ok();
    let same = read.as_ref().is_some_and(|u| {
        u.host_str().is_some_and(|h| h.eq_ignore_ascii_case(&host))
            && port.is_none_or(|p| p.parse::<u16>().ok() == u.port_or_known_default())
    });
    if !same {
        return Err(FetchError::invalid(format!(
            "the proxy {} doesn't read back as the host and port written; if it has a \
             login, percent-encode the username and password",
            masked(proxy)
        )));
    }
    Proxy::all(proxy).map_err(|e| FetchError::invalid(format!("{}: {e}", masked(proxy))))
}

/// `proxy` with any login in it hidden, for logs: `http://***@host:port`.
/// Everything up to the last `@` goes, so a password with an unencoded
/// `/`, `?`, `#` or `@` in it is hidden whole.
pub fn masked(proxy: &str) -> String {
    let (scheme, rest) = proxy.split_once("://").unwrap_or(("", proxy));
    match rest.rfind('@') {
        Some(at) if scheme.is_empty() => format!("***@{}", &rest[at + 1..]),
        Some(at) => format!("{scheme}://***@{}", &rest[at + 1..]),
        None => proxy.to_string(),
    }
}

/// Chrome follows at most 20 redirects.
const MAX_REDIRECTS: usize = 20;

impl Fetcher {
    pub fn new(options: FetchOptions) -> Result<Fetcher, FetchError> {
        Fetcher::with_jar(options, Arc::new(wreq::cookie::Jar::default()))
    }

    /// A Fetcher with `options` that keeps its cookies in `other`'s jar:
    /// the same visitor, through another proxy.
    pub fn sharing_cookies(options: FetchOptions, other: &Fetcher) -> Result<Fetcher, FetchError> {
        Fetcher::with_jar(options, other.jar.clone())
    }

    fn with_jar(options: FetchOptions, jar: Arc<wreq::cookie::Jar>) -> Result<Fetcher, FetchError> {
        let profile = &options.profile;
        let emulation = profile
            .emulation()
            .map_err(|e| FetchError::invalid(e.to_string()))?;
        let build = |emulation| -> Result<wreq::Client, FetchError> {
            let mut builder = wreq::Client::builder()
                .emulation(emulation)
                .timeout(options.timeout)
                .gzip(true)
                .brotli(true)
                .zstd(true)
                .deflate(true)
                // Redirects are followed in `send`, one request per hop, so
                // each hop gets its own cookies, header order and HTTP/2-only
                // headers.
                .redirect(redirect::Policy::none())
                .tls_cert_verification(options.verify_certificates);
            if options.cookies {
                builder = builder.cookie_provider(Arc::new(OrderedJar {
                    jar: jar.clone(),
                    newest_first: profile.cookies_newest_first,
                }));
            }
            if options.proxy.is_none() && !options.system_proxy {
                builder = builder.no_proxy();
            }
            if let Some(proxy) = &options.proxy {
                builder = builder.proxy(check_proxy(proxy)?);
            }
            builder
                .build()
                .map_err(|e| FetchError::invalid(FetchError::from(e).message))
        };
        let client = build(emulation)?;
        // HTTP/2 sets a request's priority for the whole connection, so a
        // script's requests, which Chrome sends at a lower one, go on
        // connections of their own; the cookies are the same.
        let script_priority = profile
            .fetch
            .as_ref()
            .and_then(|f| f.headers_priority.as_ref());
        let script_client = match script_priority {
            Some(priority) => Some(build(
                profile
                    .emulation_with(Some(priority))
                    .map_err(|e| FetchError::invalid(e.to_string()))?,
            )?),
            None => None,
        };
        let http2_headers = profile
            .http2_headers
            .iter()
            .map(|(k, v)| {
                let name = HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes());
                let value = HeaderValue::from_str(v);
                name.ok()
                    .zip(value.ok())
                    .ok_or_else(|| FetchError::invalid(format!("bad profile header {k}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Fetcher {
            client,
            max_body: options.max_body,
            script_client,
            http2_headers: Arc::new(http2_headers),
            navigation_order: Arc::new(profile.header_order(false)),
            redirect_order: Arc::new(profile.header_order(true)),
            http1_hosts: Arc::default(),
            jar,
            remembers_http1: profile.remembers_http1,
            shapes: Arc::new(Shapes {
                profile: profile.name.clone(),
                link: profile.link.clone(),
                form: profile.form.clone(),
                fetch: profile.fetch.clone(),
                after_form_redirect: profile.after_form_redirect.clone(),
            }),
        })
    }

    /// Whether a request to `url` will go over HTTP/2: never for http://,
    /// and not for https origins that have answered over HTTP/1.1 before.
    // Known limit: the first request to an https origin without HTTP/2 still
    // carries the HTTP/2-only headers. Knowing for sure needs the ALPN result
    // before the request is written, which wreq does not expose; the fix
    // belongs in wreq (drop marked headers when the connection is HTTP/1).
    fn expects_http2(&self, url: &Uri) -> bool {
        url.scheme_str() == Some("https")
            && !https_origin(url).is_some_and(|o| {
                self.http1_hosts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&o)
            })
    }

    /// Remembers that an https origin answered over HTTP/1.1.
    fn note_version(&self, url: &Uri, version: Version) {
        if version < Version::HTTP_2
            && let Some(origin) = https_origin(url)
        {
            self.http1_hosts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(origin);
        }
    }

    /// GETs `url`, following redirects like a browser's navigation does.
    /// `headers` replace the profile's header of the same name in its usual
    /// position; new names go after the profile's own, keeping the caller's
    /// capitalisation on HTTP/1.1.
    /// Adds cookies a browser holds, as if the browser's responses had
    /// set them here.
    pub fn add_cookies(&self, cookies: &[netweir_browser::Cookie]) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        for c in cookies {
            // A leading dot marks a cookie for the domain and its
            // subdomains; without it, the cookie is the host's alone.
            let host = c.domain.trim_start_matches('.');
            let mut line = format!("{}={}; Path={}", c.name, c.value, c.path);
            if c.domain.starts_with('.') {
                line.push_str(&format!("; Domain={host}"));
            }
            if c.expires >= 0.0 {
                line.push_str(&format!("; Max-Age={}", (c.expires - now).max(0.0) as u64));
            }
            if c.secure {
                line.push_str("; Secure");
            }
            if c.http_only {
                line.push_str("; HttpOnly");
            }
            if let Some(same_site) = &c.same_site {
                line.push_str(&format!("; SameSite={same_site}"));
            }
            let scheme = if c.secure { "https" } else { "http" };
            self.jar.add(line, format!("{scheme}://{host}{}", c.path));
        }
    }

    /// Drops every cookie that would be sent to `host`, its parent
    /// domains' among them, as a new session would start without them.
    pub fn forget(&self, host: &str) {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        for c in self.jar.get_all() {
            let path = c.path().unwrap_or("/");
            // A cookie without a domain is its host's alone, and kept under
            // that host; one with a domain is kept under the domain.
            let domain = match c.domain().map(|d| d.trim_start_matches('.')) {
                None => host.as_str(),
                Some(d) if host == d || host.ends_with(&format!(".{d}")) => d,
                Some(_) => continue,
            };
            self.jar
                .remove(c.name().to_string(), format!("https://{domain}{path}"));
        }
    }

    /// Adds cookies as if a response from `url` had set them with these
    /// Set-Cookie values.
    pub fn add_set_cookies(&self, url: &str, values: &[String]) {
        for value in values {
            self.jar.add(value.clone(), url.to_string());
        }
    }

    pub async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Response, FetchError> {
        self.send(url, &Outgoing::navigate(), headers).await
    }

    /// Sends `outgoing` to `url`, following redirects as the browser
    /// would.
    pub async fn send(
        &self,
        url: &str,
        outgoing: &Outgoing,
        headers: &[(String, String)],
    ) -> Result<Response, FetchError> {
        let (mut current, mut outgoing) = (url.to_string(), outgoing.clone());
        for hop in 0..=MAX_REDIRECTS {
            match self.hop_as(&current, &outgoing, headers, hop).await? {
                Hop::Done(response) => return Ok(response),
                Hop::Redirect(next, then, _) => (current, outgoing) = (next, then),
            }
        }
        unreachable!("the last hop returns a response or an error")
    }

    /// One request of a navigation: the response, or where it redirects.
    /// `hop` counts the redirects already followed, which sets the header
    /// order Chrome uses after one and, at 20, makes another an error. A
    /// crawler that follows redirects itself, so each hop goes through its
    /// own checks, calls this instead of `get`.
    pub async fn hop(
        &self,
        url: &str,
        headers: &[(String, String)],
        hop: usize,
    ) -> Result<Hop, FetchError> {
        self.hop_as(url, &Outgoing::navigate(), headers, hop).await
    }

    /// One request and whatever answered it, a redirect included, which
    /// isn't followed: for a caller that follows redirects itself (Scrapy's
    /// redirect middleware). `hop` is the redirects followed to get here,
    /// as for `hop`.
    pub async fn once(
        &self,
        url: &str,
        outgoing: &Outgoing,
        headers: &[(String, String)],
        hop: usize,
    ) -> Result<Response, FetchError> {
        let (current, response) = self.request(url, outgoing, headers, hop).await?;
        into_response(current.to_string(), response, self.max_body).await
    }

    /// Sends one request, and gives the URL it went to with the answer.
    async fn request(
        &self,
        url: &str,
        outgoing: &Outgoing,
        headers: &[(String, String)],
        hop: usize,
    ) -> Result<(Url, wreq::Response), FetchError> {
        let headers = match Url::parse(url) {
            Ok(u) => outgoing.callers_headers(headers, &u),
            Err(_) => headers.to_vec(),
        };
        let headers = headers.as_slice();
        let mut caller = Vec::with_capacity(headers.len());
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| FetchError::invalid(format!("bad header name {k:?}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|_| FetchError::invalid(format!("bad value for header {k}")))?;
            caller.push((k.as_str(), name, value));
        }
        let current = parse_url(url)?;
        let uri: Uri = current
            .as_str()
            .parse()
            .map_err(|_| FetchError::invalid(format!("bad URL {url:?}")))?;
        let response = if outgoing.typed() {
            self.send_typed(&uri, &caller, hop > 0).await?
        } else {
            self.send_shaped(&uri, &current, outgoing, &caller, hop > 0)
                .await?
        };
        self.note_version(&uri, response.version());
        Ok((current, response))
    }

    /// `hop` for any request.
    pub async fn hop_as(
        &self,
        url: &str,
        outgoing: &Outgoing,
        headers: &[(String, String)],
        hop: usize,
    ) -> Result<Hop, FetchError> {
        let (current, response) = self.request(url, outgoing, headers, hop).await?;
        let status = response.status().as_u16();
        let location = response
            .status()
            .is_redirection()
            .then(|| response.headers().get(LOCATION))
            .flatten()
            .and_then(|l| l.to_str().ok());
        let Some(location) = location else {
            return Ok(Hop::Done(
                into_response(current.to_string(), response, self.max_body).await?,
            ));
        };
        if hop >= MAX_REDIRECTS {
            return Err(FetchError {
                kind: FetchErrorKind::TooManyRedirects,
                message: format!("more than {MAX_REDIRECTS} redirects, the last to {url}"),
            });
        }
        let next = current
            .join(location)
            .map_err(|_| FetchError::invalid(format!("redirect to a bad URL {location:?}")))?;
        if !matches!(next.scheme(), "http" | "https") {
            return Err(FetchError::invalid(format!(
                "redirect to an unsupported URL {next}"
            )));
        }
        let then = outgoing.redirected(status, &current, &next);
        let cookies = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        Ok(Hop::Redirect(next.into(), then, cookies))
    }

    async fn send_typed(
        &self,
        uri: &Uri,
        caller: &[(&str, HeaderName, HeaderValue)],
        redirected: bool,
    ) -> Result<wreq::Response, FetchError> {
        let base = if redirected {
            &self.redirect_order
        } else {
            &self.navigation_order
        };
        let mut order = OrigHeaderMap::new();
        for name in base.iter() {
            order.insert(name.clone());
        }
        for (raw, name, _) in caller {
            if !base.iter().any(|b| b.eq_ignore_ascii_case(name.as_str())) {
                order.insert(raw.to_string());
            }
        }
        let mut request = self.client.get(uri.clone()).orig_headers(order);
        if self.remembers_http1 && uri.scheme_str() == Some("https") && !self.expects_http2(uri) {
            // As Safari does: an origin that answered over HTTP/1.1 is only
            // offered http/1.1 from then on.
            request = request.version(Version::HTTP_11);
        }
        if self.expects_http2(uri) {
            for (name, value) in self.http2_headers.iter() {
                if !caller.iter().any(|(_, n, _)| n == name) {
                    request = request.header(name.clone(), value.clone());
                }
            }
        }
        for (_, name, value) in caller {
            request = request.header(name.clone(), value.clone());
        }
        Ok(request.send().await?)
    }

    /// Any request but typing an address: the headers of the profile's
    /// captured shape for it, with the per-request ones filled in as the
    /// browser fills them.
    async fn send_shaped(
        &self,
        uri: &Uri,
        target: &Url,
        outgoing: &Outgoing,
        caller: &[(&str, HeaderName, HeaderValue)],
        redirected: bool,
    ) -> Result<wreq::Response, FetchError> {
        let shapes = &self.shapes;
        let (shape, what, without) = match outgoing.kind {
            Kind::Form => (&shapes.form, "a form submission", None),
            Kind::Fetch => (&shapes.fetch, "a script's request", None),
            Kind::Navigate if outgoing.after_form => (
                &shapes.after_form_redirect,
                "the redirect after a form submission",
                None,
            ),
            // Not captured: a link's redirect is taken to be the one after
            // a form, without the form's Cache-Control.
            Kind::Navigate if redirected => (
                &shapes.after_form_redirect,
                "the redirect after a form submission",
                Some("cache-control"),
            ),
            Kind::Navigate => (&shapes.link, "following a link", None),
        };
        let shape = shape.as_ref().ok_or_else(|| {
            FetchError::invalid(format!(
                "the {} profile has no capture of {what} yet",
                shapes.profile
            ))
        })?;
        let referer = outgoing.referer_url(target);
        let relation = referer
            .as_ref()
            .map(|r| outgoing.furthest.max(Relation::between(r, target)));
        let method = wreq::Method::from_bytes(outgoing.method.as_bytes())
            .map_err(|_| FetchError::invalid(format!("bad method {:?}", outgoing.method)))?;
        let mut order = OrigHeaderMap::new();
        let client = match (&self.script_client, outgoing.kind) {
            (Some(script), Kind::Fetch) => script,
            _ => &self.client,
        };
        let mut request = client.request(method, uri.clone()).default_headers(false);
        let overridden = |name: &str| caller.iter().any(|(_, n, _)| n.as_str() == name);
        for (raw, value) in &shape.headers {
            let name = raw.to_ascii_lowercase();
            if without == Some(name.as_str()) {
                continue;
            }
            order.insert(raw.clone());
            let value = match name.as_str() {
                // Host comes from the URL, Content-Length from the body and
                // Cookie from the jar; here they only hold their places.
                "host" | "content-length" | "cookie" => None,
                "content-type" => outgoing.body.as_ref().and(outgoing.content_type.clone()),
                // Sent with a body, and by a script to another origin;
                // "null" once a redirect has repeated the body to another
                // origin, or from https to http.
                "origin" => referer
                    .as_ref()
                    .filter(|_| outgoing.body.is_some() || relation != Some(Relation::SameOrigin))
                    .map(|r| {
                        if outgoing.tainted || downgrade(r, target) {
                            "null".to_string()
                        } else {
                            r.origin().ascii_serialization()
                        }
                    }),
                // The whole URL within the origin, only the origin across
                // one, and nothing from https to http: browsers' default
                // referrer policy.
                "referer" => {
                    referer
                        .as_ref()
                        .filter(|r| !downgrade(r, target))
                        .map(|r| match relation {
                            Some(Relation::SameOrigin) => {
                                let mut r = r.clone();
                                r.set_fragment(None);
                                r.to_string()
                            }
                            _ => format!("{}/", r.origin().ascii_serialization()),
                        })
                }
                "sec-fetch-site" => Some(relation.map_or("none", Relation::header).to_string()),
                _ if value.is_empty() => None,
                _ => Some(value.clone()),
            };
            if let Some(value) = value
                && !overridden(&name)
            {
                let value = HeaderValue::from_str(&value)
                    .map_err(|_| FetchError::invalid(format!("bad value for header {raw}")))?;
                request = request.header(
                    HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| FetchError::invalid(format!("bad header name {raw}")))?,
                    value,
                );
            }
        }
        for (raw, name, _) in caller {
            if !shape
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case(name.as_str()))
            {
                order.insert(raw.to_string());
            }
        }
        if self.expects_http2(uri) {
            for (raw, value) in &shape.http2_headers {
                order.insert(raw.clone());
                let name = HeaderName::from_bytes(raw.to_ascii_lowercase().as_bytes())
                    .map_err(|_| FetchError::invalid(format!("bad header name {raw}")))?;
                if !overridden(name.as_str()) {
                    let value = HeaderValue::from_str(value)
                        .map_err(|_| FetchError::invalid(format!("bad value for header {raw}")))?;
                    request = request.header(name, value);
                }
            }
        }
        request = request.orig_headers(order);
        if self.remembers_http1 && uri.scheme_str() == Some("https") && !self.expects_http2(uri) {
            request = request.version(Version::HTTP_11);
        }
        for (_, name, value) in caller {
            request = request.header(name.clone(), value.clone());
        }
        if let Some(body) = &outgoing.body {
            request = request.body(body.clone());
        }
        Ok(request.send().await?)
    }
}

fn parse_url(url: &str) -> Result<Url, FetchError> {
    let parsed = Url::parse(url).map_err(|_| FetchError::invalid(format!("bad URL {url:?}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(FetchError::invalid(format!(
            "only http and https URLs can be fetched, not {url:?}"
        )));
    }
    Ok(parsed)
}

/// `host:port` of an https URL, the unit HTTP/2 support is known by.
fn https_origin(url: &Uri) -> Option<String> {
    (url.scheme_str() == Some("https")).then(|| {
        format!(
            "{}:{}",
            url.host().unwrap_or(""),
            url.port_u16().unwrap_or(443)
        )
    })
}

async fn into_response(
    url: String,
    response: wreq::Response,
    max_body: Option<u64>,
) -> Result<Response, FetchError> {
    let status = response.status().as_u16();
    let version = match response.version() {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "unknown",
    };
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = match max_body {
        None => response.bytes().await?,
        Some(max) => read_at_most(&url, response, max).await?,
    };
    Ok(Response {
        url,
        status,
        version,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wreq::cookie::CookieStore;

    #[test]
    fn after_redirects_is_what_following_them_one_by_one_gives() {
        // A form POSTed on one site: 303 to a page there, which 307s on,
        // which 308s to another site.
        let first = Outgoing::form(
            b"q=1".to_vec(),
            Some("application/x-www-form-urlencoded".into()),
            Some("https://a.test/search".into()),
        );
        let hops = [
            (303, "https://a.test/submit"),
            (307, "https://a.test/results"),
            (308, "https://a.test/moved"),
        ];
        let end = "https://b.test/final";
        let mut by_hand = first.clone();
        for (i, (status, from)) in hops.iter().enumerate() {
            let to = hops.get(i + 1).map_or(end, |(_, next)| next);
            by_hand = by_hand.redirected(
                *status,
                &Url::parse(from).unwrap(),
                &Url::parse(to).unwrap(),
            );
        }
        let chain: Vec<(u16, String)> = hops.iter().map(|(s, u)| (*s, u.to_string())).collect();
        assert_eq!(first.after_redirects(&chain, end).unwrap(), by_hand);
        // The 303 made it a GET without a body, and the last hop left the
        // site it started on.
        assert_eq!(by_hand.method, "GET");
        assert!(by_hand.body.is_none());
        assert!(
            first
                .after_redirects(&[(302, "not a url".into())], end)
                .is_err()
        );
    }

    fn fetcher() -> Fetcher {
        Fetcher::new(FetchOptions::new(Profile::named("chrome").unwrap())).unwrap()
    }

    #[test]
    fn plain_http_answers_say_nothing_about_https() {
        let f = fetcher();
        let https: Uri = "https://example.com/".parse().unwrap();
        f.note_version(&"http://example.com/".parse().unwrap(), Version::HTTP_11);
        assert!(f.expects_http2(&https));
        f.note_version(&https, Version::HTTP_11);
        assert!(!f.expects_http2(&https));
        // Another port on the same host is another server.
        assert!(f.expects_http2(&"https://example.com:8443/".parse().unwrap()));
        assert!(!f.expects_http2(&"http://example.com/".parse().unwrap()));
    }

    #[test]
    fn sites_are_told_apart_by_registrable_domain_and_scheme() {
        let rel = |a: &str, b: &str| Relation::between(&a.parse().unwrap(), &b.parse().unwrap());
        assert_eq!(
            rel("https://a.example.com/x", "https://a.example.com/y"),
            Relation::SameOrigin
        );
        assert_eq!(
            rel("https://a.example.com/", "https://b.example.com/"),
            Relation::SameSite
        );
        assert_eq!(
            rel("https://shop.example.co.uk/", "https://www.example.co.uk/"),
            Relation::SameSite
        );
        // co.uk is a public suffix: these are two different sites.
        assert_eq!(
            rel("https://example.co.uk/", "https://other.co.uk/"),
            Relation::CrossSite
        );
        assert_eq!(
            rel("http://example.com/", "https://example.com/"),
            Relation::CrossSite
        );
        assert_eq!(
            rel("https://127.0.0.1/", "https://localhost/"),
            Relation::CrossSite
        );
        assert_eq!(
            rel("https://example.com:8443/", "https://example.com/"),
            Relation::SameSite
        );
    }

    #[test]
    fn redirects_turn_posts_into_gets_as_browsers_do() {
        let (from, to): (Url, Url) = (
            "https://example.com/login".parse().unwrap(),
            "https://example.com/home".parse().unwrap(),
        );
        let form = Outgoing::form(b"a=1".to_vec(), None, Some("https://example.com/".into()));
        for status in [301, 302, 303] {
            let next = form.redirected(status, &from, &to);
            assert_eq!(next.method, "GET", "{status}");
            assert_eq!((next.body, next.content_type), (None, None), "{status}");
            assert!(next.after_form && next.kind == Kind::Navigate, "{status}");
            assert_eq!(
                next.referer, form.referer,
                "the form's page stays the referer"
            );
        }
        for status in [307, 308] {
            let next = form.redirected(status, &from, &to);
            assert_eq!(
                (next.method, next.kind, next.body, next.content_type),
                (
                    form.method.clone(),
                    form.kind,
                    form.body.clone(),
                    form.content_type.clone()
                ),
                "{status}"
            );
        }
        let script = Outgoing::fetch("POST", Some(b"{}".to_vec()), None, None);
        let next = script.redirected(303, &from, &to);
        assert_eq!((next.method.as_str(), next.kind), ("GET", Kind::Fetch));
        // A GET's redirect is a GET whatever the status.
        let link = Outgoing::link("https://example.com/".into());
        assert_eq!(link.redirected(302, &from, &to).method, "GET");
        // Sec-Fetch-Site after a redirect is the furthest the chain went.
        let away: Url = "https://other.org/".parse().unwrap();
        let next = link
            .redirected(302, &from, &away)
            .redirected(302, &away, &to);
        assert_eq!(next.furthest, Relation::CrossSite);
    }

    #[test]
    fn a_form_without_a_referer_keeps_its_origin_through_a_redirect() {
        let (from, to): (Url, Url) = (
            "https://example.com/login".parse().unwrap(),
            "https://example.com/home".parse().unwrap(),
        );
        let next = Outgoing::form(b"a=1".to_vec(), None, None).redirected(302, &from, &to);
        assert_eq!(next.referer.as_deref(), Some("https://example.com/"));
    }

    #[test]
    fn a_head_stays_a_head_after_a_303() {
        let (from, to): (Url, Url) = (
            "https://example.com/a".parse().unwrap(),
            "https://example.com/b".parse().unwrap(),
        );
        let head = Outgoing::fetch("HEAD", None, None, None);
        assert_eq!(head.redirected(303, &from, &to).method, "HEAD");
    }

    #[test]
    fn a_post_repeated_to_another_origin_is_tainted() {
        let (from, to): (Url, Url) = (
            "https://example.com/a".parse().unwrap(),
            "https://other.org/b".parse().unwrap(),
        );
        let form = Outgoing::form(b"a=1".to_vec(), None, Some("https://example.com/".into()));
        assert!(form.redirected(307, &from, &to).tainted);
        assert!(!form.redirected(307, &from, &from).tainted);
        // A 302 makes it a GET, which carries no Origin to taint.
        assert!(!form.redirected(302, &from, &to).tainted);
    }

    #[test]
    fn what_a_redirect_strips_from_the_callers_headers() {
        let start: Url = "https://example.com/a".parse().unwrap();
        let headers = vec![
            ("Content-Type".to_string(), "text/csv".to_string()),
            ("Authorization".to_string(), "Bearer x".to_string()),
            ("X-Keep".to_string(), "1".to_string()),
        ];
        let names = |o: &Outgoing, at: &str| -> Vec<String> {
            o.callers_headers(&headers, &at.parse().unwrap())
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        let post = Outgoing::fetch("POST", Some(b"a".to_vec()), None, None);
        assert_eq!(names(&post, "https://example.com/a").len(), 3);
        let got = post.redirected(303, &start, &"https://example.com/b".parse().unwrap());
        assert_eq!(
            names(&got, "https://example.com/b"),
            ["Authorization", "X-Keep"]
        );
        let away = post.redirected(307, &start, &"https://other.org/".parse().unwrap());
        assert_eq!(
            names(&away, "https://other.org/"),
            ["Content-Type", "X-Keep"]
        );
    }

    #[test]
    fn a_script_body_without_a_type_is_text() {
        let script = Outgoing::fetch("post", Some(b"hi".to_vec()), None, None);
        assert_eq!(script.method, "POST");
        assert_eq!(
            script.content_type.as_deref(),
            Some("text/plain;charset=UTF-8")
        );
        assert_eq!(Outgoing::fetch("GET", None, None, None).content_type, None);
    }

    #[test]
    fn only_http_and_https_urls_are_fetched() {
        assert!(parse_url("https://example.com/a?b").is_ok());
        for bad in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "example.com",
            "",
            "https://",
        ] {
            assert_eq!(
                parse_url(bad).unwrap_err().kind,
                FetchErrorKind::Invalid,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_proxy_login_is_hidden_from_logs() {
        assert_eq!(
            masked("http://ann:p%40ss@10.0.0.1:8080"),
            "http://***@10.0.0.1:8080"
        );
        assert_eq!(
            masked("socks5://ann@proxy:1080/"),
            "socks5://***@proxy:1080/"
        );
        assert_eq!(masked("http://proxy:8080"), "http://proxy:8080");
        assert_eq!(masked("ann:secret@proxy:8080"), "***@proxy:8080");
        let err = check_proxy("ftp://ann:secret@proxy:21").unwrap_err();
        assert!(!err.message.contains("secret"), "{}", err.message);
        // A password with an unencoded / ? # or @ is hidden whole.
        for raw in [
            "http://ann:se/cret@proxy:8080",
            "http://ann:se?cret@proxy:8080",
            "http://ann:se#cret@proxy:8080",
            "http://ann:se@cret@proxy:8080",
        ] {
            assert_eq!(masked(raw), "http://***@proxy:8080", "{raw}");
        }
    }

    #[test]
    fn a_proxy_login_that_isnt_percent_encoded_is_refused() {
        for raw in [
            "http://ann:se/cret@proxy:8080",
            "http://ann:se?cret@proxy:8080",
            "http://ann:se#cret@proxy:8080",
            "http://ann:se@cret@proxy:8080",
            "http://ann:se cret@proxy:8080",
        ] {
            let err = check_proxy(raw).unwrap_err();
            assert!(err.message.contains("percent-encode"), "{raw}: {err}");
            assert!(!err.message.contains("cret"), "{raw}: {err}");
        }
        for fine in [
            "http://ann:se%2Fcret@proxy:8080",
            "http://ann:se:cret@proxy:8080",
            "http://proxy:8080",
            "http://proxy:8080/",
            "socks5h://[::1]:1080",
        ] {
            assert!(check_proxy(fine).is_ok(), "{fine}");
        }
    }

    #[test]
    fn forgetting_a_host_drops_its_cookies_and_its_domains_and_no_others() {
        let a = fetcher();
        let b = Fetcher::sharing_cookies(FetchOptions::new(Profile::named("chrome").unwrap()), &a)
            .unwrap();
        a.add_set_cookies("https://www.shop.example/", &["own=1; Path=/".into()]);
        a.add_set_cookies(
            "https://www.shop.example/cart/",
            &["deep=1; Path=/cart".into()],
        );
        a.add_set_cookies(
            "https://www.shop.example/",
            &["wide=1; Domain=shop.example".into()],
        );
        a.add_set_cookies("https://other.example/", &["own=2".into()]);
        a.add_set_cookies("https://api.shop.example/", &["api=1".into()]);
        let names = |f: &Fetcher| {
            let mut n: Vec<String> = f.jar.get_all().map(|c| c.name().to_string()).collect();
            n.sort();
            n
        };
        assert_eq!(names(&b), ["api", "deep", "own", "own", "wide"], "shared");
        b.forget("www.shop.example");
        assert_eq!(names(&a), ["api", "own"]);
        assert!(
            a.jar
                .matches("https://other.example/")
                .any(|c| c.value() == "2")
        );
    }

    fn safari_cookies(jar: &OrderedJar, url: &str, version: Version) -> Vec<String> {
        match jar.cookies(&url.parse().unwrap(), version) {
            wreq::cookie::Cookies::Uncompressed(v) => {
                v.iter().map(|h| h.to_str().unwrap().to_owned()).collect()
            }
            wreq::cookie::Cookies::Compressed(h) => vec![h.to_str().unwrap().to_owned()],
            _ => Vec::new(),
        }
    }

    #[test]
    fn safari_sends_newest_first_whatever_other_sites_set() {
        let jar = OrderedJar {
            jar: Arc::new(wreq::cookie::Jar::default()),
            newest_first: true,
        };
        let set = |url: &str, cookie: &str| {
            let value = HeaderValue::from_str(cookie).unwrap();
            jar.set_cookies(&mut std::iter::once(&value), &url.parse().unwrap());
        };
        // A host-only cookie of the same name, set earlier on another site,
        // mustn't make this site's sid look older than it is.
        set("https://a.example/", "sid=a; Path=/");
        set("https://b.example/", "t=x; Path=/");
        set("https://b.example/", "sid=b; Path=/");
        set("https://b.example/deep/", "p=1; Path=/deep");
        assert_eq!(
            safari_cookies(&jar, "https://b.example/deep/x", Version::HTTP_2),
            ["p=1", "sid=b", "t=x"]
        );
        assert_eq!(
            safari_cookies(&jar, "https://b.example/deep/x", Version::HTTP_11),
            ["p=1; sid=b; t=x"]
        );
        for version in [Version::HTTP_11, Version::HTTP_2] {
            assert!(matches!(
                jar.cookies(&"https://c.example/".parse().unwrap(), version),
                wreq::cookie::Cookies::Empty
            ));
        }
    }
}
