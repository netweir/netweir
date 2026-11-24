//! Requests over HTTP/3 the way Chrome 157 sends them: through netweir's
//! fork of Cloudflare's quiche (github.com/netweir/quiche, branch
//! chrome-157), linked against btls, the patched BoringSSL the TCP
//! connections use. The ClientHello inside the QUIC Initial comes from
//! that BoringSSL, set up from the profile's `[tls]` as Chromium's QUIC
//! sets up its own (quiche/quic/core/tls_client_handshaker.cc), and the
//! fork adds what quiche didn't do as Chrome does: the transport
//! parameters, shuffled with a GREASE one on each connection; the
//! connection IDs; the Initial packets, their CRYPTO frames split and
//! mixed with PING and PADDING (QUICHE's chaos protection); HTTP/3
//! SETTINGS with GREASE; and a QPACK decoder that takes the dynamic table
//! Chrome offers servers.
//!
//! What comes after the first round trips (loss recovery, congestion
//! control, pacing, ACK timing) is quiche's, not Chrome's.
//!
//! Safari's profiles use the same engine (the fork's `safari-27`
//! branch) with Safari's settings, as recorded from Safari 27.2 on macOS
//! (profiles/safari-27-macos.h3.capture.json). Safari's own QUIC is
//! Apple's Network.framework, so this is the closest match, not the same
//! code: see [`Browser::Safari`].
//!
//! Each connection runs on a thread of its own, as the Firefox ones do,
//! and takes requests over a channel.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use btls::ssl::{
    CertificateCompressionAlgorithm, CertificateCompressor, KeyShare, SslContextBuilder, SslMethod,
    SslVerifyMode,
};
use bytes::Bytes;
use quiche::h3::NameValue;
use tokio::sync::{mpsc, oneshot, watch};

use crate::fetch::{FetchError, FetchErrorKind};
use crate::h3::{Body, Chunk, Handle, Job, State, Trust, fail_all};
use crate::profile::Tls;

/// Chrome 157's QUIC and HTTP/3 settings, as the fingerprint server
/// recorded them (profiles/chrome-157-macos.h3.capture.json) and
/// Chromium sets them (net/quic/quic_context.h, QuicParams; and
/// quic_session_pool.cc).
mod chrome {
    use std::time::Duration;

    /// max_idle_timeout, in milliseconds.
    pub const IDLE_TIMEOUT: u64 = 30_000;
    /// max_udp_payload_size.
    pub const MAX_UDP_PAYLOAD: usize = 1472;
    /// The size of every packet, Initial ones included: QUICHE's
    /// kDefaultMaxPacketSize over IPv4, 20 bytes less over IPv6.
    pub const PACKET_IPV4: usize = 1250;
    pub const PACKET_IPV6: usize = 1230;
    pub const MAX_DATA: u64 = 15_728_640;
    pub const MAX_STREAM_DATA: u64 = 6_291_456;
    pub const MAX_STREAMS_BIDI: u64 = 100;
    pub const MAX_STREAMS_UNI: u64 = 103;
    /// google_connection_options (0x3128): the one QuicTag Chrome sends.
    pub const CONNECTION_OPTIONS: &[u8] = b"ORIG";
    /// initial_round_trip_time_us (0x3127), sent once Chrome has a
    /// smoothed RTT for the server from an earlier connection.
    pub const INITIAL_RTT: u64 = 0x3127;
    /// SETTINGS_QPACK_MAX_TABLE_CAPACITY.
    pub const QPACK_TABLE: u64 = 65_536;
    /// SETTINGS_MAX_FIELD_SECTION_SIZE.
    pub const FIELD_SECTION: u64 = 262_144;
    /// SETTINGS_QPACK_BLOCKED_STREAMS.
    pub const BLOCKED_STREAMS: u64 = 100;
    /// How long a handshake may take: QuicParams::max_time_before_crypto_handshake.
    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    /// H3_REQUEST_CANCELLED.
    pub const REQUEST_CANCELLED: u64 = 0x10c;
    /// H3_NO_ERROR.
    pub const NO_ERROR: u64 = 0x100;
}

/// Safari 27's QUIC and HTTP/3 settings, as the fingerprint server
/// recorded them from Safari 27.2 on macOS 26.7
/// (profiles/safari-27-macos.h3.capture.json). Network.framework's source
/// isn't public, so nothing here could be checked against code.
mod safari {
    /// initial_max_data.
    pub const MAX_DATA: u64 = 16_777_216;
    /// initial_max_stream_data_bidi_local, _bidi_remote and _uni.
    pub const MAX_STREAM_DATA: u64 = 2_097_152;
    /// initial_max_streams_uni; initial_max_streams_bidi isn't sent.
    pub const MAX_STREAMS_UNI: u64 = 8;
    /// active_connection_id_limit.
    pub const CONNECTION_ID_LIMIT: u64 = 64;
    /// SETTINGS_QPACK_MAX_TABLE_CAPACITY.
    pub const QPACK_TABLE: u64 = 16_383;
    /// SETTINGS_QPACK_BLOCKED_STREAMS.
    pub const BLOCKED_STREAMS: u64 = 100;
}

/// Whose QUIC a connection is laid out as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Browser {
    /// Chrome 157, ported from or checked against Google's QUICHE.
    Chrome,
    /// Safari 27, from a recording: the TLS settings of the profile's
    /// `[tls]` with the `[http3]` extension order; QUIC v1, an 8-byte
    /// Destination Connection ID and no Source one; seven transport
    /// parameters ascending from a random one; two 1200-byte Initials,
    /// at most 999 bytes of hello each, PADDING frames after; SETTINGS
    /// 1:16383, 7:100 and GREASE; `:method :scheme :authority :path`; no
    /// PRIORITY_UPDATE.
    Safari,
}

/// Brotli, the one certificate compression Chrome offers.
struct Brotli;

impl CertificateCompressor for Brotli {
    const ALGORITHM: CertificateCompressionAlgorithm = CertificateCompressionAlgorithm::BROTLI;
    const CAN_COMPRESS: bool = false;
    const CAN_DECOMPRESS: bool = true;

    fn decompress<W: io::Write>(&self, input: &[u8], output: &mut W) -> io::Result<()> {
        let mut r = brotli::Decompressor::new(input, 4096);
        std::io::copy(&mut r, output).map(|_| ())
    }
}

/// Zlib, the one certificate compression Safari offers.
struct Zlib;

impl CertificateCompressor for Zlib {
    const ALGORITHM: CertificateCompressionAlgorithm = CertificateCompressionAlgorithm::ZLIB;
    const CAN_COMPRESS: bool = false;
    const CAN_DECOMPRESS: bool = true;

    fn decompress<W: io::Write>(&self, input: &[u8], output: &mut W) -> io::Result<()> {
        let mut r = flate2::read::ZlibDecoder::new(input);
        std::io::copy(&mut r, output).map(|_| ())
    }
}

/// The TLS context for one connection: the profile's `[tls]` as
/// Chromium's QUIC takes it. TLS 1.3 only, which quiche sets, so the
/// ciphers are BoringSSL's three; no GREASE, as QUICHE never turns it on;
/// BoringSSL's own signature algorithms, as QUICHE sets none; the
/// profile's groups, extension shuffling, certificate compression and
/// trust anchors. The certificate is checked after the handshake, against
/// the roots the TCP connections use.
///
/// Safari's QUIC hello was recorded with its TCP one's settings, GREASE,
/// cipher order, signature algorithms, OCSP and SCT requests included,
/// TLS 1.3 only, and the extensions in a fixed order of their own (the
/// profile's `[http3] extension_order`, put in `tls` by the fetcher).
fn tls_context(tls: &Tls, browser: Browser) -> Result<SslContextBuilder, String> {
    let bad = |e: btls::error::ErrorStack| format!("TLS: {e}");
    let mut ctx = SslContextBuilder::new(SslMethod::tls()).map_err(bad)?;
    ctx.set_curves_list(&tls.groups.join(":")).map_err(bad)?;
    ctx.set_permute_extensions(tls.permute_extensions);
    if browser == Browser::Safari {
        // Before the cipher list, or BoringSSL's own TLS 1.3 order stays.
        ctx.set_preserve_tls13_cipher_list(tls.preserve_tls13_cipher_order);
        ctx.set_cipher_list(&tls.ciphers.join(":")).map_err(bad)?;
        ctx.set_sigalgs_list(&tls.signature_algorithms.join(":"))
            .map_err(bad)?;
        ctx.set_grease_enabled(tls.grease);
        if tls.ocsp_stapling {
            ctx.enable_ocsp_stapling();
        }
        if tls.signed_certificate_timestamps {
            ctx.enable_signed_cert_timestamps();
        }
        if !tls.extension_order.is_empty() {
            let order: Vec<btls::ssl::ExtensionType> =
                tls.extension_order.iter().map(|&e| e.into()).collect();
            ctx.set_extension_permutation(&order).map_err(bad)?;
        }
    }
    for c in &tls.certificate_compression {
        match c.as_str() {
            "brotli" => ctx
                .add_certificate_compression_algorithm(Brotli)
                .map_err(bad)?,
            "zlib" => ctx
                .add_certificate_compression_algorithm(Zlib)
                .map_err(bad)?,
            other => return Err(format!("no QUIC certificate compression {other:?}")),
        }
    }
    if let Some(hex) = &tls.trust_anchors {
        let ids = crate::profile::decode_hex(hex).ok_or("bad trust anchor hex")?;
        ctx.set_requested_trust_anchors(&ids).map_err(bad)?;
    }
    ctx.set_verify(SslVerifyMode::NONE);
    Ok(ctx)
}

/// The settings BoringSSL takes per connection, as
/// TlsClientHandshaker::CryptoConnect makes them: ECH GREASE, the key
/// shares, and ALPS for h3 with no settings of its own. Safari sends no
/// ALPS.
fn tls_per_connection(
    conn: &mut quiche::Connection,
    tls: &Tls,
    browser: Browser,
) -> Result<(), String> {
    let bad = |e: btls::error::ErrorStack| format!("TLS: {e}");
    let shares = tls
        .key_shares
        .iter()
        .map(|k| match k.as_str() {
            "X25519MLKEM768" => Ok(KeyShare::X25519_MLKEM768),
            "X25519" => Ok(KeyShare::X25519),
            "P-256" => Ok(KeyShare::P256),
            "P-384" => Ok(KeyShare::P384),
            other => Err(format!("unknown key share {other:?}")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let ssl = conn.ssl_mut();
    ssl.set_enable_ech_grease(tls.ech_grease);
    ssl.set_client_key_shares(&shares).map_err(bad)?;
    if browser == Browser::Chrome {
        ssl.add_application_settings(b"h3").map_err(bad)?;
        ssl.set_alps_use_new_codepoint(tls.alps_new_codepoint);
    }
    Ok(())
}

/// The QUIC side of one connection, as Chrome 157 sets it up, or as
/// Safari 27 was recorded setting it up.
fn quic_config(
    tls: &Tls,
    browser: Browser,
    ipv6: bool,
    rtt: Option<Duration>,
) -> Result<quiche::Config, String> {
    let bad = |e: quiche::Error| format!("QUIC: {e}");
    let mut config = quiche::Config::with_boring_ssl_ctx_builder(
        quiche::PROTOCOL_VERSION,
        tls_context(tls, browser)?,
    )
    .map_err(bad)?;
    config.set_application_protos(&[b"h3"]).map_err(bad)?;
    config.verify_peer(false);
    if browser == Browser::Safari {
        // No max_idle_timeout and no max_udp_payload_size, as Safari
        // sent none: quiche's defaults, 0 and 65527, are left out. The
        // 1200-byte datagrams are quiche's default too.
        config.set_initial_max_data(safari::MAX_DATA);
        config.set_initial_max_stream_data_bidi_local(safari::MAX_STREAM_DATA);
        config.set_initial_max_stream_data_bidi_remote(safari::MAX_STREAM_DATA);
        config.set_initial_max_stream_data_uni(safari::MAX_STREAM_DATA);
        config.set_initial_max_streams_bidi(0);
        config.set_initial_max_streams_uni(safari::MAX_STREAMS_UNI);
        config.set_active_connection_id_limit(safari::CONNECTION_ID_LIMIT);
        config.enable_safari_transport_params(true);
        config.enable_safari_initial(true);
        return Ok(config);
    }
    config.set_max_idle_timeout(chrome::IDLE_TIMEOUT);
    config.set_max_recv_udp_payload_size(chrome::MAX_UDP_PAYLOAD);
    config.set_max_send_udp_payload_size(if ipv6 {
        chrome::PACKET_IPV6
    } else {
        chrome::PACKET_IPV4
    });
    config.set_initial_max_data(chrome::MAX_DATA);
    config.set_initial_max_stream_data_bidi_local(chrome::MAX_STREAM_DATA);
    config.set_initial_max_stream_data_bidi_remote(chrome::MAX_STREAM_DATA);
    config.set_initial_max_stream_data_uni(chrome::MAX_STREAM_DATA);
    config.set_initial_max_streams_bidi(chrome::MAX_STREAMS_BIDI);
    config.set_initial_max_streams_uni(chrome::MAX_STREAMS_UNI);
    // max_datagram_frame_size: quiche's 65536, Chrome's too.
    config.enable_dgram(true, 1024, 1024);
    config.enable_chromium_transport_params(true);
    config.set_version_information(&[quiche::PROTOCOL_VERSION]);
    config.add_custom_transport_param(0x3128, chrome::CONNECTION_OPTIONS);
    if let Some(rtt) = rtt {
        let us = u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX);
        config.add_custom_transport_param(chrome::INITIAL_RTT, &varint(us));
    }
    config.enable_chromium_initial(true);
    Ok(config)
}

/// The HTTP/3 side: Chrome's SETTINGS (QuicSpdySession and
/// QuicChromiumClientSession), its GREASE, and no QPACK streams until
/// they're needed; or Safari's SETTINGS, with no GREASE frame.
fn h3_config(browser: Browser) -> Result<quiche::h3::Config, String> {
    let mut config = quiche::h3::Config::new().map_err(|e| format!("HTTP/3: {e}"))?;
    if browser == Browser::Safari {
        config.set_qpack_max_table_capacity(safari::QPACK_TABLE);
        config.set_qpack_blocked_streams(safari::BLOCKED_STREAMS);
        config.enable_safari(true);
        return Ok(config);
    }
    config.set_qpack_max_table_capacity(chrome::QPACK_TABLE);
    config.set_max_field_section_size(chrome::FIELD_SECTION);
    config.set_qpack_blocked_streams(chrome::BLOCKED_STREAMS);
    config.enable_chromium(true);
    Ok(config)
}

/// `v` as a QUIC variable-length integer.
fn varint(v: u64) -> Vec<u8> {
    match v {
        0..=63 => vec![v as u8],
        64..=16_383 => (0x4000 | v as u16).to_be_bytes().to_vec(),
        16_384..=1_073_741_823 => (0x8000_0000 | v as u32).to_be_bytes().to_vec(),
        _ => (0xc000_0000_0000_0000 | v.min((1 << 62) - 1))
            .to_be_bytes()
            .to_vec(),
    }
}

/// What a connection leaves for the next one to its server: the TLS
/// session to resume, and the smoothed RTT Chrome then sends as
/// initial_rtt (QuicSessionPool keeps it as the server's network stats,
/// HttpServerProperties::SetServerNetworkStats).
#[derive(Clone)]
pub(crate) struct Ticket {
    pub session: Vec<u8>,
    pub rtt: Duration,
}

pub(crate) type Tickets = Arc<Mutex<HashMap<String, Ticket>>>;

/// The urgency and incremental flag of a request's `priority` header,
/// which Chrome also sends in a PRIORITY_UPDATE frame unless they're the
/// defaults (QuicSpdyStream::MaybeSendPriorityUpdateFrame).
fn priority(headers: &[(String, Vec<u8>)]) -> (u8, bool) {
    let Some((_, value)) = headers.iter().find(|(k, _)| k == "priority") else {
        return (3, false);
    };
    let value = String::from_utf8_lossy(value);
    let (mut urgency, mut incremental) = (3, false);
    for item in value.split(',').map(str::trim) {
        if let Some(u) = item.strip_prefix("u=").and_then(|u| u.parse::<u8>().ok())
            && u < 8
        {
            urgency = u;
        } else if item == "i" || item == "i=?1" {
            incremental = true;
        }
    }
    (urgency, incremental)
}

/// Opens a connection to `alternative` for `host` on a thread of its own.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open(
    host: &str,
    alternative: &str,
    given: Option<IpAddr>,
    key: &str,
    tls: Arc<Tls>,
    browser: Browser,
    trust: Trust,
    tickets: &Tickets,
) -> Handle {
    let (jobs, queue) = mpsc::unbounded_channel();
    let (state, watching) = watch::channel(State::Connecting);
    let (host, alternative, key) = (host.to_string(), alternative.to_string(), key.to_string());
    let ticket = tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    let tickets = tickets.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("netweir-h3 {alternative}"))
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    state.send_replace(State::Failed { exclude: false });
                    return fail_all(queue, &format!("{e}"));
                }
            };
            let connection = Connection {
                host,
                alternative,
                given,
                tls,
                browser,
                trust,
                ticket,
                key,
                tickets,
            };
            runtime.block_on(connection.run(queue, &state));
            state.send_modify(|s| {
                *s = match *s {
                    State::Connecting => State::Failed { exclude: true },
                    State::Connected => State::Closed,
                    other => other,
                }
            });
        });
    Handle {
        jobs,
        thread: Arc::new(Mutex::new(spawned.ok())),
        state: watching,
    }
}

struct Connection {
    host: String,
    alternative: String,
    /// The address to use instead of resolving the alternative's host.
    given: Option<IpAddr>,
    tls: Arc<Tls>,
    browser: Browser,
    trust: Trust,
    ticket: Option<Ticket>,
    key: String,
    tickets: Tickets,
}

/// A request in flight, by its stream.
struct Stream {
    head: Option<oneshot::Sender<Result<http::Response<Body>, FetchError>>>,
    deadline: tokio::time::Instant,
    body: Option<mpsc::UnboundedSender<Chunk>>,
    /// Request body quiche hasn't taken yet.
    unsent: Bytes,
    /// Whether the request's end has gone to quiche.
    ended: bool,
}

impl Stream {
    fn end(self, error: FetchError) {
        if let Some(head) = self.head {
            let _ = head.send(Err(error));
        } else if let Some(body) = self.body {
            let _ = body.send(Err(io::Error::other(error.message)));
        }
    }
}

impl Connection {
    fn connect(&self) -> Result<(tokio::net::UdpSocket, SocketAddr, quiche::Connection), String> {
        let peer = match self.given {
            Some(ip) => {
                let port = self
                    .alternative
                    .rsplit_once(':')
                    .and_then(|(_, p)| p.parse::<u16>().ok())
                    .ok_or_else(|| format!("{}: no port", self.alternative))?;
                SocketAddr::new(ip, port)
            }
            None => self
                .alternative
                .to_socket_addrs()
                .map_err(|e| format!("{}: {e}", self.alternative))?
                .next()
                .ok_or_else(|| format!("{}: no address", self.alternative))?,
        };
        let any = match peer {
            SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        };
        let socket = std::net::UdpSocket::bind(any).map_err(|e| e.to_string())?;
        socket.set_nonblocking(true).map_err(|e| e.to_string())?;
        let socket = tokio::net::UdpSocket::from_std(socket).map_err(|e| e.to_string())?;
        let local = socket.local_addr().map_err(|e| e.to_string())?;
        let rtt = self.ticket.as_ref().map(|t| t.rtt);
        let mut config = quic_config(&self.tls, self.browser, peer.is_ipv6(), rtt)?;
        // An 8-byte Destination Connection ID and no Source Connection ID,
        // as Chrome's (QuicSessionPool, kQuicDefaultConnectionIdLength).
        let mut dcid = [0u8; 8];
        btls::rand::rand_bytes(&mut dcid).map_err(|e| e.to_string())?;
        let dcid = quiche::ConnectionId::from_ref(&dcid);
        let scid = quiche::ConnectionId::default();
        let mut conn =
            quiche::connect_with_dcid(Some(&self.host), &scid, &dcid, local, peer, &mut config)
                .map_err(|e| format!("QUIC: {e}"))?;
        tls_per_connection(&mut conn, &self.tls, self.browser)?;
        if let Some(ticket) = &self.ticket {
            // A session BoringSSL won't take leaves a fresh handshake.
            let _ = conn.set_session(&ticket.session);
        }
        Ok((socket, peer, conn))
    }

    /// Leaves this connection's session and RTT for the next one.
    fn leave_ticket(&self, conn: &quiche::Connection) {
        let Some(session) = conn.session() else {
            return;
        };
        let rtt = conn.path_stats().next().map_or(Duration::ZERO, |p| p.rtt);
        self.tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                self.key.clone(),
                Ticket {
                    session: session.to_vec(),
                    rtt,
                },
            );
    }

    async fn run(self, mut queue: mpsc::UnboundedReceiver<Job>, state: &watch::Sender<State>) {
        let (socket, peer, mut conn) = match self.connect() {
            Ok(c) => c,
            Err(e) => {
                state.send_replace(State::Failed { exclude: false });
                return fail_all(queue, &format!("HTTP/3 to {}: {e}", self.alternative));
            }
        };
        let local = socket.local_addr().unwrap_or(peer);
        let h3_config = match h3_config(self.browser) {
            Ok(c) => c,
            Err(e) => {
                state.send_replace(State::Failed { exclude: false });
                return fail_all(queue, &e);
            }
        };
        let mut h3: Option<quiche::h3::Connection> = None;
        let mut streams: HashMap<u64, Stream> = HashMap::new();
        let mut waiting: Vec<Job> = Vec::new();
        let (mut accepting, mut closing) = (true, false);
        let started = Instant::now();
        let mut out = vec![0u8; 65_535];
        let mut buf = vec![0u8; 65_535];
        loop {
            // Up, and its certificate checked.
            if h3.is_none() && conn.is_established() {
                let chain: Vec<Vec<u8>> = conn
                    .peer_cert_chain()
                    .map(|c| c.iter().map(|d| d.to_vec()).collect())
                    .unwrap_or_default();
                if !self.trust.trusts(&self.host, &chain) {
                    let _ = conn.close(false, 0x0128, b"");
                    state.send_replace(State::Failed { exclude: false });
                    let why = format!(
                        "HTTP/3 to {}: the certificate isn't trusted",
                        self.alternative
                    );
                    for job in waiting.drain(..) {
                        let _ = job
                            .head
                            .send(Err(FetchError::new(FetchErrorKind::Tls, why.clone())));
                    }
                    flush(&socket, &mut conn, &mut out).await;
                    return fail_all(queue, &why);
                }
                match quiche::h3::Connection::with_transport(&mut conn, &h3_config) {
                    Ok(c) => {
                        h3 = Some(c);
                        state.send_replace(State::Connected);
                    }
                    Err(e) => {
                        state.send_replace(State::Failed { exclude: true });
                        return fail_all(queue, &format!("HTTP/3: {e}"));
                    }
                }
            }
            if let Some(h3) = h3.as_mut() {
                for job in waiting.drain(..) {
                    start(h3, &mut conn, &mut streams, job, self.browser);
                }
                events(h3, &mut conn, &mut streams, &mut buf);
                // A request whose caller has gone is cancelled.
                streams.retain(|&id, s| {
                    let gone = s.head.as_ref().is_some_and(oneshot::Sender::is_closed)
                        || s.body
                            .as_ref()
                            .is_some_and(mpsc::UnboundedSender::is_closed);
                    if gone {
                        let _ = conn.stream_shutdown(
                            id,
                            quiche::Shutdown::Read,
                            chrome::REQUEST_CANCELLED,
                        );
                        let _ = conn.stream_shutdown(
                            id,
                            quiche::Shutdown::Write,
                            chrome::REQUEST_CANCELLED,
                        );
                    }
                    !gone
                });
                for (&id, s) in streams.iter_mut() {
                    push_body(h3, &mut conn, id, s);
                }
            }
            if !accepting && !closing && streams.is_empty() && waiting.is_empty() {
                closing = true;
                self.leave_ticket(&conn);
                let _ = conn.close(true, chrome::NO_ERROR, b"");
            }
            if conn.is_closed() {
                self.leave_ticket(&conn);
                let ready = h3.is_some();
                let peer_error = conn.peer_error().map(|e| (e.is_app, e.error_code));
                let local_error = conn.local_error().map(|e| (e.is_app, e.error_code));
                let why = format!(
                    "HTTP/3 connection to {} closed: peer {peer_error:?}, local {local_error:?}, timed out {}",
                    self.alternative,
                    conn.is_timed_out()
                );
                let kind = if ready {
                    FetchErrorKind::Body
                } else {
                    FetchErrorKind::Connect
                };
                let errored = |e: Option<(bool, u64)>| {
                    e.is_some_and(|(app, code)| !(code == 0 || app && code == chrome::NO_ERROR))
                };
                let broken = !closing && (errored(peer_error) || errored(local_error));
                state.send_replace(match (ready, broken) {
                    (true, true) => State::Broken,
                    (true, false) => State::Closed,
                    (false, _) => State::Failed { exclude: true },
                });
                for (_, s) in streams.drain() {
                    s.end(FetchError::new(kind, why.clone()));
                }
                for job in waiting.drain(..) {
                    let _ = job.head.send(Err(FetchError::new(kind, why.clone())));
                }
                return fail_all(queue, &why);
            }
            // A handshake that takes too long ends the connection there and
            // then, with no CONNECTION_CLOSE, as Chrome's
            // QUIC_HANDSHAKE_TIMEOUT does.
            if h3.is_none() && started.elapsed() >= chrome::HANDSHAKE_TIMEOUT {
                state.send_replace(State::Failed { exclude: true });
                let why = format!("HTTP/3 to {}: the handshake timed out", self.alternative);
                for job in waiting.drain(..) {
                    let _ = job
                        .head
                        .send(Err(FetchError::new(FetchErrorKind::Connect, why.clone())));
                }
                return fail_all(queue, &why);
            }
            if let Err(e) = send_all(&socket, &mut conn, &mut out).await {
                let why = format!("HTTP/3 to {}: {e}", self.alternative);
                state.send_replace(if h3.is_some() {
                    State::Closed
                } else {
                    State::Failed { exclude: true }
                });
                for (_, s) in streams.drain() {
                    s.end(FetchError::new(FetchErrorKind::Connect, why.clone()));
                }
                for job in waiting.drain(..) {
                    let _ = job
                        .head
                        .send(Err(FetchError::new(FetchErrorKind::Connect, why.clone())));
                }
                return fail_all(queue, &why);
            }
            let mut timeout = conn.timeout().unwrap_or(Duration::from_secs(3600));
            if h3.is_none() {
                timeout = timeout.min(chrome::HANDSHAKE_TIMEOUT.saturating_sub(started.elapsed()));
            }
            let sleep = tokio::time::sleep(timeout);
            tokio::select! {
                got = socket.recv_from(&mut buf) => {
                    if let Ok((n, from)) = got {
                        let info = quiche::RecvInfo { from, to: local };
                        let _ = conn.recv(&mut buf[..n], info);
                        // Everything else that has arrived.
                        while let Ok((n, from)) = socket.try_recv_from(&mut buf) {
                            let info = quiche::RecvInfo { from, to: local };
                            let _ = conn.recv(&mut buf[..n], info);
                        }
                    }
                }
                () = sleep => conn.on_timeout(),
                job = queue.recv(), if accepting => match job {
                    Some(job) => match h3.as_mut() {
                        Some(h3) => start(h3, &mut conn, &mut streams, job, self.browser),
                        None => waiting.push(job),
                    },
                    None => accepting = false,
                },
            }
        }
    }
}

/// Sends every datagram quiche has ready.
async fn send_all(
    socket: &tokio::net::UdpSocket,
    conn: &mut quiche::Connection,
    out: &mut [u8],
) -> io::Result<()> {
    loop {
        match conn.send(out) {
            Ok((n, info)) => {
                socket.send_to(&out[..n], info.to).await?;
            }
            Err(quiche::Error::Done) => return Ok(()),
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
    }
}

/// Sends what's left, a CONNECTION_CLOSE most likely, ignoring errors.
async fn flush(socket: &tokio::net::UdpSocket, conn: &mut quiche::Connection, out: &mut [u8]) {
    let _ = send_all(socket, conn, out).await;
}

/// Sends a request: Chrome's pseudo-header order, `:method :authority
/// :scheme :path`, then its headers as given, and a PRIORITY_UPDATE
/// with the `priority` header's value unless it's the default. Safari
/// sends `:method :scheme :authority :path` and no PRIORITY_UPDATE, its
/// `priority` header being the only word of it.
fn start(
    h3: &mut quiche::h3::Connection,
    conn: &mut quiche::Connection,
    streams: &mut HashMap<u64, Stream>,
    job: Job,
    browser: Browser,
) {
    let Job {
        request,
        head,
        deadline,
    } = job;
    let mut headers = vec![
        quiche::h3::Header::new(b":method", request.method.as_bytes()),
        quiche::h3::Header::new(b":authority", request.authority.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b":path", request.path.as_bytes()),
    ];
    if browser == Browser::Safari {
        headers.swap(1, 2);
    }
    headers.extend(
        request
            .headers
            .iter()
            .map(|(k, v)| quiche::h3::Header::new(k.as_bytes(), v)),
    );
    let body = request.body.unwrap_or_default();
    let stream_id = match h3.send_request(conn, &headers, body.is_empty()) {
        Ok(id) => id,
        Err(e) => {
            let _ = head.send(Err(FetchError::new(
                FetchErrorKind::Other,
                format!("HTTP/3 request: {e}"),
            )));
            return;
        }
    };
    let (urgency, incremental) = priority(&request.headers);
    if browser == Browser::Chrome && (urgency, incremental) != (3, false) {
        let _ = h3.send_priority_update_for_request(
            conn,
            stream_id,
            &quiche::h3::Priority::new(urgency, incremental),
        );
    }
    let mut stream = Stream {
        head: Some(head),
        deadline,
        body: None,
        ended: body.is_empty(),
        unsent: body,
    };
    push_body(h3, conn, stream_id, &mut stream);
    streams.insert(stream_id, stream);
}

/// Hands quiche as much of the request body as it takes, ending the
/// request once all of it is in.
fn push_body(
    h3: &mut quiche::h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    stream: &mut Stream,
) {
    if stream.ended {
        return;
    }
    while !stream.unsent.is_empty() {
        let last = true;
        match h3.send_body(conn, stream_id, &stream.unsent, last) {
            Ok(n) if n > 0 => {
                let _ = stream.unsent.split_to(n);
            }
            _ => return,
        }
    }
    stream.ended = true;
}

/// Reads what HTTP/3 has for the requests in flight.
fn events(
    h3: &mut quiche::h3::Connection,
    conn: &mut quiche::Connection,
    streams: &mut HashMap<u64, Stream>,
    buf: &mut [u8],
) {
    loop {
        match h3.poll(conn) {
            Ok((id, quiche::h3::Event::Headers { list, .. })) => {
                let Some(s) = streams.get_mut(&id) else {
                    continue;
                };
                // A 1xx response is followed by the real one.
                let status = list
                    .iter()
                    .find(|h| h.name() == b":status")
                    .and_then(|h| std::str::from_utf8(h.value()).ok()?.parse::<u16>().ok());
                if status.is_some_and(|s| (100..200).contains(&s)) || s.head.is_none() {
                    continue;
                }
                let (tx, rx) = mpsc::unbounded_channel();
                let response = response(&list, Body::new(rx, s.deadline));
                if let Some(head) = s.head.take() {
                    let _ = head.send(response);
                }
                s.body = Some(tx);
            }
            Ok((id, quiche::h3::Event::Data)) => {
                while let Ok(n) = h3.recv_body(conn, id, buf) {
                    if let Some(body) = streams.get(&id).and_then(|s| s.body.as_ref()) {
                        let _ = body.send(Ok(Bytes::copy_from_slice(&buf[..n])));
                    }
                }
            }
            Ok((id, quiche::h3::Event::Finished)) => {
                streams.remove(&id);
            }
            Ok((id, quiche::h3::Event::Reset(code))) => {
                if let Some(s) = streams.remove(&id) {
                    s.end(FetchError::new(
                        FetchErrorKind::Body,
                        format!("the server reset the HTTP/3 stream (error {code:#x})"),
                    ));
                }
            }
            Ok(_) => {}
            Err(quiche::h3::Error::Done) => return,
            Err(_) => return,
        }
    }
}

fn response(
    headers: &[quiche::h3::Header],
    body: Body,
) -> Result<http::Response<Body>, FetchError> {
    let bad = || FetchError::new(FetchErrorKind::Other, "a bad HTTP/3 response head");
    let status = headers
        .iter()
        .find(|h| h.name() == b":status")
        .and_then(|h| std::str::from_utf8(h.value()).ok()?.parse::<u16>().ok())
        .ok_or_else(bad)?;
    let mut builder = http::Response::builder()
        .status(status)
        .version(http::Version::HTTP_3);
    for h in headers.iter().filter(|h| !h.name().starts_with(b":")) {
        builder = builder.header(h.name(), h.value());
    }
    builder.body(body).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::{priority, varint};

    #[test]
    fn varints_and_priorities() {
        assert_eq!(varint(7648), vec![0x5d, 0xe0]);
        assert_eq!(varint(37), vec![37]);
        assert_eq!(varint(100_000), vec![0x80, 0x01, 0x86, 0xa0]);
        let of = |v: &str| priority(&[("priority".into(), v.as_bytes().to_vec())]);
        assert_eq!(of("u=0, i"), (0, true));
        assert_eq!(of("u=2"), (2, false));
        assert_eq!(priority(&[]), (3, false));
    }
}

/// Ports Chrome never connects to (net/base/port_util.cc,
/// kRestrictedPorts): an alternative on one fails with ERR_UNSAFE_PORT
/// before any packet goes out (HttpStreamFactory::Job::DoStart).
pub(crate) const RESTRICTED_PORTS: [u16; 80] = [
    1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101, 102,
    103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427, 465,
    512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990, 993,
    995, 1719, 1720, 1723, 2049, 3659, 4045, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668, 6669,
    6697, 10080,
];

/// One entry of an Alt-Svc header as Chrome keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AltSvc {
    /// "h3", or another protocol Chrome stores but doesn't use for QUIC.
    pub protocol: String,
    /// Empty for the origin's own host.
    pub host: String,
    pub port: u16,
    pub max_age: u32,
}

/// An Alt-Svc value as Chrome reads it (QUICHE's
/// SpdyAltSvcWireFormat::ParseHeaderFieldValue): `None` for a header
/// Chrome rejects whole, leaving what it knew, and an empty list for
/// `clear`, which forgets it.
pub(crate) fn parse_alt_svc(value: &str) -> Option<Vec<AltSvc>> {
    let b = value.as_bytes();
    if b.is_empty() {
        return None;
    }
    if value == "clear" {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut i = 0;
    let ws = |c: u8| c == b' ' || c == b'\t';
    loop {
        while i < b.len() && (ws(b[i]) || b[i] == b',') {
            i += 1;
        }
        if i == b.len() {
            return Some(out);
        }
        // protocol-id, percent-encoded, up to '='.
        let start = i;
        while i < b.len() && b[i] != b'=' {
            i += 1;
        }
        if i == b.len() || i == start {
            return None;
        }
        let protocol = percent_decode(&b[start..i])?;
        i += 1;
        // The authority, quoted, with backslash escapes.
        if b.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        let mut authority = Vec::new();
        loop {
            match b.get(i)? {
                b'"' => break,
                b'\\' => {
                    i += 1;
                    authority.push(*b.get(i)?);
                }
                &c => authority.push(c),
            }
            i += 1;
        }
        i += 1;
        let authority = String::from_utf8(authority).ok()?;
        let (host, port) = authority.rsplit_once(':')?;
        if host.starts_with('[') != host.ends_with(']') {
            return None;
        }
        if port.is_empty() || !port.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let port: u16 = port.parse().ok().filter(|&p| p > 0)?;
        let mut max_age = 86_400;
        // Parameters: `;name=value`, until the next entry.
        loop {
            while i < b.len() && ws(b[i]) {
                i += 1;
            }
            if i == b.len() || b[i] == b',' {
                break;
            }
            if b[i] != b';' {
                return None;
            }
            i += 1;
            while i < b.len() && ws(b[i]) {
                i += 1;
            }
            let start = i;
            while i < b.len() && b[i] != b'=' && b[i] != b';' && b[i] != b',' {
                i += 1;
            }
            if b.get(i) != Some(&b'=') {
                return None;
            }
            let name = String::from_utf8_lossy(&b[start..i])
                .trim()
                .to_ascii_lowercase();
            i += 1;
            let start = i;
            while i < b.len() && !matches!(b[i], b';' | b' ' | b'\t' | b',') {
                i += 1;
            }
            let v = &value[start..i];
            if v.is_empty() {
                return None;
            }
            if name == "ma" {
                // ParsePositiveInteger32: digits only, above zero.
                if !v.bytes().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                max_age = v
                    .parse::<u32>()
                    .ok()
                    .filter(|&m| m > 0 && m <= i32::MAX as u32)?;
            }
        }
        out.push(AltSvc {
            protocol,
            host: host.to_string(),
            port,
            max_age,
        });
    }
}

fn percent_decode(s: &[u8]) -> Option<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            let hex = std::str::from_utf8(s.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// An alternative Chrome has marked broken, and how often
/// (BrokenAlternativeServices, with Chrome's field trial defaults:
/// kInitialDelayForBrokenAlternativeService at 10 s, exponential back-off
/// on it, kMaxDelayForBrokenAlternativeService at 30 minutes).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Broken {
    /// Until when it stays broken, if it is.
    pub until: Option<Instant>,
    /// How many times it has been marked, which the next delay doubles
    /// from; kept after it expires ("recently broken").
    pub count: u32,
}

/// The first time an alternative is marked broken.
pub(crate) const BROKEN_INITIAL_DELAY: Duration = Duration::from_secs(10);
/// The longest it stays broken.
pub(crate) const BROKEN_MAX_DELAY: Duration = Duration::from_secs(30 * 60);

impl Broken {
    /// Marks it broken (ComputeBrokenAlternativeServiceExpirationDelay:
    /// initial × 2^count, the shift at most 18). Marking one already
    /// broken counts, and leaves its expiry.
    pub(crate) fn mark(&mut self, now: Instant) {
        if self.until.is_none_or(|u| u <= now) {
            let delay = BROKEN_INITIAL_DELAY
                .saturating_mul(1 << self.count.min(18))
                .min(BROKEN_MAX_DELAY);
            self.until = Some(now + delay);
        }
        self.count = self.count.saturating_add(1);
    }

    pub(crate) fn is_broken(&self, now: Instant) -> bool {
        self.until.is_some_and(|u| now < u)
    }
}

/// How long the TCP job waits for a QUIC one still connecting
/// (QuicSessionPool::GetTimeDelayForWaitingJob and
/// JobController::MaybeSetWaitTimeForMainJob): not at all until QUIC has
/// worked on this network, or when the alternative was recently broken;
/// otherwise one and a half times the server's smoothed RTT, or of 300 ms
/// without one, at most 3 s.
pub(crate) fn main_job_delay(
    worked: bool,
    recently_broken: bool,
    rtt: Option<Duration>,
) -> Duration {
    if !worked || recently_broken {
        return Duration::ZERO;
    }
    rtt.unwrap_or(Duration::from_millis(300))
        .mul_f64(1.5)
        .min(Duration::from_secs(3))
}

#[cfg(test)]
mod rules {
    use super::*;

    #[test]
    fn alt_svc_is_read_as_chrome_reads_it() {
        let h3 = |host: &str, port, max_age| AltSvc {
            protocol: "h3".into(),
            host: host.into(),
            port,
            max_age,
        };
        assert_eq!(
            parse_alt_svc(r#"h3=":443"; ma=3600, h3-29="alt.example:8443""#),
            Some(vec![
                h3("", 443, 3600),
                AltSvc {
                    protocol: "h3-29".into(),
                    ..h3("alt.example", 8443, 86_400)
                }
            ])
        );
        assert_eq!(parse_alt_svc("clear"), Some(vec![]));
        assert_eq!(
            parse_alt_svc(r#"h3="[::1]:8443";persist=1"#),
            Some(vec![h3("[::1]", 8443, 86_400)])
        );
        // Any of these sinks the whole header.
        for bad in [
            "",
            r#"h3=":443"; ma=0"#,
            r#"h3=":443"; ma="60""#,
            r#"h3=":0""#,
            r#"h3=:443"#,
            r#"h3=":443"; ma"#,
            r#"h3=":443", h2="x:65536""#,
        ] {
            assert_eq!(parse_alt_svc(bad), None, "{bad}");
        }
    }

    #[test]
    fn broken_alternatives_back_off_as_chromes_do() {
        let now = Instant::now();
        let mut b = Broken::default();
        let mut delays = Vec::new();
        for _ in 0..10 {
            b.mark(now);
            delays.push(b.until.unwrap() - now);
            b.until = Some(now); // expired, still recently broken
        }
        let secs: Vec<u64> = delays.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, [10, 20, 40, 80, 160, 320, 640, 1280, 1800, 1800]);
        let mut b = Broken::default();
        b.mark(now);
        b.mark(now);
        assert_eq!(
            b.until,
            Some(now + BROKEN_INITIAL_DELAY),
            "a second mark keeps the expiry"
        );
        assert_eq!(b.count, 2);
    }

    #[test]
    fn the_tcp_job_waits_as_chromes_does() {
        let ms = |d: Duration| d.as_millis();
        assert_eq!(ms(main_job_delay(false, false, None)), 0);
        assert_eq!(ms(main_job_delay(true, true, None)), 0);
        assert_eq!(ms(main_job_delay(true, false, None)), 450);
        assert_eq!(
            ms(main_job_delay(true, false, Some(Duration::from_millis(40)))),
            60
        );
        assert_eq!(
            ms(main_job_delay(true, false, Some(Duration::from_secs(5)))),
            3000
        );
    }
}
