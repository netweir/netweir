//! Records the TLS, HTTP/2, QUIC and HTTP/3 fingerprint of incoming
//! connections.
//!
//! netweir's tests point the client at [`Server`] and compare what arrives
//! with a fingerprint captured from the real browser by the `capture`
//! binary.

#![forbid(unsafe_code)]

mod client_hello;
mod compare;
mod h2;
mod h3;
mod ja4;
mod qpack;
mod quic;
mod server;

pub use client_hello::{ClientHello, is_grease};
pub use compare::{
    differences, extension_order_differs, http3_differences, qpack_capacity, qpack_differences,
    qpack_inserts, qpack_shape, quic_differences, transport_parameter_order_differs,
};
pub use h2::Http2;
pub use h3::{Http3, Setting, is_h3_grease};
pub use ja4::{ja4, ja4_quic};
pub use qpack::{EncoderInstruction, FieldLine, FieldSection, QpackOffer};
pub use quic::{Datagram, Frame, Packet, Quic, TransportParameter, first_flight};
pub use server::{Capture, Server};
