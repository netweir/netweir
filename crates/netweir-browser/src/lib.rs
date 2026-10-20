//! Drives Chrome over the DevTools protocol (CDP), through a pipe.
//!
//! The rules that keep it hard to detect are in docs/design/browser.md:
//! no `Runtime.enable`, `Console.enable` or `Log.enable`, netweir's own
//! scripts only in an isolated world, real input events, and a user agent
//! without the headless marker.

mod browser;
mod conn;
mod environment;
mod events;
mod frame;
mod launch;
mod network;
mod page;
pub mod steps;
mod storage;
mod window;

pub use browser::{Brands, Browser, Context, Guard, LaunchOptions};
pub use environment::{
    ColorScheme, Contrast, Environment, ForcedColors, Geolocation, Media, MediaChange, Phone,
    ReducedMotion, Size, device_names,
};
pub use events::{Dialog, Download, PageEvent};
pub use frame::{Frame, Pick};
pub use launch::{
    cft_executable, cft_platform, find_chrome, grant_sandbox_access, netweir_home,
    sandbox_grant_args,
};
pub use network::{Continue, Request, Route, glob_match, resource_types};
pub use page::{Cookie, Page, Response, WaitUntil};
pub use storage::{OriginStorage, StorageState, cookie_of, write_private};

use std::fmt;
use std::sync::OnceLock;

type Warn = Box<dyn Fn(&str) + Send + Sync>;

static WARN: OnceLock<Warn> = OnceLock::new();

/// Where netweir's warnings go: things that went wrong, which it carried
/// on past rather than fail or hang over. Standard error until this is
/// called; only the first call counts.
pub fn on_warning(f: impl Fn(&str) + Send + Sync + 'static) {
    let _ = WARN.set(Box::new(f));
}

pub(crate) fn warn(message: &str) {
    match WARN.get() {
        Some(f) => f(message),
        None => eprintln!("netweir: {message}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Chrome couldn't be found or started.
    Launch(String),
    /// The browser has closed.
    Closed,
    /// The page has closed.
    PageClosed,
    /// Chrome rejected a command.
    Protocol { method: String, message: String },
    /// A wait ran out of time; says what was being waited for.
    Timeout(String),
    /// Scrolling to the bottom ran out of time with the page still
    /// growing; `rounds` of more had loaded by then, and are in the page.
    ScrollTimeout { message: String, rounds: u32 },
    /// Navigation failed before a response (DNS, refused, TLS): the URL,
    /// Chrome's net error (`net::ERR_...`), and what Chrome had lately
    /// written to stderr, if anything, which can say why.
    Navigation {
        url: String,
        error: String,
        said: Option<String>,
    },
    /// Script in the page threw.
    Script(String),
    /// A bad argument, such as an unknown wait state.
    Invalid(String),
    /// What was asked for is no longer there, such as the body of a
    /// response from before the page navigated.
    Gone(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Launch(m) | Error::Script(m) | Error::Invalid(m) | Error::Gone(m) => {
                f.write_str(m)
            }
            Error::Navigation { url, error, said } => {
                write!(f, "{url}: {error}")?;
                match said {
                    Some(said) => write!(f, "; Chrome said lately:\n{said}"),
                    None => Ok(()),
                }
            }
            Error::Timeout(m) | Error::ScrollTimeout { message: m, .. } => {
                write!(f, "timed out: {m}")
            }
            Error::Closed => f.write_str("the browser has closed"),
            Error::PageClosed => f.write_str("the page has closed"),
            Error::Protocol { method, message } => write!(f, "{method}: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// What kind of failure a navigation was, by Chrome's net error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetFailure {
    /// It ran out of time.
    Timeout,
    /// TLS failed: a certificate, or the handshake.
    Tls,
    /// The connection never happened: refused, no address, no network,
    /// the proxy.
    Connect,
    /// Anything else, such as ERR_ABORTED for a 204 or a download, which
    /// reached the server.
    Other,
}

impl Error {
    /// For a navigation that failed, what kind of failure it was, read off
    /// the net error alone: never the URL, nor what Chrome wrote to
    /// stderr, either of which could hold any of these words.
    pub fn net_failure(&self) -> Option<NetFailure> {
        let Error::Navigation { error, .. } = self else {
            return None;
        };
        let has = |names: &[&str]| names.iter().any(|n| error.contains(n));
        Some(if has(&["TIMED_OUT"]) {
            NetFailure::Timeout
        } else if has(&["CERT", "SSL"]) {
            NetFailure::Tls
        } else if has(&[
            "CONNECTION",
            // WSAENOBUFS from connect(): a local port it couldn't have.
            "NO_BUFFER_SPACE",
            "NAME_NOT_RESOLVED",
            "ADDRESS",
            "INTERNET_DISCONNECTED",
            "PROXY",
        ]) {
            NetFailure::Connect
        } else {
            NetFailure::Other
        })
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Whether a navigation that failed with Chrome's net error `error` is
/// asked again at once. Only ERR_NO_BUFFER_SPACE: on Windows 11 22H2 and
/// later Chrome sets SO_RANDOMIZE_PORT on each connection, and a random
/// port still in TIME_WAIT fails the connect with WSAENOBUFS before
/// anything is sent. The next attempt gets another port.
pub(crate) fn retry_navigation(error: &str) -> bool {
    error == "net::ERR_NO_BUFFER_SPACE"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(url: &str, error: &str, said: Option<&str>) -> Error {
        Error::Navigation {
            url: url.into(),
            error: error.into(),
            said: said.map(str::to_string),
        }
    }

    #[test]
    fn only_the_net_error_says_what_kind_of_failure_it_was() {
        assert_eq!(
            failed("http://a/", "net::ERR_TIMED_OUT", None).net_failure(),
            Some(NetFailure::Timeout)
        );
        assert_eq!(
            failed("http://a/", "net::ERR_CERT_DATE_INVALID", None).net_failure(),
            Some(NetFailure::Tls)
        );
        assert_eq!(
            failed("http://a/", "net::ERR_CONNECTION_REFUSED", None).net_failure(),
            Some(NetFailure::Connect)
        );
        // Neither the URL nor what Chrome wrote to stderr changes it.
        let aborted = failed(
            "https://ssl.example/certificates",
            "net::ERR_ABORTED",
            Some("ERROR: net::ERR_TIMED_OUT while fetching updates; SSL handshake failed"),
        );
        assert_eq!(aborted.net_failure(), Some(NetFailure::Other));
        let text = aborted.to_string();
        assert!(
            text.starts_with(
                "https://ssl.example/certificates: net::ERR_ABORTED; Chrome said lately:"
            ),
            "{text}"
        );
        assert!(text.contains("SSL handshake failed"), "{text}");
        assert_eq!(Error::Closed.net_failure(), None);
        // The connection never started.
        assert_eq!(
            failed("http://a/", "net::ERR_NO_BUFFER_SPACE", None).net_failure(),
            Some(NetFailure::Connect)
        );
    }

    #[test]
    fn only_a_port_windows_wouldnt_give_is_asked_again() {
        assert!(retry_navigation("net::ERR_NO_BUFFER_SPACE"));
        // These reached the far end, or will fail the same way again.
        for error in [
            "net::ERR_CONNECTION_REFUSED",
            "net::ERR_CONNECTION_RESET",
            "net::ERR_NAME_NOT_RESOLVED",
            "net::ERR_TIMED_OUT",
            "net::ERR_ABORTED",
            "net::ERR_BLOCKED_BY_CLIENT",
        ] {
            assert!(!retry_navigation(error), "{error}");
        }
    }
}
