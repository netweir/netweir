//! A page: one target, one session, its navigation state and its actions.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::browser::{Inner, context_cookies, set_context_cookies, str_of};
use crate::conn::{Connection, GONE, Handler};
use crate::frame::Frame;
use crate::network::{
    Decider, NetworkEvent, Request, Route, SharedRules, chrome_type, headers_of, lock,
};
use crate::{Error, Result};

/// The lifecycle event a navigation waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitUntil {
    DomContentLoaded,
    Load,
    /// No network requests for 500 ms.
    NetworkIdle,
}

impl WaitUntil {
    fn event(self) -> &'static str {
        match self {
            WaitUntil::DomContentLoaded => "DOMContentLoaded",
            WaitUntil::Load => "load",
            WaitUntil::NetworkIdle => "networkIdle",
        }
    }
}

impl FromStr for WaitUntil {
    type Err = Error;

    fn from_str(s: &str) -> Result<WaitUntil> {
        match s {
            "domcontentloaded" => Ok(WaitUntil::DomContentLoaded),
            "load" => Ok(WaitUntil::Load),
            "networkidle" => Ok(WaitUntil::NetworkIdle),
            _ => Err(Error::Invalid(format!(
                "wait must be \"domcontentloaded\", \"load\" or \"networkidle\", not {s:?}"
            ))),
        }
    }
}

/// A response the page got: the main document's to a navigation, or any
/// other a listener heard about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub url: String,
    /// 0 when there was no HTTP response (about:, data:, chrome:).
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Chrome's id for the document's load, which
    /// [`Page::wait_for_navigation`] waits past.
    pub loader: String,
    /// Chrome's id for the request, for [`Page::body`]; empty when there
    /// was no response.
    pub request_id: String,
    /// The request's method.
    pub method: String,
    /// "document", "xhr", "fetch", "image" and so on.
    pub resource_type: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Seconds since the epoch; -1 for a session cookie.
    pub expires: f64,
    pub http_only: bool,
    pub secure: bool,
    /// "Strict", "Lax" or "None", when set.
    pub same_site: Option<String>,
}

pub(crate) fn cookies_from(list: &Value) -> Vec<Cookie> {
    list.as_array()
        .into_iter()
        .flatten()
        .map(|c| Cookie {
            name: str_of(c, "name"),
            value: str_of(c, "value"),
            domain: str_of(c, "domain"),
            path: str_of(c, "path"),
            expires: c["expires"].as_f64().unwrap_or(-1.0),
            http_only: c["httpOnly"].as_bool().unwrap_or(false),
            secure: c["secure"].as_bool().unwrap_or(false),
            same_site: c["sameSite"].as_str().map(str::to_string),
        })
        .collect()
}

pub(crate) fn cookies_to(cookies: &[Cookie]) -> Value {
    cookies
        .iter()
        .map(|c| {
            let mut v = json!({
                "name": c.name,
                "value": c.value,
                "domain": c.domain,
                "path": if c.path.is_empty() { "/" } else { &c.path },
                "httpOnly": c.http_only,
                "secure": c.secure,
            });
            if c.expires >= 0.0 {
                v["expires"] = Value::from(c.expires);
            }
            if let Some(s) = &c.same_site {
                v["sameSite"] = Value::from(s.as_str());
            }
            v
        })
        .collect()
}

/// A page. Cheap to clone; the page stays open until [`Page::close`].
#[derive(Clone)]
pub struct Page {
    pub(crate) inner: Arc<PageInner>,
}

pub(crate) struct PageInner {
    browser: Arc<Inner>,
    target: String,
    pub(crate) session: String,
    context: Option<String>,
    /// Whether closing the page disposes of its context.
    owns_context: bool,
    shared: Arc<Shared>,
    /// netweir's isolated worlds, by (session, frame id): the loader of the
    /// document each was made for (the main frame's; empty for others,
    /// whose worlds are made again when they're found gone), and its
    /// context id.
    pub(crate) worlds: tokio::sync::Mutex<HashMap<(String, String), (String, i64)>>,
    closed: AtomicBool,
    /// What the page blocks and routes.
    rules: SharedRules,
    /// What its context blocks and routes, for a shared context.
    shared_context: Option<Arc<ContextRules>>,
    /// Whether a guard sees the main frame's documents first.
    guarded: bool,
    /// Whether the Fetch domain is on, held while it's changed.
    fetching: tokio::sync::Mutex<bool>,
}

/// The rules of a shared context, and its pages, to tell when they
/// change.
#[derive(Default)]
pub(crate) struct ContextRules {
    pub rules: SharedRules,
    pages: Mutex<Vec<Weak<PageInner>>>,
}

impl ContextRules {
    /// The context's pages still open.
    pub(crate) fn pages(&self) -> Vec<Page> {
        let mut pages = self.pages.lock().unwrap_or_else(|e| e.into_inner());
        pages.retain(|p| p.strong_count() > 0);
        pages
            .iter()
            .filter_map(Weak::upgrade)
            .map(|inner| Page { inner })
            .collect()
    }
}

/// What the reader thread updates and the waiters watch.
struct Shared {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) frame: String,
    url: String,
    /// The current document's loader.
    pub(crate) loader: String,
    /// Loaders in the order their documents were committed.
    committed: Vec<String>,
    /// Lifecycle events seen, by loader.
    events: HashMap<String, Vec<String>>,
    responses: HashMap<String, Response>,
    /// Requests still loading, with their methods, by request id.
    loading: HashMap<String, String>,
    /// Where requests and responses go, while anyone listens.
    listeners: Vec<tokio::sync::mpsc::UnboundedSender<NetworkEvent>>,
    /// Frames Chrome runs in processes of their own (from other sites),
    /// each with a session: (session, frame id), in the order they came.
    pub(crate) children: Vec<(String, String)>,
    gone: bool,
}

/// How far one notch of a mouse wheel scrolls, in pixels: 100 on Windows
/// and Linux, about that on a Mac.
pub(crate) const NOTCH: f64 = 100.0;

/// How many documents' events and responses are kept.
const KEPT: usize = 16;

/// How many requests still loading are kept track of. Known limit: past
/// that (a page holding thousands of requests open), the record starts
/// again, and a response whose request was forgotten says GET for its
/// method and has its body read without waiting for it to finish.
const LOADING: usize = 4096;

impl State {
    fn on(&mut self, method: &str, p: &Value) {
        match method {
            "Page.lifecycleEvent" if p["frameId"] == self.frame.as_str() => {
                let loader = str_of(p, "loaderId");
                self.events
                    .entry(loader)
                    .or_default()
                    .push(str_of(p, "name"));
            }
            "Page.frameNavigated" if p["frame"].get("parentId").is_none() => {
                let frame = &p["frame"];
                self.frame = str_of(frame, "id");
                self.url = str_of(frame, "url");
                self.loader = str_of(frame, "loaderId");
                self.committed.push(self.loader.clone());
                if self.committed.len() > KEPT {
                    let old = self.committed.remove(0);
                    self.events.remove(&old);
                    self.responses.remove(&old);
                }
            }
            // A frame committing here is in the page's process now: any
            // session it had in a process of its own is on its way out,
            // and may never answer again.
            "Page.frameNavigated" => {
                let id = p["frame"]["id"].as_str().unwrap_or_default();
                self.children.retain(|(_, frame)| frame != id);
            }
            "Page.navigatedWithinDocument" if p["frameId"] == self.frame.as_str() => {
                self.url = str_of(p, "url");
            }
            "Network.requestWillBeSent" => {
                let id = str_of(p, "requestId");
                let request = Request::from_cdp(id.clone(), &p["request"], &p["type"]);
                if self.loading.len() >= LOADING {
                    self.loading.clear();
                }
                self.loading.insert(id, request.method.clone());
                self.tell(NetworkEvent::Request(request));
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                self.loading
                    .remove(p["requestId"].as_str().unwrap_or_default());
            }
            "Network.responseReceived" => {
                let r = &p["response"];
                let id = str_of(p, "requestId");
                let response = Response {
                    url: str_of(r, "url"),
                    status: r["status"].as_u64().unwrap_or(0) as u16,
                    headers: headers_of(&r["headers"]),
                    loader: str_of(p, "loaderId"),
                    method: self
                        .loading
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| "GET".into()),
                    request_id: id,
                    resource_type: p["type"].as_str().unwrap_or("Other").to_ascii_lowercase(),
                };
                if p["type"] == "Document" && p["frameId"] == self.frame.as_str() {
                    // Navigations that never commit (a 204, a download)
                    // leave responses behind; only committed ones are kept.
                    let committed = &self.committed;
                    self.responses.retain(|l, _| committed.contains(l));
                    self.responses
                        .insert(response.loader.clone(), response.clone());
                }
                self.tell(NetworkEvent::Response(response));
            }
            "Inspector.targetCrashed" | GONE => {
                self.gone = true;
                // Listeners hear there's nothing more: their receivers end.
                self.listeners.clear();
            }
            _ => {}
        }
    }

    /// Passes `event` to whoever is listening; nothing is kept for later.
    fn tell(&mut self, event: NetworkEvent) {
        if !self.listeners.is_empty() {
            self.listeners.retain(|l| l.send(event.clone()).is_ok());
        }
    }

    /// Whether a document committed after `loader`'s has reached `event`.
    fn reached_after(&self, loader: &str, event: &str) -> bool {
        let Some(at) = self.committed.iter().position(|l| l == loader) else {
            return false;
        };
        self.committed[at + 1..].iter().any(|l| {
            self.events
                .get(l)
                .is_some_and(|e| e.iter().any(|n| n == event))
        })
    }

    /// Whether `loader`'s navigation, or one that replaced it, has reached
    /// `event`.
    fn reached(&self, loader: &str, event: &str) -> bool {
        let from = self.committed.iter().position(|l| l == loader);
        let Some(from) = from else { return false };
        self.committed[from..].iter().any(|l| {
            self.events
                .get(l)
                .is_some_and(|e| e.iter().any(|n| n == event))
        })
    }
}

impl Page {
    pub(crate) async fn open(
        browser: Arc<Inner>,
        context: Option<String>,
        owns_context: bool,
        guard: Option<crate::browser::Guard>,
        shared_context: Option<Arc<ContextRules>>,
    ) -> Result<Page> {
        let conn = &browser.conn;
        let mut params = json!({"url": "about:blank"});
        if let Some(id) = &context {
            params["browserContextId"] = Value::from(id.as_str());
        }
        let target = str_of(
            &browser.call("", "Target.createTarget", params).await?,
            "targetId",
        );
        let session = str_of(
            &browser
                .call(
                    "",
                    "Target.attachToTarget",
                    json!({"targetId": target, "flatten": true}),
                )
                .await?,
            "sessionId",
        );
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        });
        let watched = shared.clone();
        let rules = SharedRules::default();
        let mut all_rules = vec![rules.clone()];
        all_rules.extend(shared_context.as_ref().map(|c| c.rules.clone()));
        // Holds the connection, not the browser: the connection holds the
        // handler that holds this, and the browser would never be let go.
        let decider = Arc::new(Decider {
            conn: conn.clone(),
            timeout: browser.timeout,
            session: session.clone(),
            rules: all_rules,
            runtime: tokio::runtime::Handle::current(),
            guard: guard.clone(),
            frame: target.clone(),
        });
        let is_guarded = guard.is_some();
        // Documents the guard must see first, answered on a task of their
        // own, since the guard is async and the reader thread can't wait.
        // What it lets through goes on to the page's rules, so no route
        // sees, let alone answers, a document the guard refused.
        let guarded = guard.map(|guard| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
            let (conn, session) = (conn.clone(), session.clone());
            let decider = decider.clone();
            tokio::spawn(async move {
                while let Some(params) = rx.recv().await {
                    if guard(str_of(&params["request"], "url")).await {
                        decider.decide(&params);
                    } else {
                        conn.send(
                            &session,
                            "Fetch.failRequest",
                            json!({"requestId": str_of(&params, "requestId"), "errorReason": "BlockedByClient"}),
                        );
                    }
                }
            });
            tx
        });
        let intercept = Intercept {
            conn: conn.clone(),
            session: session.clone(),
            credentials: browser.proxy_login.clone(),
            guarded,
            frame: target.clone(),
            decider,
        };
        let adopting = (conn.clone(), shared.clone());
        let handler: Handler = Arc::new(move |method, params| {
            match method {
                "Fetch.authRequired" => return intercept.answer(params),
                "Fetch.requestPaused" => return intercept.release(params),
                "Target.attachedToTarget" | "Target.detachedFromTarget" => {
                    return children(&adopting.0, &adopting.1, method, params);
                }
                _ => {}
            }
            watched
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .on(method, params);
            watched.changed.notify_waiters();
        });
        conn.on(&session, handler);
        let page = Page {
            inner: Arc::new(PageInner {
                browser: browser.clone(),
                target,
                session,
                context,
                owns_context,
                shared,
                worlds: tokio::sync::Mutex::new(HashMap::new()),
                closed: AtomicBool::new(false),
                rules,
                shared_context: shared_context.clone(),
                guarded: is_guarded,
                fetching: tokio::sync::Mutex::new(false),
            }),
        };
        if let Some(c) = &shared_context {
            c.pages
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Arc::downgrade(&page.inner));
        }
        let setup = async {
            page.call("Page.enable", json!({})).await?;
            page.call("Page.setLifecycleEventsEnabled", json!({"enabled": true}))
                .await?;
            page.call("Network.enable", json!({})).await?;
            page.refresh_fetch().await?;
            // Frames from other sites run in processes of their own, and
            // are reached through sessions of their own.
            page.call("Target.setAutoAttach", auto_attach()).await?;
            if let Some(identity) = &browser.identity {
                page.call(
                    "Network.setUserAgentOverride",
                    json!({"userAgent": identity.user_agent, "userAgentMetadata": identity.metadata}),
                )
                .await?;
            }
            let tree = page.call("Page.getFrameTree", json!({})).await?;
            let frame = &tree["frameTree"]["frame"];
            let mut state = page.state();
            if state.frame.is_empty() {
                state.frame = str_of(frame, "id");
                state.url = str_of(frame, "url");
                state.loader = str_of(frame, "loaderId");
                let loader = state.loader.clone();
                state.committed.push(loader);
            }
            Ok(())
        }
        .await;
        if let Err(e) = setup {
            let _ = page.close().await;
            return Err(e);
        }
        Ok(page)
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Sends `method` to the page and waits for Chrome's answer, for no
    /// longer than the browser's timeout: a renderer kept busy by the
    /// page's own script may never answer at all.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_on(&self.inner.session, method, params).await
    }

    /// `call`, to the session of one of the page's frames.
    pub(crate) async fn call_on(
        &self,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return Err(Error::PageClosed);
        }
        self.inner.browser.call(session, method, params).await
    }

    /// `call`, waiting as long as Chrome takes.
    async fn call_unbounded(&self, method: &str, params: Value) -> Result<Value> {
        self.call_unbounded_on(&self.inner.session, method, params)
            .await
    }

    pub(crate) async fn call_unbounded_on(
        &self,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return Err(Error::PageClosed);
        }
        self.inner.browser.conn.call(session, method, params).await
    }

    /// Waits until `ready` holds for the page's state, the page goes away
    /// (Closed), or `timeout` runs out (Timeout, saying `what`).
    async fn until(
        &self,
        what: impl Fn() -> String,
        timeout: Duration,
        ready: impl Fn(&State) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let changed = self.inner.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state();
                if ready(&state) {
                    return Ok(());
                }
                if state.gone {
                    return Err(if self.inner.browser.conn.is_closed() {
                        Error::Closed
                    } else {
                        Error::PageClosed
                    });
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return Err(Error::Timeout(what()));
            }
        }
    }

    /// The limit for one wait, at most a year: a longer one would overflow
    /// the clock.
    pub(crate) fn timeout(&self, timeout: Option<Duration>) -> Duration {
        timeout
            .unwrap_or(self.inner.browser.timeout)
            .min(Duration::from_secs(365 * 24 * 3600))
    }

    /// Navigates and waits for `wait`. An HTTP error status is returned,
    /// not raised; a network error is [`Error::Navigation`].
    pub async fn goto(
        &self,
        url: &str,
        wait: WaitUntil,
        timeout: Option<Duration>,
    ) -> Result<Response> {
        let deadline = Instant::now() + self.timeout(timeout);
        // Held to this navigation's own deadline, which may be longer than
        // the browser's timeout.
        let navigate = self.call_unbounded("Page.navigate", json!({"url": url}));
        let r = tokio::time::timeout_at(deadline, navigate)
            .await
            .map_err(|_| Error::Timeout(format!("navigating to {url}")))??;
        if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(Error::Navigation {
                url: url.to_string(),
                error: e.to_string(),
                // Chrome doesn't say why it gave up on a navigation; its
                // stderr sometimes does.
                said: self.inner.browser.said.as_ref().and_then(|s| s.recent()),
            });
        }
        let Some(loader) = r["loaderId"].as_str().map(str::to_string) else {
            // Within the same document (a #fragment): nothing to wait for.
            return Ok(self.response_for(None));
        };
        let event = wait.event();
        self.until(
            || format!("{event} of {url}"),
            deadline.saturating_duration_since(Instant::now()),
            |s| s.reached(&loader, event),
        )
        .await?;
        Ok(self.response_for(Some(&loader)))
    }

    /// The response of the document the page shows now.
    pub fn response(&self) -> Response {
        self.response_for(None)
    }

    /// Waits until a document that replaced `after` (by a script's
    /// redirect, a form, a challenge passing) reaches `wait`, and returns
    /// the response of the document showing then. Returns at once if that
    /// has already happened. Take `after` from [`Page::goto`] or
    /// [`Page::response`] before whatever is expected to navigate.
    pub async fn wait_for_navigation(
        &self,
        after: &Response,
        wait: WaitUntil,
        timeout: Option<Duration>,
    ) -> Result<Response> {
        let event = wait.event();
        self.until(
            || format!("{event} of a navigation away from {}", after.url),
            self.timeout(timeout),
            |s| s.reached_after(&after.loader, event),
        )
        .await?;
        Ok(self.response_for(None))
    }

    fn response_for(&self, loader: Option<&str>) -> Response {
        let state = self.state();
        let loader = loader.unwrap_or(&state.loader);
        state
            .responses
            .get(loader)
            .cloned()
            .unwrap_or_else(|| Response {
                url: state.url.clone(),
                status: 0,
                headers: Vec::new(),
                loader: loader.to_string(),
                request_id: String::new(),
                method: "GET".into(),
                resource_type: "document".into(),
            })
    }

    /// Hears about every request the page makes and every response it
    /// gets, from now until the receiver is dropped or the page closes.
    /// Nothing is kept for a page nobody listens to.
    pub fn listen(&self) -> tokio::sync::mpsc::UnboundedReceiver<NetworkEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = self.state();
        // A page that's gone has nothing more to say: the receiver ends.
        if !state.gone && !self.inner.closed.load(Ordering::Relaxed) {
            state.listeners.push(tx);
        }
        rx
    }

    /// The body of a response the page got, once it has all arrived.
    /// Chrome keeps bodies for the document showing now, and only so many
    /// bytes of them, so the body of a response from before a navigation,
    /// or of one Chrome let go of, is [`Error::Gone`].
    pub async fn body(&self, response: &Response) -> Result<Vec<u8>> {
        let id = &response.request_id;
        if id.is_empty() {
            return Err(Error::Gone(format!(
                "{} has no body: there was no response",
                response.url
            )));
        }
        self.until(
            || format!("the body of {}", response.url),
            self.timeout(None),
            |s| !s.loading.contains_key(id),
        )
        .await?;
        let r = match self
            .call("Network.getResponseBody", json!({"requestId": id}))
            .await
        {
            Ok(r) => r,
            Err(Error::Protocol { message, .. }) => {
                return Err(Error::Gone(format!(
                    "the body of {} isn't available any more (the page may have navigated \
                     away, or the request failed): {message}",
                    response.url
                )));
            }
            Err(e) => return Err(e),
        };
        let body = r["body"].as_str().unwrap_or_default();
        if r["base64Encoded"].as_bool().unwrap_or(false) {
            base64::engine::general_purpose::STANDARD
                .decode(body)
                .map_err(|e| Error::Protocol {
                    method: "Network.getResponseBody".into(),
                    message: e.to_string(),
                })
        } else {
            Ok(body.as_bytes().to_vec())
        }
    }

    /// Fails, before they leave Chrome, the page's requests of the given
    /// resource types ("image", "font", ...) and those whose URLs match
    /// one of the globs. Adds to what the page already blocks.
    pub async fn block(&self, resource_types: &[&str], urls: &[String]) -> Result<()> {
        add_blocks(&self.inner.rules, resource_types, urls)?;
        self.refresh_fetch().await
    }

    /// Sends every request the page makes to the receiver, as a [`Route`]
    /// to decide about, until [`Page::unroute`]. Documents a guard refuses
    /// and requests the page blocks never get there.
    pub async fn route(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<Route>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        lock(&self.inner.rules).routes = Some(tx);
        self.refresh_fetch().await?;
        Ok(rx)
    }

    /// Stops routing; requests waiting for a decision go on unchanged.
    pub async fn unroute(&self) -> Result<()> {
        lock(&self.inner.rules).routes = None;
        self.refresh_fetch().await
    }

    /// Stops handing requests to routes at once, without asking Chrome
    /// anything: for when whatever decided about them has gone (the event
    /// loop it ran on ended, say). Requests Chrome still pauses for routing
    /// go on unchanged as they come, until routing changes again.
    pub fn drop_routes(&self) {
        lock(&self.inner.rules).routes = None;
    }

    /// Has the Fetch domain pause what the page's rules, its context's,
    /// its guard and its proxy's login need, and nothing else; or turns
    /// it off when nothing does. Page script can't see the Fetch domain.
    pub(crate) async fn refresh_fetch(&self) -> Result<()> {
        let mut on = self.inner.fetching.lock().await;
        let login = self.inner.browser.proxy_login.is_some();
        let mut all = login;
        let mut types: Vec<&str> = Vec::new();
        if self.inner.guarded {
            types.push("Document");
        }
        let context = self.inner.shared_context.as_ref().map(|c| &c.rules);
        for rules in std::iter::once(&self.inner.rules).chain(context) {
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
        let patterns: Vec<Value> = if all {
            vec![json!({"urlPattern": "*"})]
        } else {
            types
                .iter()
                .map(|t| json!({"urlPattern": "*", "resourceType": t}))
                .collect()
        };
        if patterns.is_empty() {
            if *on {
                self.call("Fetch.disable", json!({})).await?;
                *on = false;
            }
            return Ok(());
        }
        self.call(
            "Fetch.enable",
            json!({"handleAuthRequests": login, "patterns": patterns}),
        )
        .await?;
        *on = true;
        Ok(())
    }

    /// The browser's default limit, which waits and route handlers get.
    pub fn default_timeout(&self) -> Duration {
        self.inner.browser.timeout
    }

    /// The current URL.
    pub fn url(&self) -> String {
        self.state().url.clone()
    }

    /// The page's own frame, the one that shows its document.
    pub fn main_frame(&self) -> Frame {
        let (id, url) = {
            let state = self.state();
            (state.frame.clone(), state.url.clone())
        };
        Frame::new(
            self.clone(),
            self.inner.session.clone(),
            id,
            None,
            String::new(),
            url,
        )
    }

    /// Runs `js` in the page's main world: an expression, or a function,
    /// which is called. A promise is awaited. The result comes back as
    /// JSON.
    pub async fn evaluate(&self, js: &str) -> Result<Value> {
        self.main_frame().evaluate(js).await
    }

    /// The document as Chrome now has it, doctype included.
    pub async fn content(&self) -> Result<String> {
        self.main_frame().content().await
    }

    pub async fn title(&self) -> Result<String> {
        self.main_frame().title().await
    }

    /// Clicks the element's centre with the mouse, once it's visible,
    /// still, enabled and not covered.
    pub async fn click(&self, selector: &str, timeout: Option<Duration>) -> Result<()> {
        self.main_frame().click(selector, timeout).await
    }

    /// Replaces the element's contents with `text`, as typing would.
    pub async fn fill(&self, selector: &str, text: &str, timeout: Option<Duration>) -> Result<()> {
        self.main_frame().fill(selector, text, timeout).await
    }

    /// Moves the mouse to the element's centre, once it's visible, still
    /// and not covered.
    pub async fn hover(&self, selector: &str, timeout: Option<Duration>) -> Result<()> {
        self.main_frame().hover(selector, timeout).await
    }

    /// See [`Frame::scroll`].
    pub async fn scroll(
        &self,
        selector: Option<&str>,
        x: f64,
        y: Option<f64>,
        times: Option<u32>,
        idle: Duration,
        timeout: Option<Duration>,
    ) -> Result<u32> {
        self.main_frame()
            .scroll(selector, x, y, times, idle, timeout)
            .await
    }

    /// See [`Frame::select_option`].
    pub async fn select_option(
        &self,
        selector: &str,
        pick: &crate::Pick,
        timeout: Option<Duration>,
    ) -> Result<Vec<String>> {
        self.main_frame()
            .select_option(selector, pick, timeout)
            .await
    }

    /// See [`Frame::set_checked`].
    pub async fn set_checked(
        &self,
        selector: &str,
        on: bool,
        timeout: Option<Duration>,
    ) -> Result<()> {
        self.main_frame().set_checked(selector, on, timeout).await
    }

    /// See [`Frame::set_input_files`].
    pub async fn set_input_files(
        &self,
        selector: &str,
        files: &[std::path::PathBuf],
        timeout: Option<Duration>,
    ) -> Result<()> {
        self.main_frame()
            .set_input_files(selector, files, timeout)
            .await
    }

    /// Waits until `selector` is "attached", "visible", "hidden" (or gone)
    /// or "detached".
    pub async fn wait_for(
        &self,
        selector: &str,
        state: &str,
        timeout: Option<Duration>,
    ) -> Result<()> {
        self.main_frame().wait_for(selector, state, timeout).await
    }

    /// Presses and releases a key in whatever has focus: a character, or
    /// a name such as "Enter", "Tab", "Escape", "Backspace" or "ArrowDown".
    pub async fn press(&self, key: &str) -> Result<()> {
        let k = Key::of(key)?;
        let mut down = json!({
            "type": if k.text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": k.key,
            "code": k.code,
            "windowsVirtualKeyCode": k.key_code,
        });
        if let Some(text) = &k.text {
            down["text"] = Value::from(text.as_str());
            down["unmodifiedText"] = Value::from(text.as_str());
        }
        self.call("Input.dispatchKeyEvent", down).await?;
        self.call(
            "Input.dispatchKeyEvent",
            json!({"type": "keyUp", "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.key_code}),
        )
        .await?;
        Ok(())
    }

    /// Moves the mouse to (x, y) in the viewport.
    pub(crate) async fn mouse_move(&self, x: f64, y: f64) -> Result<()> {
        self.call(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y}),
        )
        .await?;
        Ok(())
    }

    /// Moves the mouse to (x, y) and clicks the left button there.
    pub(crate) async fn mouse_click(&self, x: f64, y: f64) -> Result<()> {
        self.mouse_move(x, y).await?;
        for kind in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "buttons": if kind == "mousePressed" { 1 } else { 0 }, "clickCount": 1}),
            )
            .await?;
        }
        Ok(())
    }

    /// Moves the mouse to (x, y) and turns the wheel by (dx, dy) pixels,
    /// which scrolls whatever is under it. The turn goes as a run of
    /// events no bigger than a mouse wheel's notch, as a person's wheel
    /// sends it, not as one leap no wheel makes.
    pub(crate) async fn mouse_wheel(&self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        self.mouse_move(x, y).await?;
        let notches = (dx.abs().max(dy.abs()) / NOTCH).ceil().max(1.0);
        for _ in 0..notches as u64 {
            self.call(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx / notches, "deltaY": dy / notches}),
            )
            .await?;
        }
        Ok(())
    }

    /// Every frame of the page, its own first: those in its process, then
    /// those Chrome runs in processes of their own, with the frames inside
    /// them.
    pub async fn frames(&self) -> Result<Vec<Frame>> {
        let mut sessions = vec![self.inner.session.clone()];
        sessions.extend(self.state().children.iter().map(|(s, _)| s.clone()));
        let mut frames: Vec<Frame> = Vec::new();
        for session in sessions {
            let tree = match self.call_on(&session, "Page.getFrameTree", json!({})).await {
                Ok(tree) => tree,
                // A frame that went away while it was asked.
                Err(Error::Protocol { .. }) if session != self.inner.session => continue,
                Err(e) => return Err(e),
            };
            let mut walk = vec![&tree["frameTree"]];
            while let Some(node) = walk.pop() {
                let f = &node["frame"];
                let id = str_of(f, "id");
                // A frame's own process knows it best.
                frames.retain(|known| known.id() != id);
                frames.push(Frame::new(
                    self.clone(),
                    session.clone(),
                    id,
                    f["parentId"].as_str().map(str::to_string),
                    str_of(f, "name"),
                    str_of(f, "url"),
                ));
                if let Some(children) = node["childFrames"].as_array() {
                    walk.extend(children.iter().rev());
                }
            }
        }
        Ok(frames)
    }

    /// A PNG of the viewport, or of the whole page.
    pub async fn screenshot(&self, full_page: bool) -> Result<Vec<u8>> {
        let mut params = json!({"format": "png"});
        if full_page {
            let metrics = self.call("Page.getLayoutMetrics", json!({})).await?;
            let size = &metrics["cssContentSize"];
            params["captureBeyondViewport"] = Value::Bool(true);
            params["clip"] = json!({
                "x": 0, "y": 0, "scale": 1,
                "width": size["width"].as_f64().unwrap_or(0.0).ceil(),
                "height": size["height"].as_f64().unwrap_or(0.0).ceil(),
            });
        }
        let r = self.call("Page.captureScreenshot", params).await?;
        base64::engine::general_purpose::STANDARD
            .decode(r["data"].as_str().unwrap_or_default())
            .map_err(|e| Error::Protocol {
                method: "Page.captureScreenshot".into(),
                message: e.to_string(),
            })
    }

    /// The cookies of the page's context.
    pub async fn cookies(&self) -> Result<Vec<Cookie>> {
        context_cookies(&self.inner.browser, self.inner.context.as_deref()).await
    }

    pub async fn set_cookies(&self, cookies: &[Cookie]) -> Result<()> {
        set_context_cookies(&self.inner.browser, self.inner.context.as_deref(), cookies).await
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Relaxed) || self.state().gone
    }

    /// Closes the page, and its context if the page has one of its own.
    pub async fn close(&self) -> Result<()> {
        if self.inner.closed.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        // Whoever takes the page's routes hears there are no more.
        lock(&self.inner.rules).routes = None;
        let conn = &self.inner.browser.conn;
        let children = std::mem::take(&mut self.state().children);
        let closed = self
            .inner
            .browser
            .call(
                "",
                "Target.closeTarget",
                json!({"targetId": self.inner.target}),
            )
            .await;
        conn.off(&self.inner.session);
        for (session, _) in children {
            conn.off(&session);
        }
        if self.inner.owns_context
            && let Some(id) = &self.inner.context
        {
            let _ = self
                .inner
                .browser
                .call(
                    "",
                    "Target.disposeBrowserContext",
                    json!({"browserContextId": id}),
                )
                .await;
        }
        match closed {
            Ok(_) | Err(Error::Closed) | Err(Error::Protocol { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Adds resource types and URL globs to what `rules` block.
pub(crate) fn add_blocks(rules: &SharedRules, types: &[&str], urls: &[String]) -> Result<()> {
    let types = types
        .iter()
        .map(|t| chrome_type(t))
        .collect::<Result<Vec<_>>>()?;
    let mut rules = lock(rules);
    for t in types {
        if !rules.types.contains(&t) {
            rules.types.push(t);
        }
    }
    rules.urls.extend(urls.iter().cloned());
    Ok(())
}

/// What the Fetch domain asks of a page. Authentication challenges are
/// answered: a proxy's with the proxy's login, a site's by leaving it to
/// Chrome. A wrong login isn't given again: Chrome stops asking after
/// it's refused. Paused requests go to the guard, then to the rules.
struct Intercept {
    conn: crate::conn::Connection,
    session: String,
    credentials: Option<(String, String)>,
    /// Where the main frame's documents go to be guarded.
    guarded: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    /// The main frame, whose id is the target's.
    frame: String,
    decider: Arc<Decider>,
}

impl Intercept {
    fn answer(&self, params: &Value) {
        let response = match &self.credentials {
            Some((username, password)) if params["authChallenge"]["source"] == "Proxy" => {
                json!({"response": "ProvideCredentials", "username": username, "password": password})
            }
            _ => json!({"response": "Default"}),
        };
        self.conn.send(
            &self.session,
            "Fetch.continueWithAuth",
            json!({"requestId": str_of(params, "requestId"), "authChallengeResponse": response}),
        );
    }
}

impl Intercept {
    /// Puts a request Fetch paused to the guard first, if it's a document
    /// for the main frame and there's a guard; then to the rules, which
    /// let it go on unchanged unless they block or route it.
    fn release(&self, params: &Value) {
        if let Some(guarded) = &self.guarded
            && params["resourceType"] == "Document"
            && params["frameId"] == self.frame.as_str()
        {
            if guarded.send(params.clone()).is_err() {
                // The guard has gone (it panicked): nothing it would have
                // been asked about goes through unasked.
                self.conn.send(
                    &self.session,
                    "Fetch.failRequest",
                    json!({"requestId": str_of(params, "requestId"), "errorReason": "BlockedByClient"}),
                );
            }
            return;
        }
        self.decider.decide(params);
    }
}

/// What `Target.setAutoAttach` asks for: the sessions of frames Chrome
/// runs in processes of their own, and nothing else (not workers). Nothing
/// is held for it: a frame runs at once.
fn auto_attach() -> Value {
    json!({
        "autoAttach": true,
        "waitForDebuggerOnStart": false,
        "flatten": true,
        "filter": [{"type": "iframe"}],
    })
}

/// Keeps track of the sessions of frames from other sites as they come
/// and go, in a page's session or a frame's (for the frames inside it).
/// Runs on the reader thread, so it sends without waiting.
fn children(conn: &Connection, shared: &Arc<Shared>, method: &str, params: &Value) {
    let session = str_of(params, "sessionId");
    if method == "Target.detachedFromTarget" {
        lock_state(shared).children.retain(|(s, _)| *s != session);
        return;
    }
    if params["targetInfo"]["type"] != "iframe" {
        return;
    }
    let (c, s) = (conn.clone(), shared.clone());
    let handler: Handler = Arc::new(move |method, params| {
        if matches!(
            method,
            "Target.attachedToTarget" | "Target.detachedFromTarget"
        ) {
            children(&c, &s, method, params);
        }
    });
    conn.on(&session, handler);
    conn.send(&session, "Target.setAutoAttach", auto_attach());
    let frame = str_of(&params["targetInfo"], "targetId");
    let mut state = lock_state(shared);
    if !state.gone {
        state.children.push((session, frame));
    }
}

fn lock_state(shared: &Shared) -> std::sync::MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

struct Key {
    key: String,
    code: String,
    key_code: u32,
    text: Option<String>,
}

impl Key {
    fn of(name: &str) -> Result<Key> {
        let named = |code: &str, key_code: u32, text: Option<&str>| Key {
            key: name.to_string(),
            code: code.to_string(),
            key_code,
            text: text.map(str::to_string),
        };
        Ok(match name {
            "Enter" => named("Enter", 13, Some("\r")),
            "Tab" => named("Tab", 9, None),
            "Escape" => named("Escape", 27, None),
            "Backspace" => named("Backspace", 8, None),
            "Delete" => named("Delete", 46, None),
            "ArrowLeft" => named("ArrowLeft", 37, None),
            "ArrowUp" => named("ArrowUp", 38, None),
            "ArrowRight" => named("ArrowRight", 39, None),
            "ArrowDown" => named("ArrowDown", 40, None),
            "Home" => named("Home", 36, None),
            "End" => named("End", 35, None),
            "PageUp" => named("PageUp", 33, None),
            "PageDown" => named("PageDown", 34, None),
            " " | "Space" => Key {
                key: " ".into(),
                code: "Space".into(),
                key_code: 32,
                text: Some(" ".into()),
            },
            _ => {
                let mut chars = name.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return Err(Error::Invalid(format!("unknown key {name:?}")));
                };
                let (code, key_code) = if c.is_ascii_alphabetic() {
                    (
                        format!("Key{}", c.to_ascii_uppercase()),
                        c.to_ascii_uppercase() as u32,
                    )
                } else if c.is_ascii_digit() {
                    (format!("Digit{c}"), c as u32)
                } else {
                    (String::new(), 0)
                };
                Key {
                    key: c.to_string(),
                    code,
                    key_code,
                    text: Some(c.to_string()),
                }
            }
        })
    }
}
