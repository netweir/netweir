//! Records the TLS and HTTP/2 fingerprint of incoming connections.
//!
//! netweir's tests point the client at [`Server`] and compare what arrives
//! with a fingerprint captured from the real browser by the `capture`
//! binary.

#![forbid(unsafe_code)]

mod client_hello;
mod compare;
mod h2;
mod ja4;
mod server;

pub use client_hello::{ClientHello, is_grease};
pub use compare::{differences, extension_order_differs};
pub use h2::Http2;
pub use ja4::ja4;
pub use server::{Capture, Server};
