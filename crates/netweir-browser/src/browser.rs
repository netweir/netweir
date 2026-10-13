//! A running Chrome and its browser contexts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::conn::Connection;
use crate::network::Route;
use crate::page::{ContextRules, Cookie, Page, WaitUntil, cookies_from, cookies_to};
use crate::{Error, Result, launch};

/// Decides whether a page may fetch a document, given its URL.
pub type Guard = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// For a Chrome major version (such as "154"), the `Sec-CH-UA` value its
/// pages should present, if not Chrome's own.
pub type Brands = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct LaunchOptions {
    /// Chrome's executable; found with [`crate::find_chrome`] when None.
    pub executable: Option<PathBuf>,
    pub headless: bool,
    /// More command-line switches for Chrome.
    pub args: Vec<String>,
    /// The default limit for navigations and actions.
    pub timeout: Duration,
    /// An `http://`, `https://` or `socks5://` proxy for everything Chrome
    /// fetches. For an HTTP(S) proxy, a username and password in it are
    /// given when it asks; Chrome can't log in to a SOCKS proxy, so one
    /// with a login is refused.
    pub proxy: Option<String>,
    /// The brands pages present. Chrome for Testing calls itself Chromium
    /// alone, where Google Chrome of the same version lists itself too.
    pub brands: Option<Brands>,
    /// A profile directory to use and keep, instead of a fresh one that's
    /// removed on close: cookies and storage survive to the next launch.
    /// Pages from [`Browser::new_page`] then share it.
    pub user_data_dir: Option<PathBuf>,
}

impl std::fmt::Debug for LaunchOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchOptions")
            .field("executable", &self.executable)
            .field("headless", &self.headless)
            .field("args", &self.args)
            .field("timeout", &self.timeout)
            .field("proxy", &self.proxy.as_ref().map(|_| "…"))
            .field("brands", &self.brands.as_ref().map(|_| "…"))
            .field("user_data_dir", &self.user_data_dir)
            .finish()
    }
}

impl Default for LaunchOptions {
    fn default() -> LaunchOptions {
        LaunchOptions {
            executable: None,
            headless: true,
            args: Vec::new(),
            timeout: Duration::from_secs(30),
            proxy: None,
            brands: None,
            user_data_dir: None,
        }
    }
}

/// What every page says it is: the user agent without the headless
/// marker, and the client hints Chrome reports for itself.
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    pub user_agent: String,
    pub metadata: Value,
}

/// A running Chrome. Cheap to clone; closes when [`Browser::close`] is
/// called or the last clone is dropped.
#[derive(Clone)]
pub struct Browser {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub conn: Connection,
    /// The Chrome netweir launched; None for one it connected to.
    child: Mutex<Option<Child>>,
    /// The launched Chrome's profile, removed when it closes unless it's
    /// one the caller gave.
    profile: Option<PathBuf>,
    /// Whether the profile is the caller's, to keep: then pages open in
    /// its own context, the one whose cookies and storage Chrome saves.
    keep_profile: bool,
    /// Contexts netweir made, for closing a browser it connected to,
    /// whose other contexts aren't netweir's to close.
    contexts: Mutex<Vec<String>>,
    version: String,
    pub identity: Option<Identity>,
    pub timeout: Duration,
    /// The proxy's username and password, for its challenges.
    pub proxy_login: Option<(String, String)>,
    /// What the launched Chrome has lately written to stderr.
    pub said: Option<launch::Said>,
}

impl Browser {
    pub async fn launch(mut options: LaunchOptions) -> Result<Browser> {
        options.args.extend(launch::env_switches());
        let executable = match options.executable.clone() {
            Some(path) => path,
            None => launch::find_chrome()?,
        };
        // An install from before netweir granted this is fixed in place.
        launch::grant_if_ours(&executable)?;
        let keep_profile = options.user_data_dir.is_some();
        let profile = match &options.user_data_dir {
            Some(dir) => std::fs::create_dir_all(dir).map(|()| dir.clone()),
            None => profile_dir(),
        }
        .map_err(|e| Error::Launch(format!("can't make a profile directory: {e}")))?;
        let mut args = vec![
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            // Leaves navigator.webdriver false.
            "--disable-blink-features=AutomationControlled".into(),
        ];
        args.extend(KEYRING.iter().chain(FOREGROUND).map(|a| a.to_string()));
        // Chrome's client hints, when they had to be read before launch.
        let mut hints = None;
        if options.headless {
            args.push("--headless".into());
            // Service workers take their user agent from this flag alone;
            // page overrides don't reach them. The flag empties the
            // high-entropy client hints, which each page's override puts
            // back.
            if !options.args.iter().any(|a| a.starts_with("--user-agent=")) {
                let (agent, metadata) = headless_identity(&executable, &options.args).await?;
                args.push(format!(
                    "--user-agent={}",
                    agent.replace("HeadlessChrome/", "Chrome/")
                ));
                hints = Some(metadata);
            }
        }
        let mut proxy_login = None;
        if let Some(proxy) = &options.proxy {
            let (server, login) = split_proxy(proxy)?;
            args.push(format!("--proxy-server={server}"));
            proxy_login = login;
        }
        args.extend(options.args.iter().cloned());
        args.push("about:blank".into());
        let started = match launch::start(&executable, &args) {
            Ok(started) => started,
            Err(e) => {
                if !keep_profile {
                    let _ = std::fs::remove_dir_all(&profile);
                }
                return Err(e);
            }
        };
        let said = started.said.clone();
        let conn = Connection::new(started.replies, started.commands);
        let inner = Inner {
            conn,
            child: Mutex::new(Some(started.child)),
            profile: Some(profile),
            keep_profile,
            contexts: Mutex::default(),
            version: String::new(),
            identity: None,
            timeout: options.timeout,
            proxy_login,
            said: Some(said.clone()),
        };
        Browser::finish(inner, &options, &executable.display().to_string(), hints)
            .await
            .map_err(|e| match e {
                Error::Launch(message) => Error::Launch(said.explain(message)),
                other => other,
            })
    }

    /// Drives a browser that's already running: a `ws://` DevTools URL, as
    /// Chrome prints it, or an `http://host:port` whose `/json/version`
    /// names one. `options` give the timeout and the brands; nothing is
    /// launched. Closing it closes what netweir opened and leaves the
    /// browser running.
    pub async fn connect(url: &str, options: LaunchOptions) -> Result<Browser> {
        let ws = if url.starts_with("http://") {
            devtools_url(url).await?
        } else {
            url.to_string()
        };
        let late = || Error::Launch(format!("{ws} didn't answer in 30 seconds"));
        let refused = |e: String| Error::Launch(format!("can't connect to {ws}: {e}"));
        let conn = if ws.starts_with("wss://") {
            let socket = tokio::time::timeout(Duration::from_secs(30), secure_websocket(&ws))
                .await
                .map_err(|_| late())?
                .map_err(refused)?;
            Connection::over_websocket(socket)
        } else {
            let (socket, _) = tokio::time::timeout(
                Duration::from_secs(30),
                tokio_tungstenite::connect_async(ws.as_str()),
            )
            .await
            .map_err(|_| late())?
            .map_err(|e| refused(e.to_string()))?;
            Connection::over_websocket(socket)
        };
        let inner = Inner {
            conn,
            child: Mutex::new(None),
            profile: None,
            keep_profile: false,
            contexts: Mutex::default(),
            version: String::new(),
            identity: None,
            timeout: options.timeout,
            proxy_login: None,
            said: None,
        };
        Browser::finish(inner, &options, &ws, None).await
    }

    /// Asks the browser what it is, and works out what its pages should
    /// say they are.
    async fn finish(
        mut inner: Inner,
        options: &LaunchOptions,
        what: &str,
        hints: Option<Value>,
    ) -> Result<Browser> {
        let version = match tokio::time::timeout(
            Duration::from_secs(30),
            inner.conn.call("", "Browser.getVersion", json!({})),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(Error::Launch(format!("{what} didn't answer: {e}"))),
            Err(_) => return Err(Error::Launch(format!("{what} didn't answer in 30 seconds"))),
        };
        inner.version = str_of(&version, "product");
        // Anything that answers in JSON could get this far; a browser says
        // what it is.
        if inner.version.is_empty() {
            return Err(Error::Launch(format!(
                "{what} isn't a browser's DevTools: it didn't say what it is"
            )));
        }
        let user_agent = str_of(&version, "userAgent");
        let mut browser = Browser {
            inner: Arc::new(inner),
        };
        let full = browser
            .inner
            .version
            .trim_start_matches("Chrome/")
            .to_string();
        let major = full.split('.').next().unwrap_or_default().to_string();
        // A list that doesn't parse would leave pages with no brands at
        // all; Chrome's own are better.
        let brands = options
            .brands
            .as_ref()
            .and_then(|b| b(&major))
            .filter(|header| !parse_brands(header).is_empty());
        // A headless Chrome netweir launched has a clean user agent from the
        // --user-agent flag and the hints it read before the flag emptied
        // them, which each page's override restores.
        let mut identity = None;
        if hints.is_some() || user_agent.contains("HeadlessChrome/") || brands.is_some() {
            let probed = hints.is_some();
            let metadata = match hints {
                Some(hints) => hints,
                None => browser.own_metadata().await?,
            };
            // A Chrome running with a --user-agent flag of someone else's
            // reports emptied hints, and an override would send them as
            // empty headers; it's left as it is.
            let emptied = metadata["architecture"] == "" && metadata["platformVersion"] == "";
            if probed || !emptied {
                identity = Some(metadata);
            }
        }
        if let Some(mut metadata) = identity {
            if let Some(header) = brands {
                let list = parse_brands(&header);
                metadata["brands"] = list
                    .iter()
                    .map(|(brand, version)| json!({"brand": brand, "version": version}))
                    .collect();
                // A brand at Chrome's own major version carries its full
                // version; the made-up one, as in Chrome, is N.0.0.0.
                metadata["fullVersionList"] = list
                    .iter()
                    .map(|(brand, version)| {
                        let full = if *version == major {
                            full.clone()
                        } else {
                            format!("{version}.0.0.0")
                        };
                        json!({"brand": brand, "version": full})
                    })
                    .collect();
            }
            let identity = Identity {
                user_agent: user_agent.replace("HeadlessChrome/", "Chrome/"),
                metadata,
            };
            // Nothing else holds the Arc yet.
            Arc::get_mut(&mut browser.inner)
                .expect("the browser isn't shared before launch returns")
                .identity = Some(identity);
        }
        Ok(browser)
    }

    /// The client hints Chrome reports for itself, read where no override
    /// is in force and the hints are visible: chrome://version.
    async fn own_metadata(&self) -> Result<Value> {
        let page = Page::open(self.inner.clone(), None, false, None, None).await?;
        let read = async {
            page.goto("chrome://version", WaitUntil::Load, None).await?;
            page.evaluate(
                "navigator.userAgentData.getHighEntropyValues(['architecture', 'bitness', \
                 'formFactors', 'fullVersionList', 'model', 'platformVersion', 'wow64'])",
            )
            .await
        }
        .await;
        page.close().await?;
        let values = read?;
        Ok(json!({
            "brands": values["brands"],
            "fullVersionList": values["fullVersionList"],
            "platform": values["platform"],
            "platformVersion": values["platformVersion"],
            "architecture": values["architecture"],
            "model": values["model"],
            "mobile": values["mobile"],
            "bitness": values["bitness"],
            "wow64": values["wow64"],
            "formFactors": values["formFactors"],
        }))
    }

    /// Chrome's product string, such as `Chrome/154.0.8037.98`.
    pub fn version(&self) -> &str {
        &self.inner.version
    }

    /// The launched Chrome's profile directory; None for a browser netweir
    /// connected to.
    pub fn profile_dir(&self) -> Option<&Path> {
        self.inner.profile.as_deref()
    }

    /// True once closed, and also once Chrome has exited or crashed (or,
    /// for a browser netweir connected to, the connection has).
    pub fn is_closed(&self) -> bool {
        self.inner.conn.is_closed() || (self.inner.profile.is_some() && self.child().is_none())
    }

    /// Chrome's process id, while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.child().as_ref().map(|c| c.id())
    }

    fn child(&self) -> std::sync::MutexGuard<'_, Option<Child>> {
        self.inner.child.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A page in a context of its own: no cookies or storage shared with
    /// any other page. Closing it closes the context. With a
    /// [`LaunchOptions::user_data_dir`], it's a page of the profile's own
    /// context instead, so what it keeps is saved there.
    pub async fn new_page(&self) -> Result<Page> {
        if self.inner.keep_profile {
            return Page::open(self.inner.clone(), None, false, None, None).await;
        }
        let context = self.create_context().await?;
        Page::open(self.inner.clone(), Some(context), true, None, None).await
    }

    /// Like [`Browser::new_page`], but every document the page's main
    /// frame is about to fetch (each hop of a redirect included) is put
    /// to `guard` first, and one it refuses isn't fetched: navigating there
    /// fails with `ERR_BLOCKED_BY_CLIENT`.
    pub async fn new_guarded_page(&self, guard: Guard) -> Result<Page> {
        let context = self.create_context().await?;
        Page::open(self.inner.clone(), Some(context), true, Some(guard), None).await
    }

    /// A context whose pages share cookies and storage with each other.
    pub async fn new_context(&self) -> Result<Context> {
        Ok(Context {
            browser: self.inner.clone(),
            id: self.create_context().await?,
            rules: Arc::default(),
        })
    }

    async fn create_context(&self) -> Result<String> {
        // Gone when the connection is, however netweir lets go of it.
        let r = self
            .inner
            .call(
                "",
                "Target.createBrowserContext",
                json!({"disposeOnDetach": true}),
            )
            .await?;
        let id = str_of(&r, "browserContextId");
        self.inner
            .contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id.clone());
        Ok(id)
    }

    /// Every CDP method sent so far, for tests that check nothing
    /// detectable was.
    pub fn methods_sent(&self) -> Vec<String> {
        self.inner.conn.methods_sent()
    }

    /// Asks Chrome to quit, waits for it (killing it after 10 seconds),
    /// and removes its profile directory, unless it was given one.
    pub async fn close(&self) -> Result<()> {
        let inner = self.inner.clone();
        if inner.profile.is_none() {
            // Connected, not launched: close what netweir opened, and go.
            let contexts =
                std::mem::take(&mut *inner.contexts.lock().unwrap_or_else(|e| e.into_inner()));
            for id in contexts {
                let dispose = inner.conn.call(
                    "",
                    "Target.disposeBrowserContext",
                    json!({"browserContextId": id}),
                );
                // A browser that doesn't answer won't hold the close up.
                let _ = tokio::time::timeout(Duration::from_secs(5), dispose).await;
            }
            inner.conn.shut();
            return Ok(());
        }
        if inner
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return Ok(());
        }
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            inner.conn.call("", "Browser.close", json!({})),
        )
        .await;
        tokio::task::spawn_blocking(move || inner.shut_down(Duration::from_secs(10)))
            .await
            .map_err(|e| Error::Launch(format!("closing the browser failed: {e}")))?
    }
}

impl Inner {
    /// Sends `method` and waits for the answer, for no longer than the
    /// timeout, with what Chrome has lately said in the error if it
    /// doesn't come.
    pub(crate) async fn call(&self, session: &str, method: &str, params: Value) -> Result<Value> {
        self.conn
            .call_within(self.timeout, session, method, params)
            .await
            .map_err(|e| self.explain(e))
    }

    /// A timeout, with what Chrome has lately said after it: why Chrome
    /// stopped answering is often there.
    pub(crate) fn explain(&self, e: Error) -> Error {
        match (e, &self.said) {
            (Error::Timeout(m), Some(said)) => Error::Timeout(said.lately(m)),
            (e, _) => e,
        }
    }

    fn shut_down(&self, grace: Duration) -> Result<()> {
        let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            // Connected, not launched: let go of the browser. Its contexts
            // go with the connection (disposeOnDetach).
            self.conn.shut();
            return Ok(());
        };
        self.conn.shut();
        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        match &self.profile {
            Some(profile) if !self.keep_profile => remove_profile(profile),
            _ => Ok(()),
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.shut_down(Duration::ZERO);
    }
}

/// Chrome's helper processes can hold files in the profile for a moment
/// after the browser exits, and Windows won't delete an open file.
fn remove_profile(dir: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                return Err(Error::Launch(format!(
                    "can't remove the profile directory {}: {e}",
                    dir.display()
                )));
            }
        }
    }
}

/// Keeps Chrome away from the system keyring, where a fresh profile goes
/// for the key it encrypts cookies with. A headless Chrome can't show the
/// prompt that asks for access, so it waits on one nobody can answer.
/// Nothing a page can see changes.
#[cfg(target_os = "macos")]
const KEYRING: &[&str] = &["--use-mock-keychain"];
#[cfg(all(unix, not(target_os = "macos")))]
const KEYRING: &[&str] = &["--password-store=basic"];
#[cfg(not(unix))]
const KEYRING: &[&str] = &[];

/// Keeps every page at full speed. Chrome treats a page that isn't the
/// visible tab as background, slows its timers, and on macOS asks the OS
/// to run its renderer at low priority; on a busy machine a new page could
/// take over 30 seconds to answer at all. Puppeteer and Playwright pass
/// the same switches.
const FOREGROUND: &[&str] = &[
    "--disable-renderer-backgrounding",
    "--disable-backgrounding-occluded-windows",
    "--disable-background-timer-throttling",
];

fn profile_dir() -> std::io::Result<PathBuf> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "netweir-chrome-{}-{}-{nanos}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// What a headless Chrome at `executable` says it is: its user agent and
/// its client hints, which only asking it tells exactly, and only before
/// a --user-agent flag empties the hints. The first time, it's started once
/// to ask.
async fn headless_identity(executable: &Path, extra: &[String]) -> Result<(String, Value)> {
    // By the file as it is now, so a Chrome updated in place is asked
    // again, and by the switches it's started with.
    type Key = (PathBuf, Option<std::time::SystemTime>, u64, Vec<String>);
    static KNOWN: Mutex<Option<HashMap<Key, (String, Value)>>> = Mutex::new(None);
    let meta = std::fs::metadata(executable).ok();
    let key: Key = (
        executable.to_path_buf(),
        meta.as_ref().and_then(|m| m.modified().ok()),
        meta.as_ref().map_or(0, |m| m.len()),
        // Switches can change what Chrome says it is.
        extra.to_vec(),
    );
    if let Some(known) = KNOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .get(&key)
    {
        return Ok(known.clone());
    }
    let profile =
        profile_dir().map_err(|e| Error::Launch(format!("can't make a profile directory: {e}")))?;
    // The caller's switches too: a Chrome that only starts with, say,
    // --no-sandbox must have it here as well.
    let mut args = vec![
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--headless".into(),
    ];
    args.extend(KEYRING.iter().chain(FOREGROUND).map(|a| a.to_string()));
    args.extend(extra.iter().cloned());
    args.push("about:blank".into());
    let started = match launch::start(executable, &args) {
        Ok(started) => started,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&profile);
            return Err(e);
        }
    };
    let said = started.said.clone();
    let probe = Browser {
        inner: Arc::new(Inner {
            conn: Connection::new(started.replies, started.commands),
            child: Mutex::new(Some(started.child)),
            profile: Some(profile),
            keep_profile: false,
            contexts: Mutex::default(),
            version: String::new(),
            identity: None,
            timeout: Duration::from_secs(30),
            proxy_login: None,
            said: Some(said.clone()),
        }),
    };
    let what = executable.display().to_string();
    let asked = async {
        let version = tokio::time::timeout(
            Duration::from_secs(30),
            probe.inner.conn.call("", "Browser.getVersion", json!({})),
        )
        .await
        .map_err(|_| Error::Launch(format!("{what} didn't answer in 30 seconds")))?
        .map_err(|e| Error::Launch(format!("{what} didn't answer: {e}")))?;
        let metadata = probe.own_metadata().await?;
        Ok::<_, Error>((str_of(&version, "userAgent"), metadata))
    }
    .await;
    // What it said counts even if it then wouldn't close cleanly.
    let _ = probe.close().await;
    let known = asked.map_err(|e| match e {
        Error::Launch(message) => Error::Launch(said.explain(message)),
        other => other,
    })?;
    KNOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(key, known.clone());
    Ok(known)
}

/// `host:port`, `host`, `[::1]:port` or `[::1]`, with 443 by default.
fn host_and_port(authority: &str) -> std::result::Result<(&str, u16), String> {
    let port = |p: &str| p.parse::<u16>().map_err(|e| format!("port {p:?}: {e}"));
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or("an unclosed [ in the address")?;
        return match after.strip_prefix(':') {
            Some(p) => Ok((host, port(p)?)),
            None => Ok((host, 443)),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, p)) => Ok((host, port(p)?)),
        None => Ok((authority, 443)),
    }
}

/// A `wss://` WebSocket over BoringSSL, the TLS library netweir's HTTP
/// client uses, trusting the roots Chrome trusts.
async fn secure_websocket(
    url: &str,
) -> std::result::Result<
    tokio_tungstenite::WebSocketStream<tokio_btls::SslStream<tokio::net::TcpStream>>,
    String,
> {
    use btls::ssl::{SslConnector, SslMethod};
    use btls::x509::X509;
    use btls::x509::store::X509StoreBuilder;

    let authority = url
        .trim_start_matches("wss://")
        .split('/')
        .next()
        .unwrap_or_default();
    let (host, port) = host_and_port(authority)?;
    let tcp = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| e.to_string())?;
    static ROOTS: std::sync::LazyLock<Vec<X509>> = std::sync::LazyLock::new(|| {
        chromium_roots::TLS_SERVER_ROOT_CERTS
            .iter()
            .filter_map(|der| X509::from_der(der.as_ref()).ok())
            .collect()
    });
    let mut store = X509StoreBuilder::new().map_err(|e| e.to_string())?;
    for root in ROOTS.iter() {
        store.add_cert(root).map_err(|e| e.to_string())?;
    }
    let mut connector = SslConnector::builder(SslMethod::tls()).map_err(|e| e.to_string())?;
    connector.set_cert_store(store.build());
    let ssl = connector
        .build()
        .configure()
        .and_then(|c| c.into_ssl(host))
        .map_err(|e| e.to_string())?;
    let mut tls = tokio_btls::SslStream::new(ssl, tcp).map_err(|e| e.to_string())?;
    std::pin::Pin::new(&mut tls)
        .connect()
        .await
        .map_err(|e| match tls.ssl().verify_result() {
            Err(why) => format!("the certificate wasn't trusted: {}", why.error_string()),
            Ok(()) => e.to_string(),
        })?;
    let (socket, _) = tokio_tungstenite::client_async(url, tls)
        .await
        .map_err(|e| e.to_string())?;
    Ok(socket)
}

/// The WebSocket URL a browser's `http://host:port/json/version` names.
async fn devtools_url(base: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let failed = |e: String| Error::Launch(format!("{base}: {e}"));
    let host = base
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let read = async {
        let mut stream = tokio::net::TcpStream::connect(&host)
            .await
            .map_err(|e| failed(e.to_string()))?;
        let request =
            format!("GET /json/version HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| failed(e.to_string()))?;
        // Chrome keeps the connection open whatever the request says, so
        // the body is read by its length, not to the end.
        let mut reply = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(at) = reply.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&reply[..at]).to_ascii_lowercase();
                let ok = head
                    .lines()
                    .next()
                    .is_some_and(|status| status.split(' ').nth(1) == Some("200"));
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .ok_or_else(|| failed("no Content-Length in the reply".into()))?;
                if reply.len() >= at + 4 + length {
                    let body = reply[at + 4..at + 4 + length].to_vec();
                    if !ok {
                        // Chrome says why, such as a host name it won't
                        // serve DevTools to.
                        let why = String::from_utf8_lossy(&body).trim().to_string();
                        return Err(failed(format!("refused: {why}")));
                    }
                    return Ok::<_, Error>(body);
                }
            }
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| failed(e.to_string()))?;
            if n == 0 {
                return Err(failed("the reply ended early".into()));
            }
            reply.extend_from_slice(&chunk[..n]);
        }
    };
    let body = tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .map_err(|_| failed("no answer in 30 seconds".into()))??;
    let version: Value =
        serde_json::from_slice(&body).map_err(|e| failed(format!("not DevTools: {e}")))?;
    version["webSocketDebuggerUrl"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| failed("no webSocketDebuggerUrl in /json/version".into()))
}

/// `"Chromium";v="154", "Google Chrome";v="154"` as (brand, version)
/// pairs.
fn parse_brands(header: &str) -> Vec<(String, String)> {
    header
        .split(',')
        .filter_map(|part| {
            let (brand, version) = part.trim().split_once(";v=")?;
            Some((
                brand.trim_matches('"').to_string(),
                version.trim_matches('"').to_string(),
            ))
        })
        .collect()
}

/// A proxy URL as Chrome takes it (`scheme://host:port`, which has no
/// room for a login), and the username and password that were in it.
fn split_proxy(proxy: &str) -> Result<(String, Option<(String, String)>)> {
    // Without the login: an error mustn't repeat a password.
    let shown = match proxy.rsplit_once('@') {
        Some((before, host)) => match before.split_once("://") {
            Some((scheme, _)) => format!("{scheme}://…@{host}"),
            None => format!("…@{host}"),
        },
        None => proxy.to_string(),
    };
    let bad = || Error::Invalid(format!("not a proxy URL: {shown}"));
    let (scheme, rest) = proxy.split_once("://").ok_or_else(bad)?;
    let rest = rest.trim_end_matches('/');
    let Some((login, host)) = rest.rsplit_once('@') else {
        return Ok((format!("{scheme}://{rest}"), None));
    };
    if host.is_empty() {
        return Err(bad());
    }
    if scheme.starts_with("socks") {
        return Err(Error::Invalid(format!(
            "Chrome can't log in to a SOCKS proxy ({shown}); a login works for http:// and \
             https:// proxies"
        )));
    }
    let (user, password) = login.split_once(':').unwrap_or((login, ""));
    Ok((
        format!("{scheme}://{host}"),
        Some((unescape(user), unescape(password))),
    ))
}

/// Undoes URL percent-escapes, so a password can hold `@` or `:`.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Pages that share cookies and storage.
#[derive(Clone)]
pub struct Context {
    browser: Arc<Inner>,
    id: String,
    /// What every page of the context routes.
    rules: Arc<ContextRules>,
}

impl Context {
    pub async fn new_page(&self) -> Result<Page> {
        Page::open(
            self.browser.clone(),
            Some(self.id.clone()),
            false,
            None,
            Some(self.rules.clone()),
        )
        .await
    }

    /// Like [`Page::route`], for every page of the context, those open
    /// now and those opened later. A page's own routes come first.
    pub async fn route(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<Route>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        crate::network::lock(&self.rules.rules).routes = Some(tx);
        self.refresh().await?;
        Ok(rx)
    }

    /// Stops the context's routing; requests waiting for a decision go on
    /// unchanged.
    pub async fn unroute(&self) -> Result<()> {
        crate::network::lock(&self.rules.rules).routes = None;
        self.refresh().await
    }

    /// Like [`Page::drop_routes`], for the context's routes.
    pub fn drop_routes(&self) {
        crate::network::lock(&self.rules.rules).routes = None;
    }

    async fn refresh(&self) -> Result<()> {
        for page in self.rules.pages() {
            match page.refresh_fetch().await {
                Ok(()) | Err(Error::PageClosed) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub async fn cookies(&self) -> Result<Vec<Cookie>> {
        context_cookies(&self.browser, Some(&self.id)).await
    }

    pub async fn set_cookies(&self, cookies: &[Cookie]) -> Result<()> {
        set_context_cookies(&self.browser, Some(&self.id), cookies).await
    }

    /// Closes the context and every page in it.
    pub async fn close(&self) -> Result<()> {
        match self
            .browser
            .call(
                "",
                "Target.disposeBrowserContext",
                json!({"browserContextId": self.id}),
            )
            .await
        {
            Ok(_) | Err(Error::Closed) => Ok(()),
            // Already disposed.
            Err(Error::Protocol { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

pub(crate) async fn context_cookies(browser: &Inner, context: Option<&str>) -> Result<Vec<Cookie>> {
    let mut params = json!({});
    if let Some(id) = context {
        params["browserContextId"] = Value::from(id);
    }
    let r = browser.call("", "Storage.getCookies", params).await?;
    Ok(cookies_from(&r["cookies"]))
}

pub(crate) async fn set_context_cookies(
    browser: &Inner,
    context: Option<&str>,
    cookies: &[Cookie],
) -> Result<()> {
    let mut params = json!({"cookies": cookies_to(cookies)});
    if let Some(id) = context {
        params["browserContextId"] = Value::from(id);
    }
    browser.call("", "Storage.setCookies", params).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxy_login_comes_off_the_url_chrome_gets() {
        assert_eq!(
            split_proxy("http://proxy:8080").unwrap(),
            ("http://proxy:8080".into(), None)
        );
        assert_eq!(
            split_proxy("http://ann:p%40ss%3Aword@10.0.0.1:1080/").unwrap(),
            (
                "http://10.0.0.1:1080".into(),
                Some(("ann".into(), "p@ss:word".into()))
            )
        );
        assert_eq!(
            split_proxy("socks5://10.0.0.1:1080").unwrap(),
            ("socks5://10.0.0.1:1080".into(), None)
        );
        assert!(split_proxy("proxy:8080").is_err());
        assert!(split_proxy("http://ann@").is_err());
    }

    #[test]
    fn chrome_cant_log_in_to_a_socks_proxy_so_it_isnt_asked_to() {
        let err = split_proxy("socks5://ann:secret@10.0.0.1:1080").unwrap_err();
        assert!(err.to_string().contains("SOCKS"), "{err}");
    }

    #[test]
    fn a_bad_proxy_url_doesnt_repeat_its_password() {
        for bad in ["http://ann:hunter2@", "ann:hunter2@proxy"] {
            let err = split_proxy(bad).unwrap_err().to_string();
            assert!(!err.contains("hunter2"), "{err}");
        }
    }

    #[test]
    fn wss_addresses_come_apart_into_host_and_port() {
        assert_eq!(host_and_port("example.com"), Ok(("example.com", 443)));
        assert_eq!(host_and_port("example.com:9222"), Ok(("example.com", 9222)));
        assert_eq!(host_and_port("[::1]"), Ok(("::1", 443)));
        assert_eq!(host_and_port("[::1]:9222"), Ok(("::1", 9222)));
        assert!(host_and_port("[::1").is_err());
        assert!(host_and_port("host:nope").is_err());
    }

    #[test]
    fn brands_that_dont_parse_leave_chromes_own() {
        assert!(parse_brands("nonsense").is_empty());
    }
}
