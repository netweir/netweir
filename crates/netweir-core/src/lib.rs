//! netweir's fetching engine.
//!
//! A [`Fetcher`] sends requests that are byte-for-byte what its
//! [`Profile`]'s browser sends: the same TLS ClientHello, HTTP/2 settings
//! and header order.

// Unsafe code only with the http3 feature: in src/h3.rs, which sets NSS
// up as Firefox does over its C API, and src/https_rr.rs, which asks the
// system's resolver for HTTPS records as Firefox does. src/h3_chrome.rs has
// none.
#![cfg_attr(not(feature = "http3"), forbid(unsafe_code))]
#![cfg_attr(feature = "http3", deny(unsafe_code))]

mod archive;
mod cache;
pub mod canonical;
pub mod checkpoint;
pub mod classify;
pub mod cookies;
mod crawl;
mod decode;
pub mod deltafetch;
mod fetch;
#[cfg(feature = "http3")]
mod h3;
#[cfg(feature = "http3")]
mod h3_chrome;
#[cfg(feature = "http3")]
mod https_rr;
pub mod install;
mod profile;
pub mod queue;
pub mod robots;
pub mod sitemap;
pub mod tdmrep;
pub mod tracks;
pub mod traps;

pub use archive::Archive;
pub use crawl::{
    BrowserMode, CrawlRequest, CrawlSettings, Crawler, Downloaded, DropReason, Event, LivePage,
    ProxyRotation, ProxyStats, RETRY_STATUSES, RequestCookies, RequestSteps, Stats, StepCall,
    Submitted, allowed_domains, offsite,
};
pub use decode::{decode, encode, encoding_name, meta_content};
pub use deltafetch::DeltaKey;
pub use fetch::{
    Auth, FetchError, FetchErrorKind, FetchOptions, Fetcher, Hop, Kind, MAX_REDIRECTS, Outgoing,
    Policy, Response, auth_domain, check_proxy, masked,
};
#[cfg(feature = "http3")]
#[doc(hidden)]
pub use https_rr::https_answer;
pub use profile::{
    DEFAULT as DEFAULT_PROFILE, Profile, ProfileError, built_in as built_in_profiles,
};
