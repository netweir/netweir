//! What a page fetches: requests and responses as Chrome reports them,
//! and the rules that block, change or answer requests before they leave
//! Chrome. The design is in docs/design/network.md.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::browser::str_of;
use crate::conn::Connection;
use crate::{Error, Result};

/// Resource types as netweir names them, with Chrome's name for each.
const RESOURCE_TYPES: &[(&str, &str)] = &[
    ("document", "Document"),
    ("stylesheet", "Stylesheet"),
    ("image", "Image"),
    ("media", "Media"),
    ("font", "Font"),
    ("script", "Script"),
    ("texttrack", "TextTrack"),
    ("xhr", "XHR"),
    // Known limit: before a request leaves, Chrome calls fetch() calls
    // XHR too, so blocking either blocks both, and a route sees "xhr" for
    // both. Only once the response is in does it tell them apart.
    ("fetch", "XHR"),
    ("prefetch", "Prefetch"),
    ("eventsource", "EventSource"),
    ("websocket", "WebSocket"),
    ("manifest", "Manifest"),
    ("signedexchange", "SignedExchange"),
    ("ping", "Ping"),
    ("cspviolationreport", "CSPViolationReport"),
    ("preflight", "Preflight"),
    ("other", "Other"),
];

/// The resource types [`crate::Page::block`] takes, as netweir names them.
pub fn resource_types() -> impl Iterator<Item = &'static str> {
    RESOURCE_TYPES.iter().map(|(ours, _)| *ours)
}

/// Chrome's name for one of netweir's resource types.
pub(crate) fn chrome_type(ours: &str) -> Result<&'static str> {
    RESOURCE_TYPES
        .iter()
        .find(|(o, _)| *o == ours)
        .map(|(_, chrome)| *chrome)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "unknown resource type {ours:?}; known ones are {}",
                resource_types().collect::<Vec<_>>().join(", ")
            ))
        })
}

/// A request the page made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Chrome's id for it, which its response shares.
    pub id: String,
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    /// "document", "xhr", "fetch", "image" and so on.
    pub resource_type: String,
    pub post_data: Option<Vec<u8>>,
}

impl Request {
    /// From a `Network.Request` and the resource type Chrome gave it.
    pub(crate) fn from_cdp(id: String, request: &Value, resource_type: &Value) -> Request {
        let post_data = match request["postDataEntries"].as_array() {
            Some(entries) => Some(
                entries
                    .iter()
                    .filter_map(|e| e["bytes"].as_str())
                    .flat_map(|b| {
                        base64::engine::general_purpose::STANDARD
                            .decode(b)
                            .unwrap_or_default()
                    })
                    .collect(),
            ),
            None => request["postData"].as_str().map(|s| s.as_bytes().to_vec()),
        };
        Request {
            id,
            url: str_of(request, "url"),
            method: str_of(request, "method"),
            headers: headers_of(&request["headers"]),
            resource_type: resource_type
                .as_str()
                .unwrap_or("Other")
                .to_ascii_lowercase(),
            post_data,
        }
    }
}

/// A `Network.Headers` object as pairs.
pub(crate) fn headers_of(headers: &Value) -> Vec<(String, String)> {
    headers
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
        .collect()
}

/// What the page told its listeners about.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    Request(Request),
    Response(crate::Response),
}

/// A request the page's routes get to decide about. It has to be decided
/// once: aborted, continued or fulfilled. One dropped undecided goes on
/// unchanged, so a request is never left waiting on a route nobody
/// answers.
#[derive(Clone)]
pub struct Route {
    inner: Arc<RouteInner>,
}

struct RouteInner {
    /// The Fetch domain's id for the pause, not the request's own.
    paused: String,
    request: Request,
    conn: Connection,
    timeout: std::time::Duration,
    session: String,
    decided: AtomicBool,
    /// The guard, for a main-frame document a guard let through: a route
    /// that sends it elsewhere has the new URL put to the guard too.
    guard: Option<crate::Guard>,
    /// Told when it's decided, so its timer can stop.
    settled: Arc<tokio::sync::Notify>,
}

/// Changes [`Route::continue_`] makes to a request on its way out.
#[derive(Debug, Clone, Default)]
pub struct Continue {
    pub url: Option<String>,
    pub method: Option<String>,
    /// Replaces every header the request had.
    pub headers: Option<Vec<(String, String)>>,
    pub body: Option<Vec<u8>>,
}

impl Route {
    pub fn request(&self) -> &Request {
        &self.inner.request
    }

    /// Whether it's been aborted, continued or fulfilled.
    pub fn is_decided(&self) -> bool {
        self.inner.decided.load(Ordering::Relaxed)
    }

    /// Fails the request before it leaves Chrome; the page sees a network
    /// error.
    pub async fn abort(&self) -> Result<()> {
        self.decide(
            "Fetch.failRequest",
            json!({"requestId": self.inner.paused, "errorReason": "Failed"}),
        )
        .await
    }

    /// Lets the request go on, with any changes `with` makes.
    /// A document the page's guard let through, sent to a new URL, is
    /// failed as the guard fails one unless the guard allows that URL too.
    pub async fn continue_(&self, with: Continue) -> Result<()> {
        let mut params = json!({"requestId": self.inner.paused});
        if let Some(url) = with.url {
            if let Some(guard) = &self.inner.guard
                && url != self.inner.request.url
                && !self.is_decided()
                && !guard(url.clone()).await
            {
                return self
                    .decide(
                        "Fetch.failRequest",
                        json!({"requestId": self.inner.paused, "errorReason": "BlockedByClient"}),
                    )
                    .await;
            }
            params["url"] = Value::from(url);
        }
        if let Some(method) = with.method {
            params["method"] = Value::from(method);
        }
        if let Some(headers) = with.headers {
            params["headers"] = entries(&headers);
        }
        if let Some(body) = with.body {
            params["postData"] =
                Value::from(base64::engine::general_purpose::STANDARD.encode(body));
        }
        self.decide("Fetch.continueRequest", params).await
    }

    /// Answers the request without it leaving Chrome.
    pub async fn fulfill(
        &self,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<()> {
        self.decide(
            "Fetch.fulfillRequest",
            json!({
                "requestId": self.inner.paused,
                "responseCode": status,
                "responseHeaders": entries(headers),
                "body": base64::engine::general_purpose::STANDARD.encode(body),
            }),
        )
        .await
    }

    async fn decide(&self, method: &str, params: Value) -> Result<()> {
        let inner = &self.inner;
        if inner.decided.swap(true, Ordering::Relaxed) {
            return Err(Error::Gone(format!(
                "the request for {} was already decided, or let go unchanged when its \
                 handler ran out of time",
                inner.request.url
            )));
        }
        let answer = inner
            .conn
            .call_within(inner.timeout, &inner.session, method, params);
        let outcome = match answer.await {
            Ok(_) => Ok(()),
            // The page, the frame or the browser closed under the
            // request: there's nothing left to decide.
            Err(e) if went_away(&e) => Ok(()),
            // Chrome refused it (a bad header, say): the request is still
            // waiting, and goes on unchanged when the route lets go.
            Err(e @ Error::Protocol { .. }) => {
                inner.decided.store(false, Ordering::Relaxed);
                return Err(e);
            }
            Err(e) => Err(e),
        };
        inner.settled.notify_one();
        outcome
    }

    /// Lets the request go on unchanged, unless it's been decided.
    pub fn release(&self) {
        self.inner.release();
    }
}

impl RouteInner {
    fn release(&self) {
        if !self.decided.swap(true, Ordering::Relaxed) {
            self.conn.send(
                &self.session,
                "Fetch.continueRequest",
                json!({"requestId": self.paused}),
            );
        }
        self.settled.notify_one();
    }
}

impl Drop for RouteInner {
    fn drop(&mut self) {
        self.release();
    }
}

/// Whether `e` says the session it was sent to has gone: the browser
/// closed, or the page or frame went away before Chrome answered.
pub(crate) fn went_away(e: &Error) -> bool {
    match e {
        Error::Closed | Error::PageClosed => true,
        Error::Protocol { message, .. } => {
            message.contains("Session with given id") || message.contains("session went away")
        }
        _ => false,
    }
}

fn entries(headers: &[(String, String)]) -> Value {
    headers
        .iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect()
}

/// What a page, or every page of a context, blocks and routes.
#[derive(Default)]
pub(crate) struct Rules {
    /// Chrome's names for the resource types blocked.
    pub types: Vec<&'static str>,
    pub urls: Vec<String>,
    /// Where requests go while there are routes.
    pub routes: Option<mpsc::UnboundedSender<Route>>,
}

impl Rules {
    fn blocks(&self, request: &Request, chrome_type: &str) -> bool {
        self.types.contains(&chrome_type) || self.urls.iter().any(|g| glob_match(g, &request.url))
    }

    /// Whether every request has to be paused, not only some types.
    pub(crate) fn pauses_all(&self) -> bool {
        self.routes.is_some() || !self.urls.is_empty()
    }
}

pub(crate) type SharedRules = Arc<Mutex<Rules>>;

pub(crate) fn lock(rules: &SharedRules) -> std::sync::MutexGuard<'_, Rules> {
    rules.lock().unwrap_or_else(|e| e.into_inner())
}

/// Decides what happens to a request the Fetch domain paused, once the
/// guard (if there is one) has let it through: blocked, handed to a
/// route, or let go. One serves a page and every session it has: its
/// own, and those of its frames from other sites.
pub(crate) struct Decider {
    pub conn: Connection,
    /// The browser's timeout, which route handlers get to decide in.
    pub timeout: std::time::Duration,
    /// The page's own session, whose main frame the guard sees.
    pub session: String,
    /// The page's own rules, then its context's.
    pub rules: Vec<SharedRules>,
    /// For the timers that let go of requests no route decided about.
    pub runtime: tokio::runtime::Handle,
    /// The page's guard, and its main frame, whose documents it sees.
    pub guard: Option<crate::Guard>,
    pub frame: String,
    /// Whether the proxy has a login, whose challenges Fetch answers.
    pub login: bool,
    /// Whether the Fetch domain is on, held while any of the page's
    /// sessions has it changed, so none is left with patterns from
    /// before the rules last changed.
    pub fetching: tokio::sync::Mutex<bool>,
}

impl Decider {
    /// What the Fetch domain has to pause for the page's rules, its
    /// context's, its guard and its proxy's login, and nothing else: no
    /// patterns at all when nothing needs it. Every session of the page
    /// gets the same.
    pub(crate) fn patterns(&self) -> Vec<Value> {
        let mut all = self.login;
        let mut types: Vec<&str> = Vec::new();
        if self.guard.is_some() {
            types.push("Document");
        }
        for rules in &self.rules {
            let rules = lock(rules);
            all |= rules.pauses_all();
            types.extend(rules.types.iter().copied());
        }
        types.sort_unstable();
        types.dedup();
        // To answer the proxy's challenges, Chrome has every request
        // paused, each released as it comes: a round trip per request,
        // only for a proxy with a login. Routes and blocked URLs need
        // every request paused too; a guard and blocked resource types
        // only their own.
        if all {
            vec![json!({"urlPattern": "*"})]
        } else {
            types
                .iter()
                .map(|t| json!({"urlPattern": "*", "resourceType": t}))
                .collect()
        }
    }

    /// `Fetch.enable` with `patterns`, as each session gets it.
    pub(crate) fn enable(&self, patterns: Vec<Value>) -> Value {
        json!({"handleAuthRequests": self.login, "patterns": patterns})
    }

    /// Decides about a request `session` paused.
    pub(crate) fn decide(&self, session: &str, params: &Value) {
        let paused = str_of(params, "requestId");
        let chrome_type = params["resourceType"].as_str().unwrap_or("Other");
        let id = params["networkId"]
            .as_str()
            .map_or_else(|| paused.clone(), str::to_string);
        let request = Request::from_cdp(id, &params["request"], &params["resourceType"]);
        let mut routes = None;
        for rules in &self.rules {
            let rules = lock(rules);
            if rules.blocks(&request, chrome_type) {
                self.conn.send(
                    session,
                    "Fetch.failRequest",
                    json!({"requestId": paused, "errorReason": "BlockedByClient"}),
                );
                return;
            }
            if routes.is_none() {
                routes = rules.routes.clone().filter(|tx| !tx.is_closed());
            }
        }
        // Only the page's own session has its main frame: a frame from
        // another site never fetches a document for it.
        let guarded = session == self.session
            && chrome_type == "Document"
            && params["frameId"] == self.frame.as_str();
        let settled = Arc::new(tokio::sync::Notify::new());
        let route = Route {
            inner: Arc::new(RouteInner {
                paused,
                request,
                conn: self.conn.clone(),
                timeout: self.timeout,
                session: session.to_string(),
                decided: AtomicBool::new(false),
                guard: self.guard.clone().filter(|_| guarded),
                settled: settled.clone(),
            }),
        };
        let Some(routes) = routes else {
            // Dropped undecided: it goes on.
            return;
        };
        let waiting = Arc::downgrade(&route.inner);
        // A route that sent nowhere is dropped, and so let go.
        let _ = routes.send(route);
        // A handler that never decides gets the browser's timeout, then
        // the request goes on without it. The timer ends with the decision.
        let limit = self.timeout;
        self.runtime.spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(limit) => {
                    if let Some(route) = waiting.upgrade() {
                        route.release();
                    }
                }
                () = settled.notified() => {}
            }
        });
    }
}

/// Whether `url` matches the glob `pattern` as a whole: `*` is anything
/// but `/`, `**` is anything at all, `{a,b}` is either, `\` escapes, and
/// every other character, `?` included, is itself.
pub fn glob_match(pattern: &str, url: &str) -> bool {
    let url: Vec<char> = url.chars().collect();
    expand(pattern).iter().any(|p| matches(p, &url))
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Char(char),
    /// `*`
    Segment,
    /// `**`
    Anything,
}

/// The pattern with its `{a,b}` alternatives spelled out, one token list
/// each. Known limit: alternatives don't nest; a `{` inside one is itself.
fn expand(pattern: &str) -> Vec<Vec<Token>> {
    let mut out: Vec<Vec<Token>> = vec![Vec::new()];
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let c = chars.next().unwrap_or('\\');
                out.iter_mut().for_each(|p| p.push(Token::Char(c)));
            }
            '*' => {
                let token = if chars.peek() == Some(&'*') {
                    chars.next();
                    Token::Anything
                } else {
                    Token::Segment
                };
                out.iter_mut().for_each(|p| p.push(token.clone()));
            }
            '{' => {
                let mut options = vec![Vec::new()];
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '}' => {
                            closed = true;
                            break;
                        }
                        ',' => options.push(Vec::new()),
                        '\\' => {
                            let c = chars.next().unwrap_or('\\');
                            options.last_mut().expect("one").push(Token::Char(c));
                        }
                        '*' => {
                            let token = if chars.peek() == Some(&'*') {
                                chars.next();
                                Token::Anything
                            } else {
                                Token::Segment
                            };
                            options.last_mut().expect("one").push(token);
                        }
                        c => options.last_mut().expect("one").push(Token::Char(c)),
                    }
                }
                if !closed {
                    // An unclosed brace is just a character, and what
                    // followed it is too.
                    let mut literal = vec![Token::Char('{')];
                    for (i, option) in options.into_iter().enumerate() {
                        if i > 0 {
                            literal.push(Token::Char(','));
                        }
                        literal.extend(option);
                    }
                    out.iter_mut().for_each(|p| p.extend(literal.clone()));
                    continue;
                }
                out = out
                    .into_iter()
                    .flat_map(|p| {
                        options.iter().map(move |o| {
                            let mut p = p.clone();
                            p.extend(o.iter().cloned());
                            p
                        })
                    })
                    .collect();
            }
            c => out.iter_mut().for_each(|p| p.push(Token::Char(c))),
        }
    }
    out
}

/// Matches token by token, keeping the set of places in `url` each prefix
/// of the pattern can end at: no backtracking, so no pattern is slow.
fn matches(pattern: &[Token], url: &[char]) -> bool {
    let mut at = vec![false; url.len() + 1];
    at[0] = true;
    for token in pattern {
        let mut next = vec![false; url.len() + 1];
        match token {
            Token::Char(c) => {
                for i in 0..url.len() {
                    if at[i] && url[i] == *c {
                        next[i + 1] = true;
                    }
                }
            }
            Token::Segment => {
                let mut on = false;
                for i in 0..=url.len() {
                    on = at[i] || (on && url[i - 1] != '/');
                    next[i] = on;
                }
            }
            Token::Anything => {
                if let Some(first) = at.iter().position(|&a| a) {
                    next[first..].iter_mut().for_each(|n| *n = true);
                }
            }
        }
        at = next;
    }
    at[url.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_whole_urls() {
        let url = "https://shop.test/api/items?page=2";
        assert!(glob_match("**/api/*", url));
        assert!(glob_match("**/api/items*", url));
        assert!(glob_match("https://shop.test/api/items?page=2", url));
        assert!(!glob_match("**/api", url), "the whole URL, not part");
        assert!(!glob_match("https://*/items*", url), "* stops at a slash");
        assert!(glob_match("https://*/**", url));
        assert!(glob_match("**", url));
        assert!(!glob_match("**/api/items?page=3", url), "? is itself");
    }

    #[test]
    fn braces_offer_alternatives_and_backslashes_escape() {
        assert!(glob_match("**/*.{png,jpg}", "http://a.test/x/y.jpg"));
        assert!(glob_match("**/*.{png,jpg}", "http://a.test/y.png"));
        assert!(!glob_match("**/*.{png,jpg}", "http://a.test/y.gif"));
        assert!(glob_match(r"**/\*", "http://a.test/*"));
        assert!(!glob_match(r"**/\*", "http://a.test/a"));
        assert!(glob_match("**/{a", "http://a.test/{a"));
    }

    #[test]
    fn unknown_resource_types_say_which_are_known() {
        assert_eq!(chrome_type("xhr").unwrap(), "XHR");
        let err = chrome_type("images").unwrap_err().to_string();
        assert!(
            err.contains("\"images\"") && err.contains("image,"),
            "{err}"
        );
    }
}
