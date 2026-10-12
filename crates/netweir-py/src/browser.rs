//! Bindings for netweir-browser. Every method that talks to Chrome returns
//! an awaitable.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use netweir_browser::{
    Browser as CoreBrowser, Context as CoreContext, Continue, Cookie, Error, LaunchOptions,
    NetworkEvent, Page as CorePage, Request as CoreRequest, Response as CoreResponse,
    Route as CoreRoute, WaitUntil,
};
use netweir_dom::Document;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use crate::Node;

/// Raises the Python exception that matches `e`.
fn raise(e: Error) -> PyErr {
    Python::attach(|py| {
        let made = (|| -> PyResult<PyErr> {
            let errors = py.import("netweir._errors")?;
            let value = match &e {
                Error::Invalid(m) => return Ok(PyValueError::new_err(m.clone())),
                Error::Timeout(_) => errors.getattr("BrowserTimeout")?.call1((e.to_string(),))?,
                Error::Navigation(m) => {
                    // Chrome's net error names: the connection never
                    // happened, ran out of time, or failed TLS. Anything
                    // else, such as ERR_ABORTED for a 204 or a download,
                    // reached the server.
                    let kind = if m.contains("TIMED_OUT") {
                        "timeout"
                    } else if m.contains("CERT") || m.contains("SSL") {
                        "tls"
                    } else if [
                        "CONNECTION",
                        "NAME_NOT_RESOLVED",
                        "ADDRESS",
                        "INTERNET_DISCONNECTED",
                        "PROXY",
                    ]
                    .iter()
                    .any(|n| m.contains(n))
                    {
                        "connect"
                    } else {
                        "other"
                    };
                    errors.getattr("FetchError")?.call1((m.clone(), kind))?
                }
                _ => errors.getattr("BrowserError")?.call1((e.to_string(),))?,
            };
            Ok(PyErr::from_value(value))
        })();
        made.unwrap_or_else(|err| err)
    })
}

fn seconds(name: &str, value: Option<f64>) -> PyResult<Option<Duration>> {
    match value {
        None => Ok(None),
        Some(t) if t > 0.0 => Duration::try_from_secs_f64(t)
            .map(Some)
            .map_err(|_| PyValueError::new_err(format!("{name} is too long: {t} seconds"))),
        Some(_) => Err(PyValueError::new_err(format!(
            "{name} must be a positive number of seconds"
        ))),
    }
}

/// Starts Chrome. Resolves to a Browser.
#[pyfunction]
#[pyo3(signature = (executable=None, headless=true, args=Vec::new(), timeout=30.0, proxy=None, connect=None))]
pub(crate) fn launch(
    py: Python<'_>,
    executable: Option<PathBuf>,
    headless: bool,
    args: Vec<String>,
    timeout: f64,
    proxy: Option<String>,
    connect: Option<String>,
) -> PyResult<Bound<'_, PyAny>> {
    if connect.is_some()
        && (executable.is_some() || proxy.is_some() || !args.is_empty() || !headless)
    {
        return Err(PyValueError::new_err(
            "connect= drives a browser that's already running, so executable, args, proxy and \
             headless don't apply",
        ));
    }
    let timeout = seconds("timeout", Some(timeout))?.expect("checked above");
    let options = LaunchOptions {
        executable,
        headless,
        args,
        timeout,
        proxy,
        // Chrome for Testing presents itself as Google Chrome would.
        brands: Some(Arc::new(netweir_core::Profile::chrome_brands)),
    };
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let inner = match connect {
            Some(url) => CoreBrowser::connect(&url, options).await,
            None => CoreBrowser::launch(options).await,
        }
        .map_err(raise)?;
        Ok(Browser { inner })
    })
}

/// A running Chrome. Closing it closes every page.
#[pyclass(frozen, module = "netweir")]
pub struct Browser {
    inner: CoreBrowser,
}

#[pymethods]
impl Browser {
    /// Chrome's version, such as "Chrome/154.0.8037.98".
    #[getter]
    fn version(&self) -> String {
        self.inner.version().to_string()
    }

    #[getter]
    fn closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// A page in a context of its own: it shares no cookies or storage.
    fn new_page<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let inner = browser.new_page().await.map_err(raise)?;
            Ok(BrowserPage::new(inner, None))
        })
    }

    /// A context whose pages share cookies and storage.
    fn new_context<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let inner = browser.new_context().await.map_err(raise)?;
            Ok(BrowserContext {
                inner,
                network: OnceLock::new(),
            })
        })
    }

    /// Every DevTools method sent so far.
    fn methods_sent(&self) -> Vec<String> {
        self.inner.methods_sent()
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            browser.close().await.map_err(raise)
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let me = slf.clone().unbind();
        pyo3_async_runtimes::tokio::future_into_py(slf.py(), async move { Ok(me) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _kind: Bound<'py, PyAny>,
        _value: Bound<'py, PyAny>,
        _traceback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }

    fn __repr__(&self) -> String {
        format!("<Browser {}>", self.inner.version())
    }
}

/// Pages that share cookies and storage.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserContext {
    inner: CoreContext,
    /// The context's routes, kept in Python (netweir._network).
    network: OnceLock<Py<PyAny>>,
}

#[pymethods]
impl BrowserContext {
    fn new_page<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let context = slf.get().inner.clone();
        let me = slf.clone().unbind();
        pyo3_async_runtimes::tokio::future_into_py(slf.py(), async move {
            let inner = context.new_page().await.map_err(raise)?;
            Ok(BrowserPage::new(inner, Some(me)))
        })
    }

    /// Routes every page of the context: each request whose URL matches
    /// `pattern` (a glob, a compiled regex, or a callable taking the URL)
    /// goes to `handler`, called with a Route to decide about.
    fn route<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        handler: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        network(slf.py(), &slf.get().network, None)?.call_method1("route", (slf, pattern, handler))
    }

    /// Removes the routes with `pattern` (only the one with `handler`, if
    /// given).
    #[pyo3(signature = (pattern, handler=None))]
    fn unroute<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        handler: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        network(slf.py(), &slf.get().network, None)?
            .call_method1("unroute", (slf, pattern, handler))
    }

    fn _route<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let rx = context.route().await.map_err(raise)?;
            Ok(Routes::new(rx))
        })
    }

    fn _unroute<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            context.unroute().await.map_err(raise)
        })
    }

    /// Stops handing requests to the context's routes, at once.
    fn _drop_routes(&self) {
        self.inner.drop_routes();
    }

    /// The context's routes, for its pages to fall back on.
    fn _network<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        network(py, &self.network, None)
    }

    fn cookies<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let cookies = context.cookies().await.map_err(raise)?;
            Python::attach(|py| cookie_list(py, &cookies))
        })
    }

    fn set_cookies<'py>(
        &self,
        py: Python<'py>,
        cookies: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cookies = cookies
            .iter()
            .map(cookie_from)
            .collect::<PyResult<Vec<_>>>()?;
        let context = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            context.set_cookies(&cookies).await.map_err(raise)
        })
    }

    /// Closes the context and every page in it.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            context.close().await.map_err(raise)
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let me = slf.clone().unbind();
        pyo3_async_runtimes::tokio::future_into_py(slf.py(), async move { Ok(me) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _kind: Bound<'py, PyAny>,
        _value: Bound<'py, PyAny>,
        _traceback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }
}

/// A page in Chrome.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserPage {
    inner: CorePage,
    /// For a page a crawl fetched: closing it frees its place in the
    /// crawl's pool.
    live: Option<netweir_core::LivePage>,
    /// The shared context the page was opened in, whose routes it falls
    /// back on.
    context: Option<Py<BrowserContext>>,
    /// The page's listeners, waiters and routes, kept in Python
    /// (netweir._network).
    network: OnceLock<Py<PyAny>>,
}

impl BrowserPage {
    fn new(inner: CorePage, context: Option<Py<BrowserContext>>) -> BrowserPage {
        BrowserPage {
            inner,
            live: None,
            context,
            network: OnceLock::new(),
        }
    }

    pub(crate) fn from_crawl(live: netweir_core::LivePage) -> BrowserPage {
        BrowserPage {
            inner: live.page().clone(),
            live: Some(live),
            context: None,
            network: OnceLock::new(),
        }
    }

    fn network<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let parent = match &self.context {
            Some(c) => Some(network(py, &c.get().network, None)?),
            None => None,
        };
        network(py, &self.network, parent)
    }
}

/// The netweir._network.Network kept in `slot`, made the first time.
fn network<'py>(
    py: Python<'py>,
    slot: &OnceLock<Py<PyAny>>,
    parent: Option<Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    if let Some(n) = slot.get() {
        return Ok(n.bind(py).clone());
    }
    let made = py
        .import("netweir._network")?
        .getattr("Network")?
        .call1((parent,))?;
    // Another thread may have got there first; its one is kept.
    Ok(slot.get_or_init(|| made.unbind()).bind(py).clone())
}

/// What the caller passed as headers: a dict, or (name, value) pairs.
fn header_pairs(headers: &Bound<'_, PyAny>) -> PyResult<Vec<(String, String)>> {
    let items = match headers.cast::<PyDict>() {
        Ok(d) => d.items().into_any(),
        Err(_) => headers.clone(),
    };
    items
        .try_iter()?
        .map(|pair| pair?.extract::<(String, String)>())
        .collect::<PyResult<_>>()
        .map_err(|_| {
            PyTypeError::new_err("headers must be a dict, or (name, value) pairs, of strings")
        })
}

/// What the caller passed as a body: bytes, or a str sent as UTF-8.
fn body_bytes(body: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(s) = body.extract::<String>() {
        return Ok(s.into_bytes());
    }
    body.extract::<Vec<u8>>()
        .map_err(|_| PyTypeError::new_err("body must be bytes or a str"))
}

fn delegate<'py>(
    slf: &Bound<'py, BrowserPage>,
    method: &str,
    args: impl pyo3::call::PyCallArgs<'py>,
) -> PyResult<Bound<'py, PyAny>> {
    slf.get().network(slf.py())?.call_method1(method, args)
}

#[pymethods]
impl BrowserPage {
    /// The current URL.
    #[getter]
    fn url(&self) -> String {
        self.inner.url()
    }

    #[getter]
    fn closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Navigates and waits for "domcontentloaded", "load" or
    /// "networkidle". Resolves to a BrowserResponse.
    #[pyo3(signature = (url, wait="load", timeout=None))]
    fn goto<'py>(
        &self,
        py: Python<'py>,
        url: String,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait: WaitUntil = wait.parse().map_err(raise)?;
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let inner = page.goto(&url, wait, timeout).await.map_err(raise)?;
            Ok(BrowserResponse::new(inner, &page))
        })
    }

    /// Waits for a navigation away from the document showing now to reach
    /// `wait`. Resolves to the BrowserResponse of the new document.
    #[pyo3(signature = (wait="load", timeout=None))]
    fn wait_for_navigation<'py>(
        &self,
        py: Python<'py>,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait: WaitUntil = wait.parse().map_err(raise)?;
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        // The document showing now, read before anything is awaited, so a
        // navigation that starts while the future waits to run still
        // counts.
        let after = page.response();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let inner = page
                .wait_for_navigation(&after, wait, timeout)
                .await
                .map_err(raise)?;
            Ok(BrowserResponse::new(inner, &page))
        })
    }

    /// The HTML as Chrome now has it.
    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.content().await.map_err(raise)
        })
    }

    /// The page as it is now, parsed into a netweir Node.
    fn parse<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let html = page.content().await.map_err(raise)?;
            let doc = tokio::task::spawn_blocking(move || Document::parse(&html))
                .await
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            Ok(Node::root_of(Arc::new(doc)))
        })
    }

    fn title<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(
            py,
            async move { page.title().await.map_err(raise) },
        )
    }

    /// Runs `js` in the page: an expression, or a function, which is
    /// called. Resolves to the result as Python values.
    fn evaluate<'py>(&self, py: Python<'py>, js: String) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let value = page.evaluate(&js).await.map_err(raise)?;
            Python::attach(|py| to_python(py, &value))
        })
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn click<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.click(&selector, timeout).await.map_err(raise)
        })
    }

    #[pyo3(signature = (selector, text, timeout=None))]
    fn fill<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        text: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.fill(&selector, &text, timeout).await.map_err(raise)
        })
    }

    fn press<'py>(&self, py: Python<'py>, key: String) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.press(&key).await.map_err(raise)
        })
    }

    #[pyo3(signature = (selector, state="visible".to_string(), timeout=None))]
    fn wait_for<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        state: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.wait_for(&selector, &state, timeout)
                .await
                .map_err(raise)
        })
    }

    /// PNG bytes, also written to `path` if given.
    #[pyo3(signature = (path=None, full_page=false))]
    fn screenshot<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        full_page: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let png = page.screenshot(full_page).await.map_err(raise)?;
            if let Some(path) = path {
                std::fs::write(&path, &png)?;
            }
            Python::attach(|py| Ok(PyBytes::new(py, &png).unbind()))
        })
    }

    /// The cookies of the page's context, as dicts.
    fn cookies<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let cookies = page.cookies().await.map_err(raise)?;
            Python::attach(|py| cookie_list(py, &cookies))
        })
    }

    fn set_cookies<'py>(
        &self,
        py: Python<'py>,
        cookies: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cookies = cookies
            .iter()
            .map(cookie_from)
            .collect::<PyResult<Vec<_>>>()?;
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.set_cookies(&cookies).await.map_err(raise)
        })
    }

    /// Calls `callback` (a function or a coroutine function) with each
    /// response the page gets.
    fn on_response<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "on", (slf, "response", callback))
    }

    fn remove_response_listener<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "off", ("response", callback))
    }

    /// Calls `callback` with each request the page makes.
    fn on_request<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "on", (slf, "request", callback))
    }

    fn remove_request_listener<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "off", ("request", callback))
    }

    /// Waits for a response whose URL matches `pattern` (a glob or a
    /// compiled regex), or for which `pattern` (a callable) is true.
    #[pyo3(signature = (pattern, timeout=None))]
    fn wait_for_response<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "wait", (slf, "response", pattern, timeout))
    }

    #[pyo3(signature = (pattern, timeout=None))]
    fn wait_for_request<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "wait", (slf, "request", pattern, timeout))
    }

    /// `wait_for_response` as an async context manager, so the wait starts
    /// before what makes the request: `async with page.expect_response(p)
    /// as info:`, then `await info.value`.
    #[pyo3(signature = (pattern, timeout=None))]
    fn expect_response<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "expect", (slf, "response", pattern, timeout))
    }

    #[pyo3(signature = (pattern, timeout=None))]
    fn expect_request<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "expect", (slf, "request", pattern, timeout))
    }

    /// Each request whose URL matches `pattern` (a glob, a compiled regex,
    /// or a callable taking the URL) goes to `handler`, called with a
    /// Route to decide about.
    fn route<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        handler: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "route", (slf, pattern, handler))
    }

    /// Removes the routes with `pattern` (only the one with `handler`, if
    /// given).
    #[pyo3(signature = (pattern, handler=None))]
    fn unroute<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        handler: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "unroute", (slf, pattern, handler))
    }

    /// Fails, before they leave Chrome, requests of the resource types
    /// given and those whose URLs match one of the globs in `urls`.
    #[pyo3(signature = (resource_types=vec!["image".to_string(), "font".to_string(), "media".to_string()], urls=Vec::new()))]
    fn block<'py>(
        &self,
        py: Python<'py>,
        resource_types: Vec<String>,
        urls: Vec<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let types: Vec<&str> = resource_types.iter().map(String::as_str).collect();
            page.block(&types, &urls).await.map_err(raise)
        })
    }

    /// Hears every request and response from now on.
    fn _listen(&self) -> NetworkEvents {
        NetworkEvents {
            rx: Arc::new(tokio::sync::Mutex::new(self.inner.listen())),
            page: self.inner.clone(),
        }
    }

    fn _route<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let rx = page.route().await.map_err(raise)?;
            Ok(Routes::new(rx))
        })
    }

    fn _unroute<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            page.unroute().await.map_err(raise)
        })
    }

    /// Stops handing requests to the page's routes, at once.
    fn _drop_routes(&self) {
        self.inner.drop_routes();
    }

    /// The browser's default limit, in seconds.
    #[getter]
    fn _timeout(&self) -> f64 {
        self.inner.default_timeout().as_secs_f64()
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let (page, live) = (self.inner.clone(), self.live.clone());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            match live {
                Some(live) => {
                    live.close().await;
                    Ok(())
                }
                None => page.close().await.map_err(raise),
            }
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let me = slf.clone().unbind();
        pyo3_async_runtimes::tokio::future_into_py(slf.py(), async move { Ok(me) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _kind: Bound<'py, PyAny>,
        _value: Bound<'py, PyAny>,
        _traceback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }

    fn __repr__(&self) -> String {
        format!("<BrowserPage {}>", self.inner.url())
    }
}

/// A response the page got: the main document's to a navigation, or any
/// other the page's listeners heard about.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserResponse {
    inner: CoreResponse,
    /// The page that got it, which has its body.
    page: CorePage,
}

impl BrowserResponse {
    fn new(inner: CoreResponse, page: &CorePage) -> BrowserResponse {
        BrowserResponse {
            inner,
            page: page.clone(),
        }
    }

    fn read<'py>(
        &self,
        py: Python<'py>,
        then: fn(Python<'_>, Vec<u8>) -> PyResult<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (page, response) = (self.page.clone(), self.inner.clone());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let body = page.body(&response).await.map_err(raise)?;
            Python::attach(|py| then(py, body))
        })
    }
}

#[pymethods]
impl BrowserResponse {
    #[getter]
    fn url(&self) -> &str {
        &self.inner.url
    }

    /// 0 when there was no HTTP response (about:, data:).
    #[getter]
    fn status(&self) -> u16 {
        self.inner.status
    }

    #[getter]
    fn headers(&self) -> Vec<(String, String)> {
        self.inner.headers.clone()
    }

    /// The request's method.
    #[getter]
    fn method(&self) -> &str {
        &self.inner.method
    }

    /// "document", "xhr", "fetch", "image" and so on.
    #[getter]
    fn resource_type(&self) -> &str {
        &self.inner.resource_type
    }

    /// The body as bytes, once it has all arrived.
    fn body<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.read(py, |py, body| {
            Ok(PyBytes::new(py, &body).into_any().unbind())
        })
    }

    /// The body as text, read as UTF-8.
    fn text<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.read(py, |py, body| {
            Ok(String::from_utf8_lossy(&body)
                .into_pyobject(py)?
                .into_any()
                .unbind())
        })
    }

    /// The body parsed as JSON.
    fn json<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.read(py, |py, body| {
            Ok(py
                .import("json")?
                .call_method1("loads", (PyBytes::new(py, &body),))?
                .unbind())
        })
    }

    fn __repr__(&self) -> String {
        format!("<BrowserResponse {} {}>", self.inner.status, self.inner.url)
    }
}

/// A request the page made.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserRequest {
    inner: CoreRequest,
}

#[pymethods]
impl BrowserRequest {
    #[getter]
    fn url(&self) -> &str {
        &self.inner.url
    }

    #[getter]
    fn method(&self) -> &str {
        &self.inner.method
    }

    #[getter]
    fn headers(&self) -> Vec<(String, String)> {
        self.inner.headers.clone()
    }

    /// "document", "xhr", "fetch", "image" and so on.
    #[getter]
    fn resource_type(&self) -> &str {
        &self.inner.resource_type
    }

    /// The body sent with it, as bytes; None when there was none.
    #[getter]
    fn post_data<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.inner.post_data.as_ref().map(|b| PyBytes::new(py, b))
    }

    fn __repr__(&self) -> String {
        format!("<BrowserRequest {} {}>", self.inner.method, self.inner.url)
    }
}

/// A page's requests and responses, as they happen, for netweir._network.
#[pyclass(frozen, module = "netweir._native")]
pub struct NetworkEvents {
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<NetworkEvent>>>,
    page: CorePage,
}

#[pymethods]
impl NetworkEvents {
    /// Stops listening: the page sends nothing more here. A `next`
    /// still waiting finishes first.
    fn close(&self) {
        let rx = self.rx.clone();
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move { rx.lock().await.close() });
    }

    /// Resolves to the next BrowserRequest or BrowserResponse, or None
    /// once the page has closed.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let (rx, page) = (self.rx.clone(), self.page.clone());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let event = rx.lock().await.recv().await;
            Python::attach(|py| -> PyResult<Py<PyAny>> {
                Ok(match event {
                    None => py.None(),
                    Some(NetworkEvent::Request(inner)) => {
                        Py::new(py, BrowserRequest { inner })?.into_any()
                    }
                    Some(NetworkEvent::Response(inner)) => {
                        Py::new(py, BrowserResponse::new(inner, &page))?.into_any()
                    }
                })
            })
        })
    }
}

/// Requests the routes of a page or a context get to decide about, for
/// netweir._network.
#[pyclass(frozen, module = "netweir._native")]
pub struct Routes {
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<CoreRoute>>>,
}

impl Routes {
    fn new(rx: tokio::sync::mpsc::UnboundedReceiver<CoreRoute>) -> Routes {
        Routes {
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
        }
    }
}

#[pymethods]
impl Routes {
    /// Resolves to the next Route, or None once routing has stopped.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.rx.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let route = rx.lock().await.recv().await;
            Python::attach(|py| -> PyResult<Py<PyAny>> {
                Ok(match route {
                    None => py.None(),
                    Some(inner) => Py::new(py, Route { inner })?.into_any(),
                })
            })
        })
    }
}

/// A request held in Chrome until a route handler decides about it:
/// `abort()`, `continue_()` or `fulfill()`, once. One the handler leaves
/// undecided goes on unchanged when the handler returns.
#[pyclass(frozen, module = "netweir")]
pub struct Route {
    inner: CoreRoute,
}

#[pymethods]
impl Route {
    #[getter]
    fn request(&self) -> BrowserRequest {
        BrowserRequest {
            inner: self.inner.request().clone(),
        }
    }

    /// Whether it's been aborted, continued or fulfilled.
    #[getter]
    fn handled(&self) -> bool {
        self.inner.is_decided()
    }

    /// Fails the request; the page sees a network error.
    fn abort<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let route = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(
            py,
            async move { route.abort().await.map_err(raise) },
        )
    }

    /// Lets the request go on, changed as asked. `headers` replace all the
    /// request's headers.
    #[pyo3(signature = (url=None, method=None, headers=None, body=None))]
    fn continue_<'py>(
        &self,
        py: Python<'py>,
        url: Option<String>,
        method: Option<String>,
        headers: Option<Bound<'py, PyAny>>,
        body: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let with = Continue {
            url,
            method,
            headers: headers.as_ref().map(header_pairs).transpose()?,
            body: body.as_ref().map(body_bytes).transpose()?,
        };
        let route = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            route.continue_(with).await.map_err(raise)
        })
    }

    /// Answers the request without it leaving Chrome. `json` is sent as
    /// JSON, with an application/json content type unless `headers` give
    /// one.
    #[pyo3(signature = (status=200, headers=None, body=None, json=None))]
    fn fulfill<'py>(
        &self,
        py: Python<'py>,
        status: u16,
        headers: Option<Bound<'py, PyAny>>,
        body: Option<Bound<'py, PyAny>>,
        json: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut headers = headers
            .as_ref()
            .map(header_pairs)
            .transpose()?
            .unwrap_or_default();
        let body = match (body, json) {
            (Some(_), Some(_)) => {
                return Err(PyValueError::new_err("give body or json, not both"));
            }
            (Some(body), None) => body_bytes(&body)?,
            (None, Some(json)) => {
                if !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                {
                    headers.push(("Content-Type".into(), "application/json".into()));
                }
                py.import("json")?
                    .call_method1("dumps", (json,))?
                    .extract::<String>()?
                    .into_bytes()
            }
            (None, None) => Vec::new(),
        };
        let route = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            route.fulfill(status, &headers, &body).await.map_err(raise)
        })
    }

    /// Lets the request go on unchanged, unless it's been decided.
    fn _release(&self) {
        self.inner.release();
    }

    fn __repr__(&self) -> String {
        let r = self.inner.request();
        format!("<Route {} {}>", r.method, r.url)
    }
}

/// Whether `url` matches the glob `pattern`, as routes and waiters match
/// them.
#[pyfunction]
fn _glob_match(pattern: &str, url: &str) -> bool {
    netweir_browser::glob_match(pattern, url)
}

fn cookie_list(py: Python<'_>, cookies: &[Cookie]) -> PyResult<Py<PyAny>> {
    let list = PyList::empty(py);
    for c in cookies {
        let d = PyDict::new(py);
        d.set_item("name", &c.name)?;
        d.set_item("value", &c.value)?;
        d.set_item("domain", &c.domain)?;
        d.set_item("path", &c.path)?;
        d.set_item("expires", c.expires)?;
        d.set_item("http_only", c.http_only)?;
        d.set_item("secure", c.secure)?;
        d.set_item("same_site", &c.same_site)?;
        list.append(d)?;
    }
    Ok(list.into_any().unbind())
}

fn cookie_from(d: &Bound<'_, PyAny>) -> PyResult<Cookie> {
    let d = d
        .cast::<PyDict>()
        .map_err(|_| PyTypeError::new_err("each cookie must be a dict"))?;
    let text = |key: &str| -> PyResult<Option<String>> {
        d.get_item(key)?
            .filter(|v| !v.is_none())
            .map(|v| v.extract())
            .transpose()
    };
    let flag = |key: &str| -> PyResult<bool> {
        Ok(d.get_item(key)?
            .map(|v| v.is_truthy())
            .transpose()?
            .unwrap_or(false))
    };
    let required = |key: &str| -> PyResult<String> {
        text(key)?.ok_or_else(|| PyValueError::new_err(format!("a cookie needs a {key:?}")))
    };
    Ok(Cookie {
        name: required("name")?,
        value: required("value")?,
        domain: required("domain")?,
        path: text("path")?.unwrap_or_else(|| "/".into()),
        expires: d
            .get_item("expires")?
            .filter(|v| !v.is_none())
            .map(|v| v.extract())
            .transpose()?
            .unwrap_or(-1.0),
        http_only: flag("http_only")?,
        secure: flag("secure")?,
        same_site: text("same_site")?,
    })
}

/// JSON as Python values.
pub(crate) fn to_python(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    use serde_json::Value;
    Ok(match value {
        Value::Null => py.None(),
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into_pyobject(py)?.into_any().unbind(),
            None => n
                .as_f64()
                .unwrap_or(f64::NAN)
                .into_pyobject(py)?
                .into_any()
                .unbind(),
        },
        Value::String(s) => s.into_pyobject(py)?.into_any().unbind(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(to_python(py, item)?)?;
            }
            list.into_any().unbind()
        }
        Value::Object(map) => {
            let d = PyDict::new(py);
            for (k, v) in map {
                d.set_item(k, to_python(py, v)?)?;
            }
            d.into_any().unbind()
        }
    })
}

/// Installs the Chrome for Testing build matching netweir's newest Chrome
/// profile under `home` (`$NETWEIR_HOME` or `~/.netweir` by default), as
/// `index` lists it. Returns (version, executable, already installed).
#[pyfunction]
#[pyo3(signature = (headless_shell=false, index=netweir_core::install::CHROME_FOR_TESTING.to_string(), home=None))]
fn install_chrome(
    py: Python<'_>,
    headless_shell: bool,
    index: String,
    home: Option<PathBuf>,
) -> PyResult<(String, PathBuf, bool)> {
    let home = home.unwrap_or_else(netweir_browser::netweir_home);
    let runtime = pyo3_async_runtimes::tokio::get_runtime();
    py.detach(|| {
        runtime.block_on(netweir_core::install::install_chrome(
            &index,
            &home,
            headless_shell,
        ))
    })
    .map(|i| (i.version, i.executable, i.already))
    .map_err(|e| {
        Python::attach(|py| -> PyErr {
            match py
                .import("netweir._errors")
                .and_then(|m| m.getattr("BrowserError"))
                .and_then(|c| c.call1((e.clone(),)))
            {
                Ok(v) => PyErr::from_value(v),
                Err(err) => err,
            }
        })
    })
}

/// The Chrome `netweir.browser()` would start, or BrowserError saying
/// where it looked.
#[pyfunction]
fn find_chrome() -> PyResult<PathBuf> {
    netweir_browser::find_chrome().map_err(raise)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(launch, m)?)?;
    m.add_function(wrap_pyfunction!(find_chrome, m)?)?;
    m.add_function(wrap_pyfunction!(install_chrome, m)?)?;
    m.add_class::<Browser>()?;
    m.add_class::<BrowserContext>()?;
    m.add_class::<BrowserPage>()?;
    m.add_class::<BrowserResponse>()?;
    m.add_class::<BrowserRequest>()?;
    m.add_class::<Route>()?;
    m.add_class::<NetworkEvents>()?;
    m.add_class::<Routes>()?;
    m.add_function(wrap_pyfunction!(_glob_match, m)?)?;
    m.add(
        "_RESOURCE_TYPES",
        netweir_browser::resource_types().collect::<Vec<_>>(),
    )?;
    Ok(())
}
