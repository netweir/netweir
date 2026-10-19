//! Fetching from Python: `Fetcher.get` is awaitable, `Fetcher.get_blocking`
//! is for scripts. Both run on one shared tokio runtime with the GIL
//! released.

use std::sync::Arc;
use std::time::Duration;

use netweir_core::classify::{Outcome, classify};
use netweir_core::{
    Auth, FetchError as CoreError, FetchErrorKind, FetchOptions, Fetcher as CoreFetcher, Policy,
    Profile,
};
use netweir_dom::Document;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::Node;

/// A `netweir.FetchError(message, kind)`, defined in Python so it can carry
/// a `kind` attribute.
pub(crate) fn fetch_error_value(py: Python<'_>, e: &CoreError) -> PyResult<Py<PyAny>> {
    let kind = match e.kind {
        FetchErrorKind::Invalid => "invalid",
        FetchErrorKind::Timeout => "timeout",
        FetchErrorKind::Connect => "connect",
        FetchErrorKind::Tls => "tls",
        FetchErrorKind::TooManyRedirects => "too_many_redirects",
        FetchErrorKind::Body => "body",
        FetchErrorKind::TooLarge => "too_large",
        FetchErrorKind::Other => "other",
    };
    let class = py.import("netweir._errors")?.getattr("FetchError")?;
    Ok(class.call1((e.message.clone(), kind))?.unbind())
}

/// Raises `netweir.FetchError(message, kind)`.
fn fetch_error(e: CoreError) -> PyErr {
    // Called on Rust threads too, so counted for the atexit hook (aio.rs).
    crate::aio::attach(|py| match fetch_error_value(py, &e) {
        Ok(err) => PyErr::from_value(err.into_bound(py)),
        Err(err) => err,
    })
}

/// Fetch options from the arguments Fetcher and Crawler share.
/// `ca_certificates` is PEM, trusted in place of the bundled roots.
pub(crate) fn fetch_options(
    profile: &str,
    proxy: Option<String>,
    timeout: f64,
    verify: bool,
    ca_certificates: Option<Vec<u8>>,
    max_redirects: u32,
) -> PyResult<FetchOptions> {
    let profile = Profile::named(profile).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let mut options = FetchOptions::new(profile);
    options.proxy = proxy;
    options.timeout = seconds(timeout)?;
    options.verify_certificates = verify;
    options.ca_certificates = ca_certificates;
    options.max_redirects = max_redirects;
    Ok(options)
}

/// A timeout from Python's seconds.
fn seconds(timeout: f64) -> PyResult<Duration> {
    Duration::try_from_secs_f64(timeout)
        .ok()
        .filter(|t| !t.is_zero())
        .ok_or_else(|| PyValueError::new_err("timeout must be a positive number of seconds"))
}

/// A login from Python: (user, password), or (user, password, domain).
pub(crate) type AuthPy = (String, String, Option<String>);

/// A request's policy from the arguments `send` and the crawl's `submit`
/// share.
pub(crate) fn policy(
    timeout: Option<f64>,
    max_redirects: Option<u32>,
    dont_redirect: bool,
    auth: Option<AuthPy>,
) -> PyResult<Policy> {
    let auth = match auth {
        Some((user, password, domain)) => {
            let domain = domain.as_deref().map(_auth_domain).transpose()?;
            Some(Auth::new(user, password, domain))
        }
        None => None,
    };
    Ok(Policy {
        timeout: timeout.map(seconds).transpose()?,
        max_redirects,
        dont_redirect,
        auth,
    })
}

/// A login's domain as netweir_core's `auth_domain` checks it: the bare
/// host it goes to, or ValueError.
#[pyfunction]
pub(crate) fn _auth_domain(domain: &str) -> PyResult<String> {
    netweir_core::auth_domain(domain).map_err(PyValueError::new_err)
}

/// A policy as Python gets it back: (timeout, max_redirects,
/// dont_redirect, (user, password, domain) or None).
pub(crate) fn policy_py(p: Policy) -> (Option<f64>, Option<u32>, bool, Option<AuthPy>) {
    (
        p.timeout.map(|t| t.as_secs_f64()),
        p.max_redirects,
        p.dont_redirect,
        p.auth.map(|a| (a.user, a.password, a.domain)),
    )
}

/// A connection pool and cookie jar that sends every request as one
/// browser profile.
#[pyclass(frozen, module = "netweir")]
pub struct Fetcher {
    inner: CoreFetcher,
}

#[pymethods]
impl Fetcher {
    /// `ca_certificates` (PEM) are trusted in place of the bundled roots;
    /// `max_redirects` is how many redirects a request follows before it
    /// fails.
    #[new]
    #[pyo3(signature = (profile="chrome", proxy=None, timeout=30.0, verify=true, max_size=None, cookies=true, system_proxy=true, ca_certificates=None, max_redirects=netweir_core::MAX_REDIRECTS))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        profile: &str,
        proxy: Option<String>,
        timeout: f64,
        verify: bool,
        max_size: Option<u64>,
        cookies: bool,
        system_proxy: bool,
        ca_certificates: Option<Vec<u8>>,
        max_redirects: u32,
    ) -> PyResult<Fetcher> {
        let mut options = fetch_options(
            profile,
            proxy,
            timeout,
            verify,
            ca_certificates,
            max_redirects,
        )?;
        options.max_body = max_size;
        options.cookies = cookies;
        options.system_proxy = system_proxy;
        // Everything that can fail here is a bad argument (profile, proxy).
        let inner = CoreFetcher::new(options).map_err(|e| PyValueError::new_err(e.message))?;
        Ok(Fetcher { inner })
    }

    /// Every cookie in the jar, oldest first, as dicts.
    fn cookies(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::cookies::cookie_list(py, &self.inner.jar().all())
    }

    /// The cookies named `name`, of `domain` (a host's own and the
    /// domain's; with a leading dot only the domain's) and `path` if given.
    #[pyo3(signature = (name, domain=None, path=None))]
    fn find_cookies(
        &self,
        py: Python<'_>,
        name: &str,
        domain: Option<&str>,
        path: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        crate::cookies::cookie_list(py, &self.inner.jar().find(name, domain, path))
    }

    /// Adds cookie dicts, as `cookies()` gives them, or a browser does:
    /// all of them, or, if one won't go in, none.
    fn add_cookies(&self, cookies: Vec<Bound<'_, PyAny>>) -> PyResult<()> {
        let cookies = cookies
            .iter()
            .map(|c| crate::cookies::cookie_from(c, false))
            .collect::<PyResult<Vec<_>>>()?;
        for c in &cookies {
            netweir_core::cookies::Jar::check(c, None).map_err(PyValueError::new_err)?;
        }
        for c in &cookies {
            self.inner
                .jar()
                .add(c, None)
                .map_err(PyValueError::new_err)?;
        }
        Ok(())
    }

    /// Removes the cookies `find_cookies` would find; how many.
    #[pyo3(signature = (name, domain=None, path=None))]
    fn remove_cookies(&self, name: &str, domain: Option<&str>, path: Option<&str>) -> usize {
        self.inner.jar().remove(name, domain, path)
    }

    /// Removes every cookie, or those of `domain` and its subdomains; how
    /// many.
    #[pyo3(signature = (domain=None))]
    fn clear_cookies(&self, domain: Option<&str>) -> usize {
        self.inner.jar().clear(domain)
    }

    /// The jar as a storage state dict (Playwright's form, no origins);
    /// with `path`, also written there, readable by its owner alone.
    #[pyo3(signature = (path=None))]
    fn storage_state(
        &self,
        py: Python<'_>,
        path: Option<std::path::PathBuf>,
    ) -> PyResult<Py<PyAny>> {
        let state = netweir_browser::StorageState {
            cookies: self.inner.jar().all(),
            origins: Vec::new(),
        };
        crate::cookies::storage_state_out(py, &state, path)
    }

    /// Adds a storage state's cookies: a path to its file, or its dict.
    /// One that won't go in is left out, and what's wrong with it given
    /// back, so the rest of a saved login still loads.
    fn load_storage_state(&self, source: Bound<'_, PyAny>) -> PyResult<Vec<String>> {
        let state = crate::cookies::storage_state_from(&source)?;
        Ok(state
            .cookies
            .iter()
            .filter_map(|c| self.inner.jar().add(c, None).err())
            .collect())
    }

    /// Awaitable GET. Resolves to a Response.
    #[pyo3(signature = (url, headers=None))]
    fn get<'py>(
        &self,
        py: Python<'py>,
        url: String,
        headers: Option<Vec<(String, String)>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fetcher = self.inner.clone();
        let headers = headers.unwrap_or_default();
        crate::aio::future_into_py(py, async move {
            let response = fetcher.get(&url, &headers).await.map_err(fetch_error)?;
            Ok(Response::from(response))
        })
    }

    /// Awaitable request of any kind. Resolves to a Response. With
    /// `follow=False` a redirect isn't followed: the 3xx is the response.
    /// `chain` is then the redirects that led here, each (status, the URL
    /// it came from), and the request is sent as the browser would send it
    /// at the end of them: the method, the page it comes from and the
    /// header order as the redirects left them. `timeout` (seconds) and
    /// `max_redirects` replace the fetcher's for this request; `auth` is
    /// (user, password) or (user, password, domain), sent as HTTP Basic
    /// (see netweir_core's `Auth`).
    #[pyo3(signature = (url, method="GET", kind="navigate", body=None, content_type=None, referer=None, headers=None, follow=true, chain=None, timeout=None, max_redirects=None, auth=None))]
    #[allow(clippy::too_many_arguments)]
    fn send<'py>(
        &self,
        py: Python<'py>,
        url: String,
        method: &str,
        kind: &str,
        body: Option<Vec<u8>>,
        content_type: Option<String>,
        referer: Option<String>,
        headers: Option<Vec<(String, String)>>,
        follow: bool,
        chain: Option<Vec<(u16, String)>>,
        timeout: Option<f64>,
        max_redirects: Option<u32>,
        auth: Option<AuthPy>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fetcher = self.inner.clone();
        let mut outgoing = outgoing(method, kind, body, content_type, referer)?;
        outgoing.policy = policy(timeout, max_redirects, false, auth)?;
        let headers = headers.unwrap_or_default();
        let chain = chain.unwrap_or_default();
        if !chain.is_empty() {
            outgoing = outgoing
                .after_redirects(&chain, &url)
                .map_err(fetch_error)?;
        }
        crate::aio::future_into_py(py, async move {
            let response = if follow {
                fetcher.send(&url, &outgoing, &headers).await
            } else {
                fetcher.once(&url, &outgoing, &headers, chain.len()).await
            };
            Ok(Response::from(response.map_err(fetch_error)?))
        })
    }

    /// `send` that blocks the calling thread (with the GIL released).
    /// With `follow=False` a redirect is the response.
    #[pyo3(signature = (url, method="GET", kind="navigate", body=None, content_type=None, referer=None, headers=None, follow=true, timeout=None, max_redirects=None, auth=None))]
    #[allow(clippy::too_many_arguments)]
    fn send_blocking(
        &self,
        py: Python<'_>,
        url: String,
        method: &str,
        kind: &str,
        body: Option<Vec<u8>>,
        content_type: Option<String>,
        referer: Option<String>,
        headers: Option<Vec<(String, String)>>,
        follow: bool,
        timeout: Option<f64>,
        max_redirects: Option<u32>,
        auth: Option<AuthPy>,
    ) -> PyResult<Response> {
        let mut outgoing = outgoing(method, kind, body, content_type, referer)?;
        outgoing.policy = policy(timeout, max_redirects, !follow, auth)?;
        let headers = headers.unwrap_or_default();
        wait(py, self.inner.send(&url, &outgoing, &headers))?
            .map(Response::from)
            .map_err(fetch_error)
    }

    /// GET that blocks the calling thread (with the GIL released).
    #[pyo3(signature = (url, headers=None))]
    fn get_blocking(
        &self,
        py: Python<'_>,
        url: String,
        headers: Option<Vec<(String, String)>>,
    ) -> PyResult<Response> {
        let headers = headers.unwrap_or_default();
        wait(py, self.inner.get(&url, &headers))?
            .map(Response::from)
            .map_err(fetch_error)
    }
}

/// Runs `future` to completion with the GIL released, stopping every tenth
/// of a second to let Python handle signals. Without the stops, Ctrl-C did
/// nothing until the server answered, and a request that never got an
/// answer couldn't be interrupted at all. A signal handler that raises
/// (KeyboardInterrupt) drops the future, which cancels the request.
fn wait<F>(py: Python<'_>, future: F) -> PyResult<F::Output>
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    let runtime = pyo3_async_runtimes::tokio::get_runtime();
    let mut future = std::pin::pin!(future);
    loop {
        let slice = std::time::Duration::from_millis(100);
        let done = py.detach(|| {
            runtime.block_on(async { tokio::time::timeout(slice, future.as_mut()).await.ok() })
        });
        if let Some(output) = done {
            return Ok(output);
        }
        py.check_signals()?;
    }
}

/// The request `send` describes: `kind` is "navigate" (with a referer,
/// following a link), "form" or "fetch".
fn outgoing(
    method: &str,
    kind: &str,
    body: Option<Vec<u8>>,
    content_type: Option<String>,
    referer: Option<String>,
) -> PyResult<netweir_core::Outgoing> {
    use netweir_core::Outgoing;
    Ok(match kind {
        "navigate" if method.eq_ignore_ascii_case("GET") && body.is_none() => match referer {
            Some(referer) => Outgoing::link(referer),
            None => Outgoing::navigate(),
        },
        "navigate" => {
            return Err(PyValueError::new_err(
                "a navigation is a GET without a body; send a form or a script's request",
            ));
        }
        "form" => Outgoing::form(body.unwrap_or_default(), content_type, referer),
        "fetch" => Outgoing::fetch(method, body, content_type, referer),
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown kind of request {other:?}"
            )));
        }
    })
}

/// A response from its parts, as a worker process gets one, or as the
/// shell makes one for a page read from a file.
#[pyfunction]
pub(crate) fn _response(
    url: String,
    status: u16,
    version: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Response {
    // Only these are ever made; anything else reads as HTTP/1.1.
    let version = [
        "HTTP/0.9", "HTTP/1.0", "HTTP/1.1", "HTTP/2", "HTTP/3", "browser", "file",
    ]
    .into_iter()
    .find(|v| *v == version)
    .unwrap_or("HTTP/1.1");
    Response::from(netweir_core::Response {
        url,
        status,
        version,
        headers,
        body: body.into(),
    })
}

#[pyclass(frozen, module = "netweir")]
pub struct Response {
    inner: Arc<netweir_core::Response>,
}

impl Response {
    /// The response itself, for the crawl's rules.
    pub(crate) fn core(&self) -> &netweir_core::Response {
        &self.inner
    }
}

impl From<netweir_core::Response> for Response {
    fn from(r: netweir_core::Response) -> Response {
        Response { inner: Arc::new(r) }
    }
}

#[pymethods]
impl Response {
    /// Pickled as its parts, so a page can go to a worker process.
    fn __reduce__<'py>(slf: &Bound<'py, Self>) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
        let py = slf.py();
        let r = &slf.get().inner;
        let rebuild = py.import("netweir._native")?.getattr("_response")?;
        let parts = (
            r.url.clone(),
            r.status,
            r.version,
            r.headers.clone(),
            PyBytes::new(py, &r.body),
        )
            .into_pyobject(py)?
            .into_any();
        Ok((rebuild, parts))
    }

    #[getter]
    fn url(&self) -> &str {
        &self.inner.url
    }

    #[getter]
    fn status(&self) -> u16 {
        self.inner.status
    }

    #[getter]
    fn version(&self) -> &'static str {
        self.inner.version
    }

    /// (name, value) pairs in the order received; names are lowercase.
    #[getter]
    fn headers(&self) -> Vec<(String, String)> {
        self.inner.headers.clone()
    }

    #[getter]
    fn body<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.body)
    }

    /// What the response means: (outcome, vendor, kind, retry_after,
    /// price). outcome is "ok", "blocked", "throttled", "payment_required"
    /// or "http_error"; vendor and kind are set for a block, retry_after
    /// (seconds) for throttling and price for a 402.
    #[allow(clippy::type_complexity)]
    fn classify(
        &self,
    ) -> (
        &'static str,
        Option<String>,
        Option<&'static str>,
        Option<f64>,
        Option<String>,
    ) {
        let outcome = classify(&self.inner);
        let name = outcome.name();
        match outcome {
            Outcome::Blocked { vendor, kind } => {
                (name, Some(vendor), Some(kind.as_str()), None, None)
            }
            Outcome::Throttled { retry_after } => {
                (name, None, None, retry_after.map(|d| d.as_secs_f64()), None)
            }
            Outcome::PaymentRequired { price } => (name, None, None, None, price),
            Outcome::Ok | Outcome::HttpError(_) => (name, None, None, None, None),
        }
    }

    /// The body decoded the way a browser would pick the encoding.
    fn text(&self, py: Python<'_>) -> String {
        let r = self.inner.clone();
        py.detach(move || r.text())
    }

    /// Decodes and parses the body; returns the document's root node.
    #[pyo3(signature = (timeout=None))]
    fn parse(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Node> {
        let r = self.inner.clone();
        let budget = crate::budget(timeout)?;
        let doc = py
            .detach(move || Document::parse_within(&r.text(), budget))
            .map_err(|e| crate::ParseTimeout::new_err(e.to_string()))?;
        Ok(Node::root_of(Arc::new(doc)))
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Fetcher>()?;
    m.add_class::<Response>()?;
    m.add_function(wrap_pyfunction!(_response, m)?)?;
    m.add_function(wrap_pyfunction!(_auth_domain, m)?)?;
    Ok(())
}
