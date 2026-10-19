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
use crate::events::{Dialog, Downloads, PageEvent};
use crate::frame::Frame;
use crate::network::{
    Decider, Request, Route, SharedRules, chrome_type, headers_of, lock, went_away,
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
    /// The session that saw it, whose own the request id is: the page's,
    /// or that of a frame from another site.
    pub(crate) session: String,
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
    /// What decides about its requests, and what Fetch pauses for it.
    decider: Arc<Decider>,
    /// What the last scroll to the bottom did, turn by turn.
    pub(crate) scroll_trace: Mutex<Vec<String>>,
    /// The shared context it's in, whose routes it has.
    shared_context: Option<Arc<ContextRules>>,
    /// The pages it opened, which have its rules too.
    popups: Mutex<Vec<Weak<PageInner>>>,
    /// The target of the tab in front, of those in its window: a popup
    /// opens as a tab in front of its opener, which Chrome then counts
    /// as hidden, and shares this with it.
    front: Arc<Mutex<String>>,
}

/// How a page comes to be: in which context, guarded by what, and opened
/// by which page, if it's a popup.
struct Opening {
    context: Option<String>,
    /// Whether closing the page disposes of its context.
    owns_context: bool,
    guard: Option<crate::browser::Guard>,
    shared_context: Option<Arc<ContextRules>>,
    opener: Option<Page>,
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
    /// Requests still loading, with their methods, by session and request
    /// id: each session numbers its requests its own way.
    loading: HashMap<(String, String), String>,
    /// Where requests and responses go, while anyone listens.
    listeners: Vec<tokio::sync::mpsc::UnboundedSender<PageEvent>>,
    /// Where dialogs go, while anyone answers them.
    dialogs: Option<tokio::sync::mpsc::UnboundedSender<Dialog>>,
    /// Frames Chrome runs in processes of their own (from other sites),
    /// each with a session: (session, frame id), in the order they came.
    pub(crate) children: Vec<(String, String)>,
    /// Every session the page has besides its own, those frames', which
    /// get the same Network and Fetch setup as its own. Unlike
    /// `children`, a frame stays here until its session detaches.
    sessions: Vec<String>,
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
    /// An event from `session`: the page's own, or one of its frames'
    /// from other sites, which tell only of requests.
    fn on(&mut self, session: &str, method: &str, p: &Value) {
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
                self.loading
                    .insert((session.to_string(), id), request.method.clone());
                self.tell(PageEvent::Request(request));
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                self.loading
                    .remove(&(session.to_string(), str_of(p, "requestId")));
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
                        .get(&(session.to_string(), id.clone()))
                        .cloned()
                        .unwrap_or_else(|| "GET".into()),
                    request_id: id,
                    session: session.to_string(),
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
                self.tell(PageEvent::Response(response));
            }
            "Inspector.targetCrashed" | GONE => {
                self.gone = true;
                // Listeners hear there's nothing more: their receivers end.
                self.listeners.clear();
                self.dialogs = None;
            }
            _ => {}
        }
    }

    /// Passes `event` to whoever is listening; nothing is kept for later.
    fn tell(&mut self, event: PageEvent) {
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
        let mut params = json!({"url": "about:blank"});
        if let Some(id) = &context {
            params["browserContextId"] = Value::from(id.as_str());
        }
        let target = str_of(
            &browser.call("", "Target.createTarget", params).await?,
            "targetId",
        );
        // Kept attached when the browser's session hears it.
        browser
            .attaching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(target.clone());
        let attached = browser
            .call(
                "",
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await;
        if attached.is_err() {
            // No attaching for the browser's session to hear.
            browser
                .attaching
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&target);
        }
        let session = str_of(&attached?, "sessionId");
        let opening = Opening {
            context,
            owns_context,
            guard,
            shared_context,
            opener: None,
        };
        Page::start(browser, target, session, opening).await
    }

    /// Takes on a popup `opener` opened, which Chrome holds on `session`
    /// until it's set up as any page is, then lets it run and tells the
    /// opener's listeners. One whose setup fails is closed, with a
    /// warning, rather than left to fetch without its rules.
    pub(crate) async fn adopt(opener: Page, target: String, session: String) {
        let o = &opener.inner;
        let opening = Opening {
            context: o.context.clone(),
            owns_context: false,
            guard: o.decider.guard.clone(),
            shared_context: o.shared_context.clone(),
            opener: Some(opener.clone()),
        };
        match Page::start(o.browser.clone(), target, session.clone(), opening).await {
            Ok(popup) => {
                o.popups
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Arc::downgrade(&popup.inner));
                opener.state().tell(PageEvent::Popup(popup));
            }
            Err(e) if went_away(&e) => {}
            Err(e) => crate::warn(&format!(
                "a popup of the page at {} was closed: it couldn't be set up to fetch as its \
                 opener does: {e}",
                opener.url()
            )),
        }
    }

    async fn start(
        browser: Arc<Inner>,
        target: String,
        session: String,
        opening: Opening,
    ) -> Result<Page> {
        let Opening {
            context,
            owns_context,
            guard,
            shared_context,
            opener,
        } = opening;
        let conn = &browser.conn;
        let shared = Arc::new(Shared {
            // A page's main frame has its target's id; known from the
            // start, a popup's first response, which can come before the
            // frame tree does, is its document's.
            state: Mutex::new(State {
                frame: target.clone(),
                ..State::default()
            }),
            changed: Notify::new(),
        });
        let rules = SharedRules::default();
        let mut all_rules = vec![rules.clone()];
        // A popup has its own rules, then its opener's (the context's
        // among them).
        match &opener {
            Some(opener) => all_rules.extend(opener.inner.decider.rules.iter().cloned()),
            None => all_rules.extend(shared_context.as_ref().map(|c| c.rules.clone())),
        }
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
            login: browser.proxy_login.is_some(),
            fetching: tokio::sync::Mutex::new(false),
        });
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
                        decider.decide(&session, &params);
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
        let identity = browser.identity.as_ref().map(|identity| {
            json!({"userAgent": identity.user_agent, "userAgentMetadata": identity.metadata})
        });
        let watch = Arc::new(Watch {
            conn: conn.clone(),
            shared: shared.clone(),
            downloads: browser.downloads.clone(),
            context: context.clone(),
            opener: opener.as_ref().map(|o| o.inner.shared.clone()),
            pause: browser.pause,
            intercept: Intercept {
                conn: conn.clone(),
                session: session.clone(),
                credentials: browser.proxy_login.clone(),
                guarded,
                frame: target.clone(),
                decider: decider.clone(),
            },
            identity: identity.clone(),
        });
        let own = session.clone();
        let handler: Handler = Arc::new(move |method, params| watch.hear(&own, method, params));
        conn.on(&session, handler);
        let front = match &opener {
            Some(opener) => {
                let front = opener.inner.front.clone();
                *front.lock().unwrap_or_else(|e| e.into_inner()) = target.clone();
                front
            }
            None => Arc::new(Mutex::new(target.clone())),
        };
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
                decider,
                scroll_trace: Mutex::default(),
                shared_context: shared_context.clone(),
                popups: Mutex::default(),
                front,
            }),
        };
        {
            let mut pages = browser.pages.lock().unwrap_or_else(|e| e.into_inner());
            pages.retain(|p| p.strong_count() > 0);
            pages.push(Arc::downgrade(&page.inner));
        }
        if let Some(c) = &shared_context {
            c.pages
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Arc::downgrade(&page.inner));
        }
        type Step<'a> =
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
        let setup = {
            let me = &page;
            let call = |method: &'static str, params: Value| -> Step<'_> {
                Box::pin(async move { me.call(method, params).await.map(drop) })
            };
            let mut steps = vec![
                call("Page.enable", json!({})),
                call("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
                call("Network.enable", json!({})),
                Box::pin(page.refresh_fetch()),
                // Frames from other sites run in processes of their own, and
                // are reached through sessions of their own.
                call("Target.setAutoAttach", auto_attach()),
            ];
            if let Some(identity) = identity {
                steps.push(call("Network.setUserAgentOverride", identity));
            }
            steps.push(Box::pin(async {
                let tree = page.call("Page.getFrameTree", json!({})).await?;
                let frame = &tree["frameTree"]["frame"];
                let mut state = page.state();
                // Unless a document has committed meanwhile.
                if state.loader.is_empty() {
                    state.frame = str_of(frame, "id");
                    state.url = str_of(frame, "url");
                    state.loader = str_of(frame, "loaderId");
                    let loader = state.loader.clone();
                    state.committed.push(loader);
                }
                Ok(())
            }));
            // Each step sends its command the first time it's polled, in
            // order, so all of them have gone out after one poll. A popup,
            // which Chrome holds, is let go after them: Chrome takes a
            // session's commands in the order they come, so it runs set up,
            // and some answers (Page.enable's) only come once it runs.
            let mut setup = Box::pin(futures_util::future::try_join_all(steps));
            let first = futures_util::future::poll_immediate(&mut setup).await;
            if opener.is_some() {
                conn.send(
                    &page.inner.session,
                    "Runtime.runIfWaitingForDebugger",
                    json!({}),
                );
            }
            match first {
                Some(done) => done,
                None => setup.await,
            }
        };
        if let Err(e) = setup {
            let _ = page.close().await;
            return Err(e);
        }
        Ok(page)
    }

    /// Whether the target `id` is this page, or one of its frames from
    /// other sites: what a popup's opener can be.
    pub(crate) fn opened(&self, id: &str) -> bool {
        self.inner.target == id || self.state().children.iter().any(|(_, frame)| frame == id)
    }

    /// Brings the page's tab in front of a popup it opened (or of its
    /// opener, for a popup), as a person switches to the tab they're
    /// about to use: in a tab behind, Chrome runs no animation frames and
    /// routes the mouse elsewhere. Nothing is sent for a page in front,
    /// nor for one Chrome has shown again by itself.
    pub(crate) async fn to_front(&self) -> Result<()> {
        let target = &self.inner.target;
        if *self.inner.front.lock().unwrap_or_else(|e| e.into_inner()) == *target {
            return Ok(());
        }
        // Chrome shows a tab again by itself when the one in front of it
        // closes, whoever closed it, so the page is asked first, in
        // netweir's own world, where page script can't see the asking.
        let shown = self.main_frame().helper("document.visibilityState").await;
        if !matches!(shown, Ok(v) if v == "visible") {
            self.call("Page.bringToFront", json!({})).await?;
            // A person looks at the tab before using it.
            tokio::time::sleep(crate::events::pause(self.inner.browser.pause)).await;
        }
        *self.inner.front.lock().unwrap_or_else(|e| e.into_inner()) = target.clone();
        Ok(())
    }

    /// Tells the page's listeners of a download one of its frames
    /// started that only the browser's session heard of.
    pub(crate) fn download_began(&self, params: &Value) {
        let inner = &self.inner;
        let browser = &inner.browser;
        let begun = browser.downloads.begin(
            &browser.conn,
            inner.decider.timeout,
            inner.context.as_deref(),
            params,
        );
        if let Some(download) = begun {
            self.state().tell(PageEvent::Download(download));
        }
    }

    /// The runtime the page's own tasks run on.
    pub(crate) fn runtime(&self) -> tokio::runtime::Handle {
        self.inner.decider.runtime.clone()
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
        let mut retries = 0;
        let r = loop {
            // Held to this navigation's own deadline, which may be longer
            // than the browser's timeout.
            let navigate = self.call_unbounded("Page.navigate", json!({"url": url}));
            let r = tokio::time::timeout_at(deadline, navigate)
                .await
                .map_err(|_| Error::Timeout(format!("navigating to {url}")))??;
            let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) else {
                break r;
            };
            // A connection that never started, so the far end saw nothing:
            // asked again, as a person would reload, it gets another port.
            if crate::retry_navigation(e) && retries < 2 {
                retries += 1;
                tokio::time::sleep_until(
                    (Instant::now() + Duration::from_millis(50)).min(deadline),
                )
                .await;
                continue;
            }
            return Err(Error::Navigation {
                url: url.to_string(),
                error: e.to_string(),
                // Chrome doesn't say why it gave up on a navigation; its
                // stderr sometimes does.
                said: self.inner.browser.said.as_ref().and_then(|s| s.recent()),
            });
        };
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
        let deadline = Instant::now() + self.timeout(timeout);
        // A page's script that navigates may wait on a frame, or check
        // that it's seen, first.
        self.to_front().await?;
        self.until(
            || format!("{event} of a navigation away from {}", after.url),
            deadline.saturating_duration_since(Instant::now()),
            |s| s.reached_after(&after.loader, event),
        )
        .await?;
        Ok(self.response_for(None))
    }

    /// Waits until the document showing now, or one that replaced it,
    /// reaches `wait`, and returns the response of the document showing
    /// then. For a popup just opened, that's the document it was opened
    /// for: the empty one Chrome shows first never loads.
    pub async fn wait_for_load(
        &self,
        wait: WaitUntil,
        timeout: Option<Duration>,
    ) -> Result<Response> {
        let event = wait.event();
        let deadline = Instant::now() + self.timeout(timeout);
        let (loader, url) = {
            let state = self.state();
            (state.loader.clone(), state.url.clone())
        };
        // As wait_for_navigation.
        self.to_front().await?;
        self.until(
            || format!("{event} of the page at {url}"),
            deadline.saturating_duration_since(Instant::now()),
            // A page whose first document hasn't committed yet has no
            // loader to wait past: any document's will do.
            |s| {
                if loader.is_empty() {
                    s.committed.first().is_some_and(|l| s.reached(l, event))
                } else {
                    s.reached(&loader, event)
                }
            },
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
                session: String::new(),
                method: "GET".into(),
                resource_type: "document".into(),
            })
    }

    /// Hears about every request the page makes and every response it
    /// gets, from now until the receiver is dropped or the page closes.
    /// Nothing is kept for a page nobody listens to.
    pub fn listen(&self) -> tokio::sync::mpsc::UnboundedReceiver<PageEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = self.state();
        // A page that's gone has nothing more to say: the receiver ends.
        if !state.gone && !self.inner.closed.load(Ordering::Relaxed) {
            state.listeners.push(tx);
        }
        rx
    }

    /// Hands every dialog the page opens to the receiver, from now until
    /// it's dropped, the page closes, or this is called again (which
    /// ends the last receiver). A dialog the receiver's holder doesn't
    /// answer is answered the default way once dropped, or after the
    /// browser's timeout; with no receiver, at once. Answers wait until
    /// a person could have given them.
    pub fn dialogs(&self) -> tokio::sync::mpsc::UnboundedReceiver<Dialog> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = self.state();
        if !state.gone && !self.inner.closed.load(Ordering::Relaxed) {
            state.dialogs = Some(tx);
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
        let key = (response.session.clone(), id.clone());
        self.until(
            || format!("the body of {}", response.url),
            self.timeout(None),
            |s| !s.loading.contains_key(&key),
        )
        .await?;
        // Asked of the session that saw it, which is the only one that has
        // it: a frame's from another site, once it has gone, has taken its
        // bodies with it.
        let r = match self
            .call_on(
                &response.session,
                "Network.getResponseBody",
                json!({"requestId": id}),
            )
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
    /// its guard and its proxy's login need, and nothing else, in every
    /// session the page has; or turns it off when nothing does. Page
    /// script can't see the Fetch domain. Its popups, which have its
    /// rules too, are brought up to date after it.
    pub(crate) async fn refresh_fetch(&self) -> Result<()> {
        self.refresh_own_fetch().await?;
        let popups: Vec<Page> = {
            let mut popups = self.inner.popups.lock().unwrap_or_else(|e| e.into_inner());
            popups.retain(|p| p.strong_count() > 0);
            popups
                .iter()
                .filter_map(Weak::upgrade)
                .map(|inner| Page { inner })
                .collect()
        };
        for popup in popups {
            match Box::pin(popup.refresh_fetch()).await {
                Err(e) if !went_away(&e) => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    async fn refresh_own_fetch(&self) -> Result<()> {
        let decider = &self.inner.decider;
        let mut on = decider.fetching.lock().await;
        let patterns = decider.patterns();
        let (method, params) = if !patterns.is_empty() {
            ("Fetch.enable", decider.enable(patterns))
        } else if *on {
            ("Fetch.disable", json!({}))
        } else {
            return Ok(());
        };
        self.call(method, params.clone()).await?;
        *on = method == "Fetch.enable";
        // Its frames' sessions, all at once: one that's busy or on its way
        // out holds up none of the others.
        let sessions = self.state().sessions.clone();
        let calls = sessions
            .iter()
            .map(|session| self.call_on(session, method, params.clone()));
        for (session, r) in sessions
            .iter()
            .zip(futures_util::future::join_all(calls).await)
        {
            match r {
                Err(e) if !went_away(&e) => crate::warn(&format!(
                    "{method} on the session {session} of the page at {} failed, so its requests \
                     may not be blocked or routed as the page's are: {e}",
                    self.url()
                )),
                _ => {}
            }
        }
        Ok(())
    }

    /// What the last scroll to the bottom did, turn by turn: where each
    /// turn went and what it moved, and how the trip ended. For tests to
    /// show when a platform scrolls differently.
    #[doc(hidden)]
    pub fn scroll_trace(&self) -> Vec<String> {
        self.inner
            .scroll_trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
        self.to_front().await?;
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

    /// Clicks the left button at (x, y), where the mouse already is.
    pub(crate) async fn mouse_press(&self, x: f64, y: f64) -> Result<()> {
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
        let sessions = {
            let mut state = self.state();
            state.children.clear();
            std::mem::take(&mut state.sessions)
        };
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
        for session in sessions {
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
            self.inner.browser.downloads.remove(Some(id));
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

/// What the Fetch domain asks of a page, in any of its sessions.
/// Authentication challenges are answered: a proxy's with the proxy's
/// login, a site's by leaving it to Chrome. A wrong login isn't given
/// again: Chrome stops asking after it's refused. Paused requests go to
/// the guard, then to the rules.
struct Intercept {
    conn: crate::conn::Connection,
    /// The page's own session.
    session: String,
    credentials: Option<(String, String)>,
    /// Where the main frame's documents go to be guarded.
    guarded: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    /// The main frame, whose id is the target's.
    frame: String,
    decider: Arc<Decider>,
}

impl Intercept {
    fn answer(&self, session: &str, params: &Value) {
        let response = match &self.credentials {
            Some((username, password)) if params["authChallenge"]["source"] == "Proxy" => {
                json!({"response": "ProvideCredentials", "username": username, "password": password})
            }
            _ => json!({"response": "Default"}),
        };
        self.conn.send(
            session,
            "Fetch.continueWithAuth",
            json!({"requestId": str_of(params, "requestId"), "authChallengeResponse": response}),
        );
    }

    /// Puts a request Fetch paused to the guard first, if it's a document
    /// for the main frame and there's a guard; then to the rules, which
    /// let it go on unchanged unless they block or route it. Only the
    /// page's own session fetches the main frame's documents: one a frame
    /// from another site sends the page to (`top.location`) is the main
    /// frame's navigation, and Chrome pauses it there.
    fn release(&self, session: &str, params: &Value) {
        if let Some(guarded) = &self.guarded
            && session == self.session
            && params["resourceType"] == "Document"
            && params["frameId"] == self.frame.as_str()
        {
            if guarded.send(params.clone()).is_err() {
                // The guard has gone (it panicked): nothing it would have
                // been asked about goes through unasked.
                self.conn.send(
                    session,
                    "Fetch.failRequest",
                    json!({"requestId": str_of(params, "requestId"), "errorReason": "BlockedByClient"}),
                );
            }
            return;
        }
        self.decider.decide(session, params);
    }
}

/// What `Target.setAutoAttach` asks for: a session for every target the
/// page or frame starts, each held before it runs anything until it's
/// let go (see [`Watch::adopt`]). Frames from other sites and dedicated
/// workers are set up first, so none of their requests can go out unheard
/// or unrouted; anything else (worklets, and whatever kinds Chrome adds
/// later) is let go and left at once. Every kind is attached because
/// Chrome holds kinds the filter leaves out too (dedicated workers and
/// worklets, with a filter of iframes alone), and a kind held but never
/// attached would never run. Holding is what DevTools
/// does when it's open: page script has nothing to see it by, beyond a
/// frame or worker starting a round trip or two later.
fn auto_attach() -> Value {
    json!({
        "autoAttach": true,
        "waitForDebuggerOnStart": true,
        "flatten": true,
        // Everything but the browser and tabs, which a page never starts.
        "filter": [{"type": "browser", "exclude": true}, {"type": "tab", "exclude": true}, {}],
    })
}

/// What the handlers of a page's sessions share: its own, and those of
/// its frames from other sites and its workers, each told which session
/// an event came from. They run on the reader thread, so they send
/// without waiting.
struct Watch {
    conn: Connection,
    shared: Arc<Shared>,
    intercept: Intercept,
    /// The user agent override the page's own session has, for the
    /// others too.
    identity: Option<Value>,
    /// Downloads under way, and their folders.
    downloads: Arc<Downloads>,
    /// The page's context; None for the default one.
    context: Option<String>,
    /// For a popup, its opener's.
    opener: Option<Arc<Shared>>,
    /// See [`crate::LaunchOptions::dialog_pause`].
    pause: (Duration, Duration),
}

impl Watch {
    fn hear(self: &Arc<Self>, session: &str, method: &str, params: &Value) {
        match method {
            "Fetch.authRequired" => return self.intercept.answer(session, params),
            "Fetch.requestPaused" => return self.intercept.release(session, params),
            "Target.attachedToTarget" | "Target.detachedFromTarget" => {
                return self.adopt(session, method, params);
            }
            // Chrome tells the page's own session of a frame's dialog
            // too, and only that session has the Page domain.
            "Page.javascriptDialogOpening" => {
                let decider = &self.intercept.decider;
                let dialog = Dialog::opened(
                    &self.conn,
                    session,
                    params,
                    decider.timeout,
                    self.pause,
                    &decider.runtime,
                );
                // Nobody listening drops it, which answers it.
                let mut state = lock_state(&self.shared);
                if let Some(to) = &state.dialogs
                    && to.send(dialog).is_err()
                {
                    state.dialogs = None;
                }
                return;
            }
            "Page.downloadWillBegin" => return self.download(params),
            _ => {}
        }
        // From another session, only what it fetches is the page's
        // business; its going away, say, isn't the page's.
        if session != self.intercept.session && !method.starts_with("Network.") {
            return;
        }
        lock_state(&self.shared).on(session, method, params);
        self.shared.changed.notify_waiters();
    }

    /// Tells the page's listeners of a download it started; a popup that
    /// never showed a document (a link to a file with a target opens one,
    /// which Chrome closes again) tells its opener's too.
    fn download(&self, params: &Value) {
        let decider = &self.intercept.decider;
        let Some(download) =
            self.downloads
                .begin(&self.conn, decider.timeout, self.context.as_deref(), params)
        else {
            return;
        };
        let blank = {
            let mut state = lock_state(&self.shared);
            state.tell(PageEvent::Download(download.clone()));
            state.url.is_empty() || state.url == "about:blank"
        };
        if blank && let Some(opener) = &self.opener {
            lock_state(opener).tell(PageEvent::Download(download));
        }
    }

    /// Keeps track of the sessions of frames from other sites and of
    /// workers as they come and go, in a page's session or a frame's or
    /// worker's (for the frames and workers inside it), the `parent`.
    fn adopt(self: &Arc<Self>, parent: &str, method: &str, params: &Value) {
        let session = str_of(params, "sessionId");
        if method == "Target.detachedFromTarget" {
            let mut state = lock_state(&self.shared);
            state.children.retain(|(s, _)| *s != session);
            state.sessions.retain(|s| *s != session);
            // Its requests will never finish now: a body waiting on one
            // asks, and hears it's gone.
            state.loading.retain(|(s, _), _| *s != session);
            drop(state);
            self.shared.changed.notify_waiters();
            return;
        }
        let info = &params["targetInfo"];
        // A worker has no Fetch domain: the frame that started it pauses
        // its requests, so its own session only reports them. Any other
        // kind isn't watched: it's let go, and its session left.
        let frame = match info["type"].as_str() {
            Some("iframe") => true,
            Some("worker") => false,
            _ => {
                // Left only once it has been let go: a service worker
                // left first stays held. Left at all, because Chrome keeps
                // a service worker alive while anything is attached to it.
                let (conn, parent) = (self.conn.clone(), parent.to_string());
                self.intercept.decider.runtime.spawn(async move {
                    let _ = conn
                        .call(&session, "Runtime.runIfWaitingForDebugger", json!({}))
                        .await;
                    let _ = conn
                        .call(
                            &parent,
                            "Target.detachFromTarget",
                            json!({"sessionId": session}),
                        )
                        .await;
                });
                return;
            }
        };
        let (watch, own) = (self.clone(), session.clone());
        let handler: Handler = Arc::new(move |method, params| watch.hear(&own, method, params));
        self.conn.on(&session, handler);
        if frame {
            let mut state = lock_state(&self.shared);
            if !state.gone {
                state.sessions.push(session.clone());
                state
                    .children
                    .push((session.clone(), str_of(info, "targetId")));
            }
        }
        let (watch, url) = (self.clone(), str_of(info, "url"));
        self.intercept
            .decider
            .runtime
            .spawn(async move { watch.prepare(session, frame, url).await });
    }

    /// Sets up a frame's or worker's session as the page's own is, while
    /// Chrome holds it before it runs anything, then lets it run. Known
    /// limit: it runs however setup ends. One that hasn't finished within
    /// the browser's timeout runs without it, with a warning, and its
    /// requests may go out unheard and unrouted, rather than leave the
    /// frame, and the page's load, waiting on it for ever.
    async fn prepare(&self, session: String, frame: bool, url: String) {
        let decider = &self.intercept.decider;
        let call = |method: &'static str, params: Value| self.conn.call(&session, method, params);
        let setup = async {
            call("Network.enable", json!({})).await?;
            if let Some(identity) = &self.identity {
                call("Network.setUserAgentOverride", identity.clone()).await?;
            }
            if frame {
                // Worked out and sent while the lock is held, so that what
                // reaches this session last, this or `refresh_fetch`'s
                // (which holds it too), has the newest rules: Chrome takes
                // a session's commands in the order they're sent. The
                // answer is waited for after, so frames being set up don't
                // queue behind each other's round trips.
                let enabling = {
                    let mut on = decider.fetching.lock().await;
                    let patterns = decider.patterns();
                    if patterns.is_empty() {
                        None
                    } else {
                        *on = true;
                        let mut enable = Box::pin(call("Fetch.enable", decider.enable(patterns)));
                        match futures_util::future::poll_immediate(&mut enable).await {
                            Some(answer) => {
                                answer?;
                                None
                            }
                            None => Some(enable),
                        }
                    }
                };
                if let Some(enable) = enabling {
                    enable.await?;
                }
            }
            // The frames and workers inside it, the same way.
            call("Target.setAutoAttach", auto_attach()).await?;
            Ok::<_, Error>(())
        };
        let failed = match tokio::time::timeout(decider.timeout, setup).await {
            Ok(Ok(())) => None,
            // It went away while it was set up: there's nothing to run.
            Ok(Err(e)) if went_away(&e) => return,
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some(format!(
                "Chrome didn't finish setting it up within {}s",
                decider.timeout.as_secs_f64()
            )),
        };
        if let Some(why) = failed {
            let kind = if frame { "frame" } else { "worker" };
            // Chrome may attach a frame before it knows its URL.
            let what = if url.is_empty() {
                format!("a {kind}")
            } else {
                format!("the {kind} at {url}")
            };
            crate::warn(&format!(
                "{what} runs without all its requests heard, blocked or routed: {why}"
            ));
        }
        self.conn
            .send(&session, "Runtime.runIfWaitingForDebugger", json!({}));
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
