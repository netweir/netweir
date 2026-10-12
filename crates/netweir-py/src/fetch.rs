//! Fetching from Python: `Fetcher.get` is awaitable, `Fetcher.get_blocking`
//! is for scripts. Both run on one shared tokio runtime with the GIL
//! released.

use std::sync::Arc;
use std::time::Duration;

use netweir_core::classify::{Outcome, classify};
use netweir_core::{
    FetchError as CoreError, FetchErrorKind, FetchOptions, Fetcher as CoreFetcher, Profile,
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
    Python::attach(|py| match fetch_error_value(py, &e) {
        Ok(err) => PyErr::from_value(err.into_bound(py)),
        Err(err) => err,
    })
}

/// Fetch options from the arguments Fetcher and Crawler share.
pub(crate) fn fetch_options(
    profile: &str,
    proxy: Option<String>,
    timeout: f64,
    verify: bool,
) -> PyResult<FetchOptions> {
    let profile = Profile::named(profile).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let timeout = Duration::try_from_secs_f64(timeout)
        .ok()
        .filter(|t| !t.is_zero())
        .ok_or_else(|| PyValueError::new_err("timeout must be a positive number of seconds"))?;
    let mut options = FetchOptions::new(profile);
    options.proxy = proxy;
    options.timeout = timeout;
    options.verify_certificates = verify;
    Ok(options)
}

/// A connection pool and cookie jar that sends every request as one
/// browser profile.
#[pyclass(frozen, module = "netweir")]
pub struct Fetcher {
    inner: CoreFetcher,
}

#[pymethods]
impl Fetcher {
    #[new]
    #[pyo3(signature = (profile="chrome", proxy=None, timeout=30.0, verify=true, max_size=None, cookies=true, system_proxy=true))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        profile: &str,
        proxy: Option<String>,
        timeout: f64,
        verify: bool,
        max_size: Option<u64>,
        cookies: bool,
        system_proxy: bool,
    ) -> PyResult<Fetcher> {
        let mut options = fetch_options(profile, proxy, timeout, verify)?;
        options.max_body = max_size;
        options.cookies = cookies;
        options.system_proxy = system_proxy;
        // Everything that can fail here is a bad argument (profile, proxy).
        let inner = CoreFetcher::new(options).map_err(|e| PyValueError::new_err(e.message))?;
        Ok(Fetcher { inner })
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
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = fetcher.get(&url, &headers).await.map_err(fetch_error)?;
            Ok(Response::from(response))
        })
    }

    /// Awaitable request of any kind. Resolves to a Response. With
    /// `follow=False` a redirect isn't followed: the 3xx is the response.
    /// `chain` is then the redirects that led here, each (status, the URL
    /// it came from), and the request is sent as the browser would send it
    /// at the end of them: the method, the page it comes from and the
    /// header order as the redirects left them.
    #[pyo3(signature = (url, method="GET", kind="navigate", body=None, content_type=None, referer=None, headers=None, follow=true, chain=None))]
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
    ) -> PyResult<Bound<'py, PyAny>> {
        let fetcher = self.inner.clone();
        let mut outgoing = outgoing(method, kind, body, content_type, referer)?;
        let headers = headers.unwrap_or_default();
        let chain = chain.unwrap_or_default();
        if !chain.is_empty() {
            outgoing = outgoing
                .after_redirects(&chain, &url)
                .map_err(fetch_error)?;
        }
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = if follow {
                fetcher.send(&url, &outgoing, &headers).await
            } else {
                fetcher.once(&url, &outgoing, &headers, chain.len()).await
            };
            Ok(Response::from(response.map_err(fetch_error)?))
        })
    }

    /// `send` that blocks the calling thread (with the GIL released).
    #[pyo3(signature = (url, method="GET", kind="navigate", body=None, content_type=None, referer=None, headers=None))]
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
    ) -> PyResult<Response> {
        let outgoing = outgoing(method, kind, body, content_type, referer)?;
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
    Ok(())
}
