//! netweir's fetching engine.
//!
//! A [`Fetcher`] sends requests that are byte-for-byte what its
//! [`Profile`]'s browser sends: the same TLS ClientHello, HTTP/2 settings
//! and header order.

mod archive;
mod cache;
pub mod canonical;
pub mod checkpoint;
pub mod classify;
mod crawl;
mod decode;
pub mod deltafetch;
mod fetch;
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
    ProxyRotation, ProxyStats, Stats, Submitted, allowed_domains, offsite,
};
pub use decode::{decode, meta_content};
pub use deltafetch::DeltaKey;
pub use fetch::{
    FetchError, FetchErrorKind, FetchOptions, Fetcher, Hop, Kind, Outgoing, Response, check_proxy,
    masked,
};
pub use profile::{DEFAULT as DEFAULT_PROFILE, Profile, ProfileError};
