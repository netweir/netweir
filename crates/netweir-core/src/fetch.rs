//! Making requests that look like the profile's browser made them.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use netweir_browser::steps::Outcome;
use url::Url;
use wreq::header::{HeaderMap, HeaderName, HeaderValue, LOCATION, OrigHeaderMap, SET_COOKIE};
use wreq::{Proxy, Uri, Version, redirect};
use wreq_proto::ext::{OnPreserveHeaderCallback, ReasonPhrase};

use crate::archive::{Archive, Exchange};
use crate::cookies::{self, Jar};
use crate::decode::decode;
use crate::profile::{Profile, Shape};

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub profile: Profile,
    /// `http://`, `https://` or `socks5://` URL of a proxy for every request.
    pub proxy: Option<String>,
    /// Whole-request limit, connect to last body byte.
    pub timeout: Duration,
    /// Check the server's certificate. Off only for a host whose
    /// certificate nothing vouches for (a test server, an appliance).
    /// Checking happens on the client: nothing on the wire changes.
    pub verify_certificates: bool,
    /// PEM certificates of the CAs to trust in place of the bundled roots,
    /// for hosts an internal CA vouches for. None trusts the bundled roots
    /// (Chrome's).
    pub ca_certificates: Option<Vec<u8>>,
    /// Redirects followed before a request fails with `TooManyRedirects`;
    /// a request's own `Policy::max_redirects` replaces it. Chrome's is 20.
    pub max_redirects: u32,
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
    /// Where every request sent and the response it got are kept.
    pub archive: Option<Archive>,
}

impl FetchOptions {
    pub fn new(profile: Profile) -> FetchOptions {
        FetchOptions {
            profile,
            proxy: None,
            timeout: Duration::from_secs(30),
            verify_certificates: true,
            ca_certificates: None,
            max_redirects: MAX_REDIRECTS,
            max_body: None,
            cookies: true,
            system_proxy: true,
            archive: None,
        }
    }
}

/// A jar as one request sees it: what the request's context lets it
/// carry and set, in the order the profile's browser sends cookies.
struct JarView {
    jar: Arc<Jar>,
    newest_first: bool,
    context: cookies::Context,
}

impl wreq::cookie::CookieStore for JarView {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, uri: &Uri) {
        let Ok(url) = Url::parse(&uri.to_string()) else {
            return;
        };
        for value in cookie_headers {
            if let Ok(value) = value.to_str() {
                self.jar.set_cookie(value, &url, &self.context);
            }
        }
    }

    fn cookies(&self, uri: &Uri, version: Version) -> wreq::cookie::Cookies {
        let Ok(url) = Url::parse(&uri.to_string()) else {
            return wreq::cookie::Cookies::Empty;
        };
        let pairs = self.jar.header(&url, &self.context, self.newest_first);
        if pairs.is_empty() {
            return wreq::cookie::Cookies::Empty;
        }
        // Over HTTP/2 each cookie is a field of its own, as Chrome's HPACK
        // encoder splits the header into crumbs.
        if matches!(version, Version::HTTP_2 | Version::HTTP_3) {
            wreq::cookie::Cookies::Uncompressed(
                pairs
                    .iter()
                    .filter_map(|p| HeaderValue::from_str(p).ok())
                    .collect(),
            )
        } else {
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
    /// Redirects a request may follow, unless its policy says otherwise.
    max_redirects: u32,
    /// For a script's requests, where they have a priority of their own.
    script_client: Option<wreq::Client>,
    /// For images, likewise.
    image_client: Option<wreq::Client>,
    http2_headers: Arc<Vec<(HeaderName, HeaderValue)>>,
    /// Header order for a navigation's first request, and after a redirect.
    navigation_order: Arc<Vec<String>>,
    redirect_order: Arc<Vec<String>>,
    /// https origins (`host:port`) that answered over HTTP/1.1, so get no
    /// HTTP/2-only headers.
    http1_hosts: Arc<Mutex<HashSet<String>>>,
    jar: Arc<Jar>,
    /// Whether requests carry and keep cookies at all.
    cookies: bool,
    /// Safari sends cookies of equal path length newest first.
    newest_first: bool,
    /// Whether to offer only http/1.1 to origins known to answer with it.
    remembers_http1: bool,
    /// The headers of requests other than typing an address.
    shapes: Arc<Shapes>,
    archive: Option<Archive>,
}

/// The header lines a request went out with, as the client wrote them.
type Sent = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// What an exchange's record needs of a request and its response before
/// the body is read.
struct Record {
    started: SystemTime,
    clock: Instant,
    /// From sending to the response's head.
    wait: Duration,
    sent: Sent,
    headers: Vec<(String, Vec<u8>)>,
    reason: String,
}

impl Record {
    fn exchange(
        self,
        current: &Url,
        outgoing: &Outgoing,
        response: &Response,
        truncated: bool,
    ) -> Exchange {
        Exchange {
            started: self.started,
            url: current.to_string(),
            method: outgoing.method.clone(),
            // Logins stay out of an archive, which gets shared: the
            // proxy's, never sent to the site, and the site's own.
            request_headers: std::mem::take(
                &mut *self.sent.lock().unwrap_or_else(|e| e.into_inner()),
            )
            .into_iter()
            .filter(|(k, _)| {
                !k.eq_ignore_ascii_case("proxy-authorization")
                    && !k.eq_ignore_ascii_case("authorization")
            })
            .collect(),
            request_body: outgoing.body.clone(),
            version: response.version,
            status: response.status,
            reason: self.reason,
            response_headers: self.headers,
            body: response.body.clone(),
            truncated,
            wait: self.wait,
            receive: self.clock.elapsed().saturating_sub(self.wait),
        }
    }
}

/// wreq's header ordering for one request, writing down each line as the
/// client emits it: the request's headers after the profile's defaults,
/// the jar's Cookie, Host and Content-Length are all in. wreq calls
/// `call_visit` as it writes an HTTP/1.1 head, and `call` before it encodes
/// an HTTP/2 HEADERS frame (whose names are lowercase, and whose
/// pseudo-headers aren't in the map).
struct Recorder {
    order: OrigHeaderMap,
    sent: Sent,
}

impl OnPreserveHeaderCallback for Recorder {
    fn call(&self, headers: &mut HeaderMap) {
        self.order.call(headers);
        let lines = headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
            .collect();
        *self.sent.lock().unwrap_or_else(|e| e.into_inner()) = lines;
    }

    fn call_visit(
        &self,
        headers: &mut HeaderMap,
        dst: &mut dyn FnMut(&dyn AsRef<[u8]>, &HeaderValue),
    ) {
        let mut lines = Vec::with_capacity(headers.len());
        self.order.call_visit(headers, &mut |name, value| {
            lines.push((
                String::from_utf8_lossy(name.as_ref()).into_owned(),
                value.as_bytes().to_vec(),
            ));
            dst(name, value);
        });
        *self.sent.lock().unwrap_or_else(|e| e.into_inner()) = lines;
    }
}

/// Sends `request` with its headers in `order`, writing down the lines it
/// went out with in `sent`, if given.
async fn dispatch(
    request: wreq::RequestBuilder,
    order: OrigHeaderMap,
    sent: Option<&Sent>,
    view: Option<JarView>,
) -> Result<wreq::Response, FetchError> {
    let request = match view {
        Some(view) => request.cookie_provider(Arc::new(view)),
        None => request,
    };
    let Some(sent) = sent else {
        return Ok(request.orig_headers(order).send().await?);
    };
    // The hook goes in the request's extensions, where wreq's own ordering
    // would otherwise go; the client has no default order to put back in
    // its place (see Profile::emulation).
    let (client, request) = request.build_split();
    let mut request = request?;
    let mut carrier = http::Request::new(());
    wreq_proto::ext::on_preserve_header(
        &mut carrier,
        Recorder {
            order,
            sent: sent.clone(),
        },
    );
    request
        .extensions_mut()
        .extend(carrier.into_parts().0.extensions);
    Ok(client.execute(request).await?)
}

/// A profile's captured shapes, and its name to say which is missing.
struct Shapes {
    profile: String,
    link: Option<Shape>,
    form: Option<Shape>,
    fetch: Option<Shape>,
    image: Option<Shape>,
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
    /// For a page Chrome rendered, how each of its request's steps went;
    /// empty for any other.
    pub steps: Arc<[Outcome]>,
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

    /// The encoding `text` decodes the body with, by name.
    pub fn encoding(&self) -> &'static str {
        crate::decode::encoding_name(&self.body, self.header("content-type"))
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
    /// A browser request's step failed, and its `on_error` said to fail
    /// the request. Not retried: steps may have done something (sent a
    /// form) a retry would do twice.
    Step,
    Other,
}

#[derive(Debug, Clone)]
pub struct FetchError {
    pub kind: FetchErrorKind,
    pub message: String,
    /// For a browser request whose steps failed, how each went, up to the
    /// one that failed them; empty for any other error.
    pub steps: Arc<[Outcome]>,
}

impl FetchError {
    pub fn new(kind: FetchErrorKind, message: impl Into<String>) -> FetchError {
        FetchError {
            kind,
            message: message.into(),
            steps: Arc::default(),
        }
    }
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
            steps: Default::default(),
        }
    }

    /// Whether the proxy, not the site, made the request fail: no
    /// connection to the proxy, or a tunnel the proxy wanted a login for
    /// or broke off. A tunnel it refused ("unsuccessful") isn't: that's a
    /// 502, 503 or 504 when it couldn't reach the site, and the client
    /// keeps no status to tell those from a refusal of its own. Nor is a
    /// timeout, which looks the same whether the proxy or the site was
    /// slow, nor a TLS error with the site through an open tunnel.
    // Known limit: nor a plain-HTTP request the proxy closed the connection
    // on before answering. That error reads the same as a kept-alive
    // connection the proxy let go of while idle, and blaming the proxy
    // for it would bench healthy ones; through a tunnel it's caught.
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
        FetchError::new(kind, message)
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
    /// An image the referer page shows, as an `<img>` loads it.
    Image,
}

impl Kind {
    /// "navigate", "form", "fetch" or "image", as a checkpoint saves it.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Navigate => "navigate",
            Kind::Form => "form",
            Kind::Fetch => "fetch",
            Kind::Image => "image",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "navigate" => Some(Kind::Navigate),
            "form" => Some(Kind::Form),
            "fetch" => Some(Kind::Fetch),
            "image" => Some(Kind::Image),
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
        steps: Default::default(),
    };
    let declared = response
        .headers()
        .get(wreq::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max) {
        return Err(too_large());
    }
    // Room for the whole declared body up front. Grown as it arrives, the
    // last step copies half the body into a buffer the size of all of it,
    // so both are held at once, and the macOS allocator now and then keeps
    // the old one resident after it's freed: a 32 MiB body took 72 MiB
    // at its peak. Known limit: a body of no declared length
    // (chunked) still grows that way. The length is the server's word, so
    // the room is asked for, not insisted on: a limit set high and a
    // length nobody could hold leave the body to grow as it comes instead
    // of ending the process.
    let room = declared.map_or(0, |n| usize::try_from(n).unwrap_or(0));
    let mut body = Vec::new();
    let _ = body.try_reserve_exact(room);
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

/// How a request is handled beyond what goes on the wire: decided on the
/// client, or a header where the browser puts it. The default is the
/// fetcher's own: its timeout and redirect limit, and no login.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Policy {
    /// The limit for each request, from connecting to the last byte, in
    /// place of the fetcher's, longer or shorter. Each redirect is a
    /// request of its own.
    #[serde(with = "seconds")]
    pub timeout: Option<Duration>,
    /// Redirects to follow before failing with `TooManyRedirects`, in
    /// place of the fetcher's.
    pub max_redirects: Option<u32>,
    /// Answer with a redirect itself rather than follow it.
    pub dont_redirect: bool,
    pub auth: Option<Auth>,
}

/// A timeout as seconds in JSON, which a person reading a checkpoint or a
/// queue can make sense of.
mod seconds {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(t: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_f64(t.as_secs_f64()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Option::<f64>::deserialize(d)?
            .map(|s| Duration::try_from_secs_f64(s).map_err(D::Error::custom))
            .transpose()
    }
}

/// A login sent as HTTP Basic (RFC 7617), up front.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Auth {
    pub user: String,
    pub password: String,
    /// The domain it goes to, with its subdomains, as Scrapy's
    /// `http_auth_domain` names one: on every redirect hop there, and
    /// nowhere else. None sends it to the request's first origin only, as
    /// a browser drops a page's Authorization when a redirect leaves it.
    pub domain: Option<String>,
    /// Read back from a checkpoint or a shared queue, which keep no
    /// password: a resume looks for it among the crawl's logins.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("user", &self.user)
            .field("password", &"***")
            .field("domain", &self.domain)
            .finish()
    }
}

impl Auth {
    pub fn new(
        user: impl Into<String>,
        password: impl Into<String>,
        domain: Option<String>,
    ) -> Auth {
        Auth {
            user: user.into(),
            password: password.into(),
            domain,
            hidden: false,
        }
    }

    /// The Authorization header's value: the pair as UTF-8, in base64.
    pub fn header(&self) -> String {
        let pair = format!("{}:{}", self.user, self.password);
        format!("Basic {}", data_encoding::BASE64.encode(pair.as_bytes()))
    }

    /// Whether the login goes with a request to `url`, when the request
    /// started at the origin `started` (None for its first hop), and
    /// `left` says whether a redirect has taken it to another origin. One
    /// for a domain goes to it and its subdomains, but not over plain HTTP
    /// once the request started over HTTPS; one without, only to the
    /// origin the request started at, and not again once it has left it.
    pub fn applies(&self, url: &Url, started: Option<&str>, left: bool) -> bool {
        let Some(domain) = &self.domain else {
            return !left;
        };
        let bare = |h: &str| {
            h.trim_end_matches('.')
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase()
        };
        let domain = url::Host::parse(domain.trim_end_matches('.'))
            .map_or_else(|_| bare(domain), |h| bare(&h.to_string()));
        let host = bare(url.host_str().unwrap_or_default());
        let downgrade =
            url.scheme() == "http" && started.is_some_and(|s| s.starts_with("https://"));
        (host == domain || host.ends_with(&format!(".{domain}"))) && !downgrade
    }
}

/// `domain` as a login's domain (Scrapy's `http_auth_domain`): a bare host
/// name or IP address, lowercased, a leading or trailing dot dropped. One
/// with a scheme, a port, a path or a login would never match a host, and
/// the login would silently go nowhere; a public suffix (`com`, `co.uk`)
/// would send it to every site under it. Both are errors.
pub fn auth_domain(domain: &str) -> Result<String, String> {
    let d = domain
        .trim()
        .trim_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if d.is_empty() {
        return Err("a login's domain can't be empty".into());
    }
    if d.contains("://") || d.contains(['/', '?', '#', '@', ' ']) {
        return Err(format!(
            "a login's domain is a host name, without a scheme, path or login: not {domain:?}"
        ));
    }
    if let Ok(ip) = d.parse::<std::net::IpAddr>() {
        return Ok(ip.to_string());
    }
    if d.contains(':') {
        return Err(format!(
            "a login's domain is a host name, without a port: not {domain:?}; it goes to every \
             port there"
        ));
    }
    let host = match url::Host::parse(&d) {
        Ok(url::Host::Domain(host)) => host,
        Ok(other) => return Ok(other.to_string()),
        Err(_) => return Err(format!("{domain:?} isn't a host name")),
    };
    if psl::suffix(host.as_bytes()).is_some_and(|s| s.is_known() && s.as_bytes() == host.as_bytes())
    {
        return Err(format!(
            "{domain:?} is a public suffix: a login for it would go to every site under it"
        ));
    }
    Ok(host)
}

/// A request beyond typing an address: its method, body and kind, the
/// page it comes from, and how it's handled.
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
    /// A redirect has taken it to another origin: Authorization stays
    /// behind from then on, even if a later one leads back.
    left: bool,
    /// Its timeout, redirect limit and login, kept across redirects.
    pub policy: Policy,
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
            left: false,
            policy: Policy::default(),
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

    /// An `<img>` on `referer` loading its picture: always a GET.
    pub fn image(referer: Option<String>) -> Outgoing {
        Outgoing {
            kind: Kind::Image,
            referer,
            ..Outgoing::navigate()
        }
    }

    /// The request these parts describe, as `kind`'s constructor makes it.
    /// A navigation or an image is always a GET without a body.
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
            Kind::Image => Outgoing::image(referer),
        }
    }

    /// Repeating it can't do anything twice: not a POST or a PATCH.
    pub fn idempotent(&self) -> bool {
        !matches!(self.method.as_str(), "POST" | "PATCH")
    }

    /// What the jar needs to know of this request to `url`: whether it's
    /// cross-site to the page it comes from (or any redirect on the way),
    /// a top-level navigation, a safe method, and the top-level site.
    pub fn cookie_context(&self, url: &Url) -> cookies::Context {
        if self.typed() {
            return cookies::Context::typed(url);
        }
        let referer = self.referer_url(url);
        let relation = referer
            .as_ref()
            .map(|r| self.furthest.max(Relation::between(r, url)));
        let navigation = matches!(self.kind, Kind::Navigate | Kind::Form);
        let top = match (&referer, navigation) {
            (Some(r), false) => r,
            _ => url,
        };
        cookies::Context {
            cross_site: relation == Some(Relation::CrossSite),
            navigation,
            safe_method: matches!(self.method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE"),
            top_level_site: Some(cookies::site_of(top)),
        }
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
        // As the Fetch standard's HTTP-redirect fetch deletes it: the
        // moment a redirect crosses to another origin, and for good.
        next.left = self.left || from.origin() != to.origin();
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

    /// The caller's headers for this hop, without what a
    /// browser drops on the way there: the body's Content-Type once a
    /// redirect has dropped the body, and Authorization once one has
    /// crossed to another origin, wherever the next ones lead.
    pub(crate) fn callers_headers(&self, headers: &[(String, String)]) -> Vec<(String, String)> {
        if self.started.is_none() {
            return headers.to_vec();
        }
        headers
            .iter()
            .filter(|(k, _)| {
                !(self.body.is_none() && k.eq_ignore_ascii_case("content-type"))
                    && !(self.left && k.eq_ignore_ascii_case("authorization"))
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

/// The CAs in a PEM file, as a trust store, or why they can't be one. A
/// file without a certificate in it is an error: an empty store would
/// refuse every site, and say only that each certificate is untrusted.
fn ca_store(pem: &[u8]) -> Result<wreq::tls::trust::CertStore, FetchError> {
    let bad = |why: String| FetchError::invalid(format!("the CA certificates: {why}"));
    match btls::x509::X509::stack_from_pem(pem) {
        Ok(certs) if !certs.is_empty() => {}
        Ok(_) => return Err(bad("no PEM certificate in them".into())),
        Err(e) => return Err(bad(e.to_string())),
    }
    wreq::tls::trust::CertStore::from_pem_stack(pem).map_err(|e| bad(e.to_string()))
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
pub const MAX_REDIRECTS: u32 = 20;

/// What a request's connection is doing, for [`telling_connects`]: a new
/// one being made (DNS, TCP, a proxy's tunnel, TLS), made, or failed. A
/// request on a connection already open goes out as it's sent, and is
/// told nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Connect {
    Started,
    Ready(Instant),
    Failed,
}

/// Who hears of the connections the requests in a future make.
pub(crate) type ConnectTold = Arc<dyn Fn(Connect) + Send + Sync>;

tokio::task_local! {
    static TOLD: ConnectTold;
}

/// Runs `future` with `tell` hearing of each new connection its requests
/// open: when one starts and when it's made, which is when the request's
/// bytes go out. A crawl spaces its requests to a site by that, not by
/// when it handed them out: a request waiting 400 ms for a TLS handshake
/// leaves 400 ms later than it was handed out, and the next one, on a
/// connection already open, would reach the site that much sooner.
pub(crate) async fn telling_connects<F: std::future::Future>(
    tell: ConnectTold,
    future: F,
) -> F::Output {
    // Connections still being made when the request is done (the pool
    // found it an idle one and goes on making this one in the background)
    // are told as failed now, and nothing of them after: no request of
    // this scope rides them, and a background handshake that hangs would
    // otherwise hold the host for good.
    struct Scope(Arc<std::sync::Mutex<Option<usize>>>, ConnectTold);
    impl Drop for Scope {
        fn drop(&mut self) {
            let open = self.0.lock().unwrap().take().unwrap_or(0);
            for _ in 0..open {
                (self.1)(Connect::Failed);
            }
        }
    }
    let open = Arc::new(std::sync::Mutex::new(Some(0usize)));
    let scope = Scope(open.clone(), tell.clone());
    let gated: ConnectTold = Arc::new(move |c| {
        let mut open = open.lock().unwrap();
        let Some(n) = open.as_mut() else { return };
        match c {
            Connect::Started => *n += 1,
            _ => *n = n.saturating_sub(1),
        }
        drop(open);
        tell(c);
    });
    let out = TOLD.scope(gated, future).await;
    drop(scope);
    out
}

/// The connector layer that tells [`telling_connects`]'s listener.
#[derive(Clone, Copy)]
struct Told;

impl<S> tower::Layer<S> for Told {
    type Service = TellingConnector<S>;
    fn layer(&self, inner: S) -> TellingConnector<S> {
        TellingConnector(inner)
    }
}

#[derive(Clone)]
struct TellingConnector<S>(S);

/// Says the connection failed if it's dropped before it's made, so a
/// listener counting connections being made never counts one for good.
struct Telling(Option<ConnectTold>);

impl Drop for Telling {
    fn drop(&mut self) {
        if let Some(tell) = self.0.take() {
            tell(Connect::Failed);
        }
    }
}

impl<S, R> tower::Service<R> for TellingConnector<S>
where
    S: tower::Service<R>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), S::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, request: R) -> Self::Future {
        // Told when asked for inside `telling_connects`. The pool races
        // its checkout against this connect: if an idle connection wins,
        // this one goes on in the background, and the scope, done with
        // its request by then, has taken it as failed and hears nothing
        // more of it (see `telling_connects`).
        let tell = TOLD.try_with(Arc::clone).ok();
        if let Some(tell) = &tell {
            tell(Connect::Started);
        }
        let mut telling = Telling(tell);
        let connecting = self.0.call(request);
        Box::pin(async move {
            let made = connecting.await;
            if let Some(tell) = telling.0.take() {
                tell(match &made {
                    Ok(_) => Connect::Ready(Instant::now()),
                    Err(_) => Connect::Failed,
                });
            }
            made
        })
    }
}

impl Fetcher {
    pub fn new(options: FetchOptions) -> Result<Fetcher, FetchError> {
        Fetcher::made_with(options, Arc::new(Jar::new()))
    }

    /// A Fetcher with `options` that keeps its cookies in `other`'s jar:
    /// the same visitor, through another proxy.
    pub fn sharing_cookies(options: FetchOptions, other: &Fetcher) -> Result<Fetcher, FetchError> {
        Fetcher::made_with(options, other.jar.clone())
    }

    /// This Fetcher, its connections and all, with its cookies in `jar`:
    /// another visitor through the same client. The jar is applied per
    /// request, so this costs a clone.
    pub fn with_jar(&self, jar: Arc<Jar>) -> Fetcher {
        Fetcher {
            jar,
            ..self.clone()
        }
    }

    /// The jar its requests carry and keep cookies in.
    pub fn jar(&self) -> &Arc<Jar> {
        &self.jar
    }

    fn made_with(options: FetchOptions, jar: Arc<Jar>) -> Result<Fetcher, FetchError> {
        let profile = &options.profile;
        let emulation = profile
            .emulation()
            .map_err(|e| FetchError::invalid(e.to_string()))?;
        let cas = options
            .ca_certificates
            .as_deref()
            .map(ca_store)
            .transpose()?;
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
                // A connection the pool goes on making in the background,
                // after an idle one came free for its request, has no
                // request's timeout over it: one whose handshake never
                // answered held its socket for good. Making one gets the
                // client's timeout, and never less than the 30 s Chrome
                // gives a TLS handshake (net/socket/ssl_connect_job.cc;
                // its TCP connect may take 4 minutes). A request's own
                // longer timeout doesn't stretch its connect.
                .connect_timeout(options.timeout.max(Duration::from_secs(30)))
                .tls_cert_verification(options.verify_certificates);
            if let Some(cas) = &cas {
                builder = builder.tls_cert_store(cas.clone());
            }
            builder = builder.connector_layer(Told);
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
        // script's requests and images, which Chrome sends at lower ones,
        // go on connections of their own; the cookies are the same.
        let client_for = |shape: &Option<Shape>| -> Result<Option<wreq::Client>, FetchError> {
            match shape.as_ref().and_then(|s| s.headers_priority.as_ref()) {
                Some(priority) => Ok(Some(build(
                    profile
                        .emulation_with(Some(priority))
                        .map_err(|e| FetchError::invalid(e.to_string()))?,
                )?)),
                None => Ok(None),
            }
        };
        let script_client = client_for(&profile.fetch)?;
        let image_client = client_for(&profile.image)?;
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
            max_redirects: options.max_redirects,
            script_client,
            image_client,
            http2_headers: Arc::new(http2_headers),
            navigation_order: Arc::new(profile.header_order(false)),
            redirect_order: Arc::new(profile.header_order(true)),
            http1_hosts: Arc::default(),
            jar,
            cookies: options.cookies,
            newest_first: profile.cookies_newest_first,
            remembers_http1: profile.remembers_http1,
            shapes: Arc::new(Shapes {
                profile: profile.name.clone(),
                link: profile.link.clone(),
                form: profile.form.clone(),
                fetch: profile.fetch.clone(),
                image: profile.image.clone(),
                after_form_redirect: profile.after_form_redirect.clone(),
            }),
            archive: options.archive.clone(),
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

    /// Adds cookies a browser holds, as if the browser's responses had
    /// set them here. One that won't go in (no domain) is left out.
    pub fn add_cookies(&self, cookies: &[netweir_browser::Cookie]) {
        for c in cookies {
            let _ = self.jar.add(c, None);
        }
    }

    /// Drops every cookie that would be sent to `host`, its parent
    /// domains' among them, as a new session would start without them.
    pub fn forget(&self, host: &str) {
        self.jar.forget(host);
    }

    /// Adds cookies as if a response from `url` to a request sent as
    /// `outgoing` had set them with these Set-Cookie values.
    pub fn add_set_cookies(&self, url: &str, outgoing: &Outgoing, values: &[String]) {
        let Ok(url) = Url::parse(url) else {
            return;
        };
        let context = outgoing.cookie_context(&url);
        for value in values {
            self.jar.set_cookie(value, &url, &context);
        }
    }

    /// What `outgoing` to `url` hands the jar: the request's own view of
    /// it, or none when this Fetcher keeps no cookies.
    fn jar_view(&self, url: &Url, outgoing: &Outgoing) -> Option<JarView> {
        self.cookies.then(|| JarView {
            jar: self.jar.clone(),
            newest_first: self.newest_first,
            context: outgoing.cookie_context(url),
        })
    }

    /// GETs `url`, following redirects like a browser's navigation does.
    /// `headers` replace the profile's header of the same name in its usual
    /// position; new names go after the profile's own, keeping the caller's
    /// capitalisation on HTTP/1.1.
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
        for hop in 0..=self.redirect_limit(&outgoing) {
            match self.hop_as(&current, &outgoing, headers, hop).await? {
                Hop::Done(response) => return Ok(response),
                Hop::Redirect(next, then, _) => (current, outgoing) = (next, then),
            }
        }
        unreachable!("the last hop returns a response or an error")
    }

    /// Redirects `outgoing` may follow: its own limit, or the fetcher's.
    fn redirect_limit(&self, outgoing: &Outgoing) -> usize {
        outgoing.policy.max_redirects.unwrap_or(self.max_redirects) as usize
    }

    /// One request of a navigation: the response, or where it redirects.
    /// `hop` counts the redirects already followed, which sets the header
    /// order Chrome uses after one and, at the limit, makes another an
    /// error. A crawler that follows redirects itself, so each hop goes
    /// through its own checks, calls this instead of `get`.
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
        let (current, response, record) = self.request(url, outgoing, headers, hop).await?;
        let response = into_response(current.to_string(), response, self.max_body).await?;
        self.keep(record, &current, outgoing, &response, false)
            .await;
        Ok(response)
    }

    /// Hands the exchange to the archive, when there is one.
    async fn keep(
        &self,
        record: Option<Record>,
        current: &Url,
        outgoing: &Outgoing,
        response: &Response,
        truncated: bool,
    ) {
        if let (Some(archive), Some(record)) = (&self.archive, record) {
            archive
                .record(record.exchange(current, outgoing, response, truncated))
                .await;
        }
    }

    /// Sends one request, and gives the URL it went to with the answer,
    /// and, with an archive, what its record needs before the body is read.
    async fn request(
        &self,
        url: &str,
        outgoing: &Outgoing,
        headers: &[(String, String)],
        hop: usize,
    ) -> Result<(Url, wreq::Response, Option<Record>), FetchError> {
        let parsed = Url::parse(url).ok();
        let mut headers = outgoing.callers_headers(headers);
        // The login, where it may go and the caller sent none of its own;
        // the profile lists Authorization where its browser sends it.
        if let (Some(auth), Some(u)) = (&outgoing.policy.auth, &parsed)
            && auth.applies(u, outgoing.started.as_deref(), outgoing.left)
            && !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        {
            headers.push(("Authorization".into(), auth.header()));
        }
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
        let sent: Option<Sent> = self.archive.as_ref().map(|_| Sent::default());
        let view = self.jar_view(&current, outgoing);
        let (started, clock) = (SystemTime::now(), Instant::now());
        let response = if outgoing.typed() {
            self.send_typed(
                &uri,
                &caller,
                hop > 0,
                sent.as_ref(),
                outgoing.policy.timeout,
                view,
            )
            .await?
        } else {
            self.send_shaped(
                &uri,
                &current,
                outgoing,
                &caller,
                hop > 0,
                sent.as_ref(),
                view,
            )
            .await?
        };
        let wait = clock.elapsed();
        self.note_version(&uri, response.version());
        // The headers byte for byte, and the server's own reason phrase.
        let record = sent.map(|sent| Record {
            started,
            clock,
            wait,
            sent,
            headers: response
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
                .collect(),
            reason: match response.extensions().get::<ReasonPhrase>() {
                Some(r) => String::from_utf8_lossy(r.as_ref()).into_owned(),
                None => response
                    .status()
                    .canonical_reason()
                    .unwrap_or_default()
                    .to_string(),
            },
        });
        Ok((current, response, record))
    }

    /// `hop` for any request.
    pub async fn hop_as(
        &self,
        url: &str,
        outgoing: &Outgoing,
        headers: &[(String, String)],
        hop: usize,
    ) -> Result<Hop, FetchError> {
        let (current, response, record) = self.request(url, outgoing, headers, hop).await?;
        let status = response.status().as_u16();
        let location = (response.status().is_redirection() && !outgoing.policy.dont_redirect)
            .then(|| response.headers().get(LOCATION))
            .flatten()
            .and_then(|l| l.to_str().ok())
            .map(str::to_string);
        let cookies = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        let Some(location) = location else {
            let response = into_response(current.to_string(), response, self.max_body).await?;
            self.keep(record, &current, outgoing, &response, false)
                .await;
            return Ok(Hop::Done(response));
        };
        // A redirect's body is read the same way whether or not it's
        // recorded, so the connection is reused or not alike: a short one
        // in full, which leaves the connection free for the next request,
        // and of a long or slow one only its start, the rest dropped with
        // the connection.
        let (hop_response, truncated) = drain(current.to_string(), response).await;
        self.keep(record, &current, outgoing, &hop_response, truncated)
            .await;
        let limit = self.redirect_limit(outgoing);
        if hop >= limit {
            return Err(FetchError {
                kind: FetchErrorKind::TooManyRedirects,
                message: format!("more than {limit} redirects, the last to {url}"),
                steps: Default::default(),
            });
        }
        let mut next = current
            .join(&location)
            .map_err(|_| FetchError::invalid("redirect to a bad URL".to_string()))?;
        // A login in the URL is never sent: a browser uses one only to
        // answer a 401's challenge, which Netweir doesn't, and the client
        // would send it up front. Nor kept, as the archive, the page's URL
        // and errors would then show it.
        let login = !next.username().is_empty() || next.password().is_some();
        let _ = next.set_username("");
        let _ = next.set_password(None);
        if !matches!(next.scheme(), "http" | "https") {
            return Err(FetchError::invalid(format!(
                "redirect to an unsupported URL {next}"
            )));
        }
        // The Fetch standard fails a script's redirect to a URL with a
        // login once the request has gone beyond the page's origin.
        if login
            && outgoing.kind == Kind::Fetch
            && let Some(page) = outgoing.referer_url(&current)
            && (outgoing.furthest != Relation::SameOrigin
                || page.origin() != current.origin()
                || page.origin() != next.origin())
        {
            return Err(FetchError::invalid(format!(
                "a script's request was redirected to {next} with a login in the URL, \
                 on another origin than its page's"
            )));
        }
        let then = outgoing.redirected(status, &current, &next);
        Ok(Hop::Redirect(next.into(), then, cookies))
    }

    async fn send_typed(
        &self,
        uri: &Uri,
        caller: &[(&str, HeaderName, HeaderValue)],
        redirected: bool,
        sent: Option<&Sent>,
        timeout: Option<Duration>,
        view: Option<JarView>,
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
        let mut request = self.client.get(uri.clone());
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
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
        dispatch(request, order, sent, view).await
    }

    /// Any request but typing an address: the headers of the profile's
    /// captured shape for it, with the per-request ones filled in as the
    /// browser fills them.
    #[allow(clippy::too_many_arguments)]
    async fn send_shaped(
        &self,
        uri: &Uri,
        target: &Url,
        outgoing: &Outgoing,
        caller: &[(&str, HeaderName, HeaderValue)],
        redirected: bool,
        sent: Option<&Sent>,
        view: Option<JarView>,
    ) -> Result<wreq::Response, FetchError> {
        let shapes = &self.shapes;
        let (shape, what, without) = match outgoing.kind {
            Kind::Form => (&shapes.form, "a form submission", None),
            Kind::Fetch => (&shapes.fetch, "a script's request", None),
            Kind::Image => (&shapes.image, "an image", None),
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
        let client = match (&self.script_client, &self.image_client, outgoing.kind) {
            (Some(script), _, Kind::Fetch) => script,
            (_, Some(image), Kind::Image) => image,
            _ => &self.client,
        };
        let mut request = client.request(method, uri.clone()).default_headers(false);
        if let Some(timeout) = outgoing.policy.timeout {
            request = request.timeout(timeout);
        }
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
        if self.remembers_http1 && uri.scheme_str() == Some("https") && !self.expects_http2(uri) {
            request = request.version(Version::HTTP_11);
        }
        for (_, name, value) in caller {
            request = request.header(name.clone(), value.clone());
        }
        if let Some(body) = &outgoing.body {
            request = request.body(body.clone());
        }
        dispatch(request, order, sent, view).await
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

/// The most of a redirect's body read, and how long reading it may take.
const REDIRECT_BODY: usize = 64 << 10;
const REDIRECT_WAIT: Duration = Duration::from_secs(2);

/// A redirect as a Response, with as much of its body as `REDIRECT_BODY`
/// and `REDIRECT_WAIT` allow; true if that wasn't all of it.
async fn drain(url: String, response: wreq::Response) -> (Response, bool) {
    use http_body_util::BodyExt;
    let status = response.status().as_u16();
    let version = version_name(response.version());
    let headers = response_headers(&response);
    let mut body = Vec::new();
    let mut stream = wreq::Body::from(response);
    let read = tokio::time::timeout(REDIRECT_WAIT, async {
        while let Some(frame) = stream.frame().await {
            let Ok(frame) = frame else {
                return false;
            };
            let Ok(chunk) = frame.into_data() else {
                continue; // trailers
            };
            let room = REDIRECT_BODY - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                return false;
            }
            body.extend_from_slice(&chunk);
        }
        true
    })
    .await;
    let whole = matches!(read, Ok(true));
    (
        Response {
            url,
            status,
            version,
            headers,
            body: body.into(),
            steps: Default::default(),
        },
        !whole,
    )
}

fn version_name(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "unknown",
    }
}

fn response_headers(response: &wreq::Response) -> Vec<(String, String)> {
    response
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

async fn into_response(
    url: String,
    response: wreq::Response,
    max_body: Option<u64>,
) -> Result<Response, FetchError> {
    let status = response.status().as_u16();
    let version = version_name(response.version());
    let headers = response_headers(&response);
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
        steps: Default::default(),
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
        let names = |o: &Outgoing| -> Vec<String> {
            o.callers_headers(&headers)
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        let post = Outgoing::fetch("POST", Some(b"a".to_vec()), None, None);
        assert_eq!(names(&post).len(), 3);
        let got = post.redirected(303, &start, &"https://example.com/b".parse().unwrap());
        assert_eq!(names(&got), ["Authorization", "X-Keep"]);
        let away = post.redirected(307, &start, &"https://other.org/".parse().unwrap());
        assert_eq!(names(&away), ["Content-Type", "X-Keep"]);
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
        let typed = Outgoing::navigate();
        a.add_set_cookies(
            "https://www.shop.example/",
            &typed,
            &["own=1; Path=/".into()],
        );
        a.add_set_cookies(
            "https://www.shop.example/cart/",
            &typed,
            &["deep=1; Path=/cart".into()],
        );
        a.add_set_cookies(
            "https://www.shop.example/",
            &typed,
            &["wide=1; Domain=shop.example".into()],
        );
        a.add_set_cookies("https://other.example/", &typed, &["own=2".into()]);
        a.add_set_cookies("https://api.shop.example/", &typed, &["api=1".into()]);
        let names = |f: &Fetcher| {
            let mut n: Vec<String> = f.jar().all().into_iter().map(|c| c.name).collect();
            n.sort();
            n
        };
        assert_eq!(names(&b), ["api", "deep", "own", "own", "wide"], "shared");
        b.forget("www.shop.example");
        assert_eq!(names(&a), ["api", "own"]);
        // Another jar on the same client is another visitor.
        let c = a.with_jar(Arc::new(Jar::new()));
        assert!(c.jar().is_empty());
        assert_eq!(names(&a), ["api", "own"]);
    }

    fn pairs(view: &JarView, url: &str, version: Version) -> Vec<String> {
        match view.cookies(&url.parse().unwrap(), version) {
            wreq::cookie::Cookies::Uncompressed(v) => {
                v.iter().map(|h| h.to_str().unwrap().to_owned()).collect()
            }
            wreq::cookie::Cookies::Compressed(h) => vec![h.to_str().unwrap().to_owned()],
            _ => Vec::new(),
        }
    }

    #[test]
    fn cookies_are_a_field_each_over_http2_and_one_field_over_http1() {
        let jar = Arc::new(Jar::new());
        let url = Url::parse("https://b.example/deep/x").unwrap();
        let view = JarView {
            jar: jar.clone(),
            newest_first: true,
            context: cookies::Context::typed(&url),
        };
        let set = |at: &str, cookie: &str| {
            let value = HeaderValue::from_str(cookie).unwrap();
            view.set_cookies(&mut std::iter::once(&value), &at.parse().unwrap());
        };
        set("https://a.example/", "sid=a; Path=/");
        set("https://b.example/", "t=x; Path=/");
        set("https://b.example/", "sid=b; Path=/");
        set("https://b.example/deep/", "p=1; Path=/deep");
        // Safari: newest first among equal paths.
        assert_eq!(
            pairs(&view, url.as_str(), Version::HTTP_2),
            ["p=1", "sid=b", "t=x"]
        );
        assert_eq!(
            pairs(&view, url.as_str(), Version::HTTP_11),
            ["p=1; sid=b; t=x"]
        );
        assert!(pairs(&view, "https://c.example/", Version::HTTP_2).is_empty());
    }

    #[test]
    fn a_requests_cookie_context_follows_where_it_comes_from() {
        let url = |s: &str| Url::parse(s).unwrap();
        let shop = url("https://shop.example/cart");
        let typed = Outgoing::navigate().cookie_context(&shop);
        assert!(!typed.cross_site && typed.navigation && typed.safe_method);
        let link = Outgoing {
            referer: Some("https://elsewhere.example/".into()),
            ..Outgoing::navigate()
        };
        let link = link.cookie_context(&shop);
        assert!(link.cross_site && link.navigation);
        assert_eq!(link.top_level_site.as_deref(), Some("https://shop.example"));
        let script = Outgoing::fetch(
            "POST",
            None,
            None,
            Some("https://www.elsewhere.example/page".into()),
        )
        .cookie_context(&shop);
        assert!(script.cross_site && !script.navigation && !script.safe_method);
        assert_eq!(
            script.top_level_site.as_deref(),
            Some("https://elsewhere.example")
        );
        let same = Outgoing::fetch("GET", None, None, Some("https://www.shop.example/".into()))
            .cookie_context(&shop);
        assert!(!same.cross_site);
    }
}
