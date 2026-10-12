//! Drives Chrome over the DevTools protocol (CDP), through a pipe.
//!
//! The rules that keep it hard to detect are in docs/design/browser.md:
//! no `Runtime.enable`, `Console.enable` or `Log.enable`, netweir's own
//! scripts only in an isolated world, real input events, and a user agent
//! without the headless marker.

mod browser;
mod conn;
mod launch;
mod network;
mod page;

pub use browser::{Brands, Browser, Context, Guard, LaunchOptions};
pub use launch::{cft_executable, cft_platform, find_chrome, netweir_home};
pub use network::{Continue, NetworkEvent, Request, Route, glob_match, resource_types};
pub use page::{Cookie, Page, Response, WaitUntil};

use std::fmt;

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
    /// Navigation failed before a response (DNS, refused, TLS).
    Navigation(String),
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
            Error::Launch(m)
            | Error::Navigation(m)
            | Error::Script(m)
            | Error::Invalid(m)
            | Error::Gone(m) => f.write_str(m),
            Error::Timeout(m) => write!(f, "timed out: {m}"),
            Error::Closed => f.write_str("the browser has closed"),
            Error::PageClosed => f.write_str("the page has closed"),
            Error::Protocol { method, message } => write!(f, "{method}: {message}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
