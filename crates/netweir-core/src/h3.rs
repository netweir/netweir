//! Requests over HTTP/3 the way Firefox 160 sends them: through neqo and
//! NSS, Mozilla's own QUIC stack and TLS library, at the versions Firefox
//! 160.0.1 builds with, configured as Firefox's
//! `netwerk/socket/neqo_glue/src/lib.rs` configures them, with the values
//! of the preferences it reads there. The QUIC Initial, the ClientHello in
//! it, the transport parameters, the HTTP/3 SETTINGS and streams, QPACK,
//! loss recovery and congestion control are then Firefox's by
//! construction, not a copy of them.
//!
//! Each connection runs on a thread of its own, as neqo's state isn't
//! `Send`; requests reach it over a channel, as Firefox's reach its socket
//! thread.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use btls::x509::store::{X509Store, X509StoreBuilder};
use btls::x509::{X509, X509StoreContext};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame};
use neqo_common::event::Provider as _;
use neqo_common::{Header, datagram};
use neqo_http3::{Http3Client, Http3ClientEvent, Http3Parameters, Http3State, Priority};
use neqo_transport::{
    CongestionControl, Connection, ConnectionParameters, HyStartCssBaseline, OutputBatch,
    RandomConnectionIdGenerator, SlowStart, StreamDataLimit, StreamId, Version,
};
use nss_rs::cert::CertificateCompressor;
use nss_rs::{AuthenticationStatus, ResumptionToken};
use tokio::sync::{mpsc, oneshot, watch};
use url::Url;

use crate::fetch::{FetchError, FetchErrorKind};

/// Firefox 160's values for what `neqo_glue` reads from preferences and
/// from `Http3Session::Init`'s arguments (modules/libpref/init/
/// StaticPrefList.yaml and all.js at FIREFOX_160_0_1_RELEASE).
mod firefox {
    /// network.http.http3.max_data
    pub const MAX_DATA: u64 = 25_165_824;
    /// network.http.http3.max_stream_data
    pub const MAX_STREAM_DATA: u64 = 12_582_912;
    /// network.http.http3.default-qpack-table-size (all.js)
    pub const QPACK_TABLE_SIZE: u64 = 65_536;
    /// network.http.http3.default-max-stream-blocked (all.js)
    pub const MAX_BLOCKED_STREAMS: u16 = 20;
    /// network.http.http3.idle_timeout, in seconds
    pub const IDLE_TIMEOUT: u64 = 30;
    /// network.http.http3.recvBufferSize: what Firefox asks of the UDP
    /// socket's receive buffer.
    pub const RECV_BUFFER: usize = 1 << 20;
    /// network.http.http3.backup_timer_delay: how long a request waits
    /// for an HTTP/3 connection still being made before it goes over TCP
    /// instead (nsHttpTransaction::OnHttp3BackupTimer).
    pub const BACKUP_TIMER: std::time::Duration = std::time::Duration::from_millis(100);
    /// An Alt-Svc alternative's lifetime without `ma=`, in seconds
    /// (AltSvcMapping::ProcessHeader).
    pub const ALT_SVC_MAX_AGE: i64 = 86_400;
}

/// The connection settings Firefox 160 makes, line for line in the order
/// `NeqoHttp3Conn::new` makes them.
fn firefox_client(
    server_name: &str,
    local: SocketAddr,
    remote: SocketAddr,
    resume: Option<&ResumptionToken>,
) -> Result<Http3Client, String> {
    nss_rs::init().map_err(|e| format!("NSS: {e:?}"))?;
    gecko::defaults()?;
    // network.http.http3.version_negotiation.enabled: true.
    let versions = Version::all();
    // Http3Session passes pmtud_enabled only for a proxy's connection,
    // and network.http.http3.pmtud is false, so PMTUD is off.
    let pmtud = false;
    let params = ConnectionParameters::default()
        .versions(Version::Version1, versions)
        // network.http.http3.cc_algorithm: 1.
        .congestion_control(CongestionControl::Cubic)
        // network.http.http3.slow_start_algorithm: 0.
        .slow_start(SlowStart::Classic)
        .max_data(firefox::MAX_DATA)
        .max_stream_data(StreamDataLimit::BiDiLocal, firefox::MAX_STREAM_DATA)
        // security.tls.grease_http3_enable: false.
        .grease(false)
        // network.http.http3.sni-slicing: true.
        .sni_slicing(true)
        .idle_timeout(Duration::from_secs(firefox::IDLE_TIMEOUT))
        .pmtud_iface_mtu(cfg!(not(target_os = "openbsd")))
        .mlkem(false)
        .pmtud(pmtud)
        // network.http.http3.spurious_recovery: true.
        .spurious_recovery(true)
        // network.http.http3.hystart_alternative_css_baseline: false.
        .hystart_css_baseline(HyStartCssBaseline::CurrentRoundMinRtt);
    // fast_pto is 0 for every connection but DNS over HTTPS's, which
    // leaves neqo's default.
    let settings = Http3Parameters::default()
        .max_table_size_encoder(firefox::QPACK_TABLE_SIZE)
        .max_table_size_decoder(firefox::QPACK_TABLE_SIZE)
        .max_blocked_streams(firefox::MAX_BLOCKED_STREAMS)
        .connection_parameters(params)
        // WebTransport only for a WebTransport session's connection.
        .webtransport(false)
        .connect(true)
        .http3_datagram(true);
    let mut conn = Connection::new_client(
        server_name,
        &["h3"],
        Rc::new(RefCell::new(RandomConnectionIdGenerator::new(3))),
        local,
        remote,
        settings.get_connection_parameters().clone(),
        Instant::now(),
    )
    .map_err(|e| format!("{e:?}"))?;
    // security.tls.enable_kyber and network.http.http3.enable_kyber are
    // both true, and security.tls.enable_mlkem1024 false.
    conn.set_groups(&[
        nss_rs::TLS_GRP_KEM_MLKEM768X25519,
        nss_rs::TLS_GRP_EC_X25519,
        nss_rs::TLS_GRP_EC_SECP256R1,
        nss_rs::TLS_GRP_EC_SECP384R1,
        nss_rs::TLS_GRP_EC_SECP521R1,
    ])
    .map_err(|e| format!("{e:?}"))?;
    conn.send_additional_key_shares(2)
        .map_err(|e| format!("{e:?}"))?;
    // Each enabled by its two preferences, all true.
    conn.set_certificate_compression::<Zlib>()
        .map_err(|e| format!("{e:?}"))?;
    conn.set_certificate_compression::<Zstd>()
        .map_err(|e| format!("{e:?}"))?;
    conn.set_certificate_compression::<Brotli>()
        .map_err(|e| format!("{e:?}"))?;
    let mut client = Http3Client::new_with_conn(conn, settings);
    // Http3Session::Init: with no ECH configuration for the host,
    // security.tls.ech.grease_http3 (true) and grease_probability (100)
    // make every connection GREASE ECH, which an empty configuration does.
    client.enable_ech([]).map_err(|e| format!("{e:?}"))?;
    // Then, network.http.http3.enable_0rtt being true, the session ticket
    // the last connection to this server left, if it hasn't expired.
    if let Some(token) = resume.filter(|t| t.expiration_time() > Instant::now()) {
        // A ticket neqo won't take leaves a fresh handshake, as in Firefox.
        let _ = client.enable_resumption(Instant::now(), token);
    }
    Ok(client)
}

/// What Firefox sets for all of NSS when it starts, before any connection:
/// `CommonInit` and `InitializeCipherSuite` in
/// security/manager/ssl/nsNSSComponent.cpp, with Firefox 160's preference
/// values. neqo sets most of what a ClientHello carries per connection,
/// but leaves these to the process-wide defaults; two of them show in it
/// (the delegated_credentials extension, and no ML-DSA among the signature
/// algorithms). nss-rs has no binding for them, hence the C calls.
#[allow(unsafe_code)]
mod gecko {
    use std::ffi::c_int;
    use std::sync::OnceLock;

    // ssl.h and secoidt.h, NSS 3.132.
    const SSL_ENABLE_SSL2: i32 = 7;
    const SSL_NO_CACHE: i32 = 9;
    const SSL_V2_COMPATIBLE_HELLO: i32 = 12;
    const SSL_ENABLE_SESSION_TICKETS: i32 = 18;
    const SSL_ENABLE_RENEGOTIATION: i32 = 20;
    const SSL_REQUIRE_SAFE_NEGOTIATION: i32 = 21;
    const SSL_ENABLE_FALSE_START: i32 = 22;
    const SSL_ENABLE_ALPN: i32 = 26;
    const SSL_ENABLE_EXTENDED_MASTER_SECRET: i32 = 30;
    const SSL_ENABLE_0RTT_DATA: i32 = 33;
    const SSL_ENABLE_HELLO_DOWNGRADE_CHECK: i32 = 37;
    const SSL_ENABLE_POST_HANDSHAKE_AUTH: i32 = 39;
    const SSL_ENABLE_DELEGATED_CREDENTIALS: i32 = 40;
    const SSL_DB_LOAD_CERTIFICATE_CHAIN: i32 = 44;
    const SSL_RENEGOTIATE_REQUIRES_XTN: c_int = 2;
    const SEC_OID_ML_DSA_44: c_int = 391;
    const SEC_OID_ML_DSA_65: c_int = 392;
    const SEC_OID_ML_DSA_87: c_int = 393;
    const SEC_OID_MLKEM768X25519: c_int = 389;
    const NSS_USE_ALG_IN_SSL_KX: u32 = 0x0000_0004;

    unsafe extern "C" {
        fn SSL_OptionSetDefault(option: i32, value: c_int) -> c_int;
        fn NSS_SetAlgorithmPolicy(tag: c_int, set: u32, clear: u32) -> c_int;
        #[cfg(windows)]
        fn PR_CeilingLog2(i: u32) -> c_int;
    }

    // On Windows NSPR's plds is built to call NSPR through its DLL, and
    // MSVC's linker binds such a call to a static library only for a
    // function something else has already pulled in. Nothing else calls
    // PR_CeilingLog2, which PL_NewHashTable does, so this pulls it in.
    #[cfg(windows)]
    #[used]
    static PULL_PR_CEILING_LOG2: unsafe extern "C" fn(u32) -> c_int = PR_CeilingLog2;

    /// Once per process, after NSS's own initialisation.
    pub(super) fn defaults() -> Result<(), String> {
        static DONE: OnceLock<Result<(), String>> = OnceLock::new();
        DONE.get_or_init(|| {
            let options = [
                (SSL_ENABLE_SSL2, 0),
                (SSL_V2_COMPATIBLE_HELLO, 0),
                // security.ssl.disable_session_identifiers: false.
                (SSL_ENABLE_SESSION_TICKETS, 1),
                (SSL_NO_CACHE, 0),
                // security.ssl.require_safe_negotiation: false.
                (SSL_REQUIRE_SAFE_NEGOTIATION, 0),
                (SSL_ENABLE_RENEGOTIATION, SSL_RENEGOTIATE_REQUIRES_XTN),
                (SSL_ENABLE_EXTENDED_MASTER_SECRET, 1),
                // security.tls.hello_downgrade_check: true.
                (SSL_ENABLE_HELLO_DOWNGRADE_CHECK, 1),
                // security.ssl.enable_false_start: true.
                (SSL_ENABLE_FALSE_START, 1),
                // security.ssl.enable_alpn: true.
                (SSL_ENABLE_ALPN, 1),
                // security.tls.enable_0rtt_data: true.
                (SSL_ENABLE_0RTT_DATA, 1),
                // security.tls.enable_post_handshake_auth: false.
                (SSL_ENABLE_POST_HANDSHAKE_AUTH, 0),
                // security.tls.enable_delegated_credentials: true.
                (SSL_ENABLE_DELEGATED_CREDENTIALS, 1),
                (SSL_DB_LOAD_CERTIFICATE_CHAIN, 0),
            ];
            for (option, value) in options {
                // SAFETY: SSL_OptionSetDefault takes an option number and
                // a value, and NSS is initialised (nss_rs::init) before this
                // runs.
                if unsafe { SSL_OptionSetDefault(option, value) } != 0 {
                    return Err(format!("NSS refused SSL option {option}"));
                }
            }
            // SetKyberPolicy: security.tls.enable_kyber true, and
            // security.tls.enable_mldsa false. (ML-KEM-1024 is off too, and
            // neqo never offers it, as above.)
            let policy = [
                (SEC_OID_MLKEM768X25519, NSS_USE_ALG_IN_SSL_KX, 0),
                (SEC_OID_ML_DSA_44, 0, NSS_USE_ALG_IN_SSL_KX),
                (SEC_OID_ML_DSA_65, 0, NSS_USE_ALG_IN_SSL_KX),
                (SEC_OID_ML_DSA_87, 0, NSS_USE_ALG_IN_SSL_KX),
            ];
            for (tag, set, clear) in policy {
                // SAFETY: as above; the tags are SECOidTag values.
                if unsafe { NSS_SetAlgorithmPolicy(tag, set, clear) } != 0 {
                    return Err(format!("NSS refused the policy for OID tag {tag}"));
                }
            }
            Ok(())
        })
        .clone()
    }
}

/// Certificate decompression as Firefox registers it (RFC 8879). The
/// decoders differ from Firefox's own (zlib-rs, and its zstd and brotli
/// builds); only what they decode is visible, which is the same.
struct Zlib;
struct Zstd;
struct Brotli;

impl CertificateCompressor for Zlib {
    const ID: u16 = 0x1;
    const NAME: &std::ffi::CStr = c"zlib";
    fn decode(input: &[u8], output: &mut [u8]) -> nss_rs::Res<()> {
        let mut d = flate2::Decompress::new(true);
        match d.decompress(input, output, flate2::FlushDecompress::Finish) {
            Ok(flate2::Status::StreamEnd) if d.total_out() as usize == output.len() => Ok(()),
            _ => Err(nss_rs::Error::CertificateDecoding),
        }
    }
}

impl CertificateCompressor for Zstd {
    const ID: u16 = 0x3;
    const NAME: &std::ffi::CStr = c"zstd";
    fn decode(input: &[u8], output: &mut [u8]) -> nss_rs::Res<()> {
        match zstd::bulk::decompress_to_buffer(input, output) {
            Ok(n) if n == output.len() => Ok(()),
            _ => Err(nss_rs::Error::CertificateDecoding),
        }
    }
}

impl CertificateCompressor for Brotli {
    const ID: u16 = 0x2;
    const NAME: &std::ffi::CStr = c"brotli";
    fn decode(input: &[u8], output: &mut [u8]) -> nss_rs::Res<()> {
        let mut out = Vec::with_capacity(output.len());
        let read = std::io::Read::read_to_end(
            &mut std::io::Read::take(
                brotli::Decompressor::new(input, 4096),
                output.len() as u64 + 1,
            ),
            &mut out,
        );
        match read {
            Ok(n) if n == output.len() => {
                output.copy_from_slice(&out);
                Ok(())
            }
            _ => Err(nss_rs::Error::CertificateDecoding),
        }
    }
}

/// How a server's certificate is checked. NSS hands the chain over and
/// leaves the decision to the caller, as it does to Firefox; it's made
/// with BoringSSL against the roots the TCP connections use, so both
/// trust the same servers.
#[derive(Clone)]
pub(crate) enum Trust {
    Roots(Arc<X509Store>),
    Anything,
}

impl Trust {
    pub(crate) fn new(verify: bool, pem: Option<&[u8]>) -> Result<Trust, FetchError> {
        if !verify {
            return Ok(Trust::Anything);
        }
        let bad = |e: btls::error::ErrorStack| FetchError::invalid(format!("CA certificates: {e}"));
        let mut store = X509StoreBuilder::new().map_err(bad)?;
        match pem {
            Some(pem) => {
                for cert in X509::stack_from_pem(pem).map_err(bad)? {
                    store.add_cert(cert).map_err(bad)?;
                }
            }
            None => {
                for der in chromium_roots::TLS_SERVER_ROOT_CERTS {
                    // A root BoringSSL can't read is one no connection
                    // could be checked against anyway.
                    if let Ok(cert) = X509::from_der(der.as_ref()) {
                        let _ = store.add_cert(cert);
                    }
                }
            }
        }
        Ok(Trust::Roots(Arc::new(store.build())))
    }

    /// Whether `chain` (leaf first, DER) is good for `host`.
    fn check(&self, host: &str, chain: &[Vec<u8>]) -> AuthenticationStatus {
        let Trust::Roots(store) = self else {
            return AuthenticationStatus::Ok;
        };
        let check = || -> Result<bool, btls::error::ErrorStack> {
            let leaf = X509::from_der(chain.first().map_or(&[][..], Vec::as_slice))?;
            let mut rest = btls::stack::Stack::new()?;
            for der in chain.iter().skip(1) {
                rest.push(X509::from_der(der)?)?;
            }
            let mut ctx = X509StoreContext::new()?;
            ctx.init(store, &leaf, &rest, |ctx| {
                let param = ctx.verify_param_mut();
                match host.parse::<IpAddr>() {
                    Ok(ip) => param.set_ip(ip)?,
                    Err(_) => param.set_host(host)?,
                }
                ctx.verify_cert()
            })
        };
        match check() {
            Ok(true) => AuthenticationStatus::Ok,
            _ => AuthenticationStatus::CertUntrusted,
        }
    }
}

/// One request, its headers as Firefox hands them to neqo: in order,
/// lowercase, with no pseudo-headers (neqo writes those) and none that
/// HTTP/3 forbids.
pub(crate) struct Request {
    pub method: String,
    pub authority: String,
    pub path: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Option<Bytes>,
}

/// The `priority` header Firefox sends, as the urgency and incremental
/// flag it also gives neqo (`neqo_http3conn_fetch`), for the PRIORITY_UPDATE
/// frames neqo would send if they changed (RFC 9218).
fn priority(headers: &[(String, Vec<u8>)]) -> Priority {
    let Some((_, value)) = headers.iter().find(|(k, _)| k == "priority") else {
        return Priority::default();
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
    Priority::new(urgency, incremental)
}

type Chunk = Result<Bytes, io::Error>;

/// A response body as it arrives on its stream, until the request's
/// deadline.
pub(crate) struct Body {
    chunks: mpsc::UnboundedReceiver<Chunk>,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl HttpBody for Body {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        if let Poll::Ready(chunk) = self.chunks.poll_recv(cx) {
            return Poll::Ready(chunk.map(|c| c.map(Frame::data)));
        }
        match self.deadline.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the response body took longer than the request's timeout",
            )))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The service tower-http's decompression wraps: one answer, already in.
struct Answered(Option<http::Response<Body>>);

impl tower::Service<http::Request<()>> for Answered {
    type Response = http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = std::future::Ready<Result<http::Response<Body>, std::convert::Infallible>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<()>) -> Self::Future {
        std::future::ready(Ok(self.0.take().expect("called once")))
    }
}

/// `response` as wreq gives one, its body decoded as wreq's own
/// decompression layer (tower-http's) decodes the TCP connections'.
pub(crate) async fn decompressed(
    response: http::Response<Body>,
    accept_encoding: Option<http::HeaderValue>,
) -> wreq::Response {
    use tower::Service;
    let mut request = http::Request::new(());
    if let Some(accept) = accept_encoding {
        request
            .headers_mut()
            .insert(http::header::ACCEPT_ENCODING, accept);
    }
    let mut service = tower_http::decompression::Decompression::new(Answered(Some(response)));
    let Ok(response) = service.call(request).await;
    wreq::Response::from(response.map(wreq::Body::wrap))
}

/// What the connection's thread needs to send a request and answer it.
struct Job {
    request: Request,
    head: oneshot::Sender<Result<http::Response<Body>, FetchError>>,
    deadline: tokio::time::Instant,
}

/// How far an HTTP/3 connection has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Connecting,
    Connected,
    /// It never connected. Firefox stops using HTTP/3 to the alternative's
    /// host after any such failure but a TLS one (Http3Session::Shutdown,
    /// DnsAndConnectSocket::SetupConn).
    Failed {
        exclude: bool,
    },
    /// It connected, and has closed since.
    Closed,
}

/// An open HTTP/3 connection, or one opening.
#[derive(Clone)]
struct Handle {
    jobs: mpsc::UnboundedSender<Job>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    state: watch::Receiver<State>,
}

impl Handle {
    fn usable(&self) -> bool {
        !self.jobs.is_closed()
            && matches!(*self.state.borrow(), State::Connecting | State::Connected)
    }
}

/// An origin's HTTP/3 alternative, as Firefox's AltSvcMapping holds it.
#[derive(Debug, Clone)]
struct Alternative {
    /// `host:port`.
    to: String,
    expires: Instant,
    /// Whether a connection to it has been made, with a certificate that
    /// checked out. Firefox uses an alternative only once it has
    /// (AltSvcCache::GetAltServiceMapping, AltSvcTransaction::MaybeValidate).
    validated: bool,
    /// Set by `Fetcher::use_http3`: kept whatever Alt-Svc says, and used
    /// without racing TCP or falling back to it.
    forced: bool,
}

/// Where a request goes over HTTP/3.
pub(crate) struct Route {
    /// The request's network partition.
    pub partition: String,
    /// `host:port`.
    pub alternative: String,
    pub forced: bool,
}

/// The hosts HTTP/3 failed to reach, as Firefox's mExcludedHttp3Origins
/// keeps them: until the network changes (nsHttpHandler::Observe on
/// NS_NETWORK_LINK_TOPIC clears them all), with no timer of its own.
#[derive(Default)]
struct Excluded {
    hosts: HashSet<String>,
    /// The network they failed on.
    network: Network,
}

/// The local addresses the system would send from to reach the internet
/// over IPv4 and IPv6: what changes when the machine moves to another
/// network. Netweir has no link-change notice as Firefox does, so a change
/// here stands for one.
type Network = (Option<IpAddr>, Option<IpAddr>);

/// The network this machine is on now. Connecting a UDP socket sends
/// nothing; it only asks the system which address it would send from.
fn network() -> Network {
    let from = |bind: &str, to: &str| {
        let socket = std::net::UdpSocket::bind(bind).ok()?;
        socket.connect(to).ok()?;
        Some(socket.local_addr().ok()?.ip())
    };
    // Documentation addresses (RFC 5737, RFC 3849): routed by default,
    // never reached.
    (
        from("0.0.0.0:0", "192.0.2.1:443"),
        from("[::]:0", "[2001:db8::1]:443"),
    )
}

impl Excluded {
    /// Whether `host` is excluded, after forgetting every exclusion if
    /// the network has changed since they were made.
    fn has(&mut self, host: &str, now: impl FnOnce() -> Network) -> bool {
        if self.hosts.is_empty() {
            return false;
        }
        if now() != self.network {
            self.hosts.clear();
            return false;
        }
        self.hosts.contains(host)
    }

    fn add(&mut self, host: &str, now: impl FnOnce() -> Network) {
        if self.hosts.is_empty() {
            self.network = now();
        }
        self.hosts.insert(host.to_string());
    }
}

/// The host part of an alternative's `host:port`.
fn host_of(alternative: &str) -> &str {
    alternative
        .rsplit_once(':')
        .map_or(alternative, |(host, _)| host)
}

/// What became of a request sent over HTTP/3.
pub(crate) enum Answer {
    Response(http::Response<Body>),
    /// It wasn't sent, or failed before any of its response came, in a way
    /// that Firefox would send it again over TCP: the connection wasn't up
    /// within the backup timer, or never came up, or closed under a request
    /// that is safe to repeat.
    Tcp,
}

/// Where each origin's requests go over HTTP/3, and the connections to
/// them, shared by every clone of a Fetcher.
pub(crate) struct Routes {
    trust: Trust,
    /// Each origin's alternative, by network partition (the request's
    /// top-level site) and origin (`https://host:port`), as Firefox keys
    /// them: the origin attributes in AltSvcMapping::MakeHashKey carry the
    /// partition key when network state is partitioned, as it is by
    /// default. They live as long as the Fetcher, as in a private window:
    /// nothing is written to disk.
    alternatives: Mutex<HashMap<(String, String), Alternative>>,
    excluded: Mutex<Excluded>,
    /// Connections by partition, server name and alternative.
    connections: Mutex<HashMap<String, Handle>>,
    /// The newest session ticket from each server, for the next
    /// connection to it, as Firefox's SSLTokensCache keeps them. Each is
    /// used once.
    tickets: Tickets,
}

type Tickets = Arc<Mutex<HashMap<String, ResumptionToken>>>;

impl Routes {
    pub(crate) fn new(trust: Trust) -> Routes {
        Routes {
            trust,
            alternatives: Mutex::default(),
            excluded: Mutex::default(),
            connections: Mutex::default(),
            tickets: Tickets::default(),
        }
    }

    fn excluded(&self, host: &str) -> bool {
        self.excluded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has(host, network)
    }

    /// Sends `origin`'s requests in `partition` to `alternative`, and
    /// only there.
    pub(crate) fn add(&self, partition: String, origin: String, alternative: String) {
        let alternative = Alternative {
            to: alternative,
            expires: Instant::now() + Duration::from_secs(365 * 86_400),
            validated: true,
            forced: true,
        };
        self.alternatives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((partition, origin), alternative);
    }

    /// Takes in an `Alt-Svc` header that came with a response from
    /// `origin` to a request in `partition`, as Firefox's
    /// AltSvcMapping::ProcessHeader and AltSvcCache::UpdateAltServiceMapping
    /// do: `clear` forgets the origin's alternative; the first `h3` one
    /// whose host HTTP/3 hasn't failed to reach is kept for its `ma=`
    /// seconds (a day without one), and a connection is made to it at
    /// once, which the origin's requests use once it is up.
    pub(crate) fn learn(self: &Arc<Self>, partition: &str, origin: &Url, header: &str) {
        let (Some(host), Some(port)) = (origin.host_str(), origin.port_or_known_default()) else {
            return;
        };
        let key = (partition.to_string(), origin.origin().ascii_serialization());
        let forget = || {
            let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
            if alternatives.get(&key).is_some_and(|a| !a.forced) {
                alternatives.remove(&key);
            }
        };
        let mut chosen = None;
        for entry in alt_svc(header) {
            let Some(((protocol, authority), parameters)) = entry.split_first() else {
                continue;
            };
            if protocol == "clear" {
                forget();
                continue;
            }
            if protocol != "h3" {
                continue;
            }
            // Firefox takes the port after the first colon, as atoi reads
            // it, and the host before it, the origin's if empty.
            let (to_host, to_port) = match authority.split_once(':') {
                Some((h, p)) => (h, leading_number(p)),
                None => ("", i64::from(port)),
            };
            let to_host = if to_host.is_empty() { host } else { to_host };
            // shortcut: NS_CheckPortSafety's list of banned TCP ports isn't
            // copied, only the range; copy it if a site ever points QUIC at one.
            let Ok(to_port) = u16::try_from(to_port) else {
                continue;
            };
            if to_port == 0 || self.excluded(to_host) {
                continue;
            }
            let max_age = parameters
                .iter()
                .rev()
                .find(|(k, _)| k == "ma")
                .map_or(firefox::ALT_SVC_MAX_AGE, |(_, v)| leading_number(v));
            if max_age <= 0 {
                // Firefox clears what was there, as accepting it would.
                forget();
                continue;
            }
            if chosen.is_none() {
                let max_age = Duration::from_secs(max_age.unsigned_abs());
                chosen = Some((format!("{to_host}:{to_port}"), max_age));
            }
        }
        let Some((to, max_age)) = chosen else {
            return;
        };
        let expires = Instant::now() + max_age;
        {
            let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
            match alternatives.get_mut(&key) {
                Some(known) if known.forced => return,
                Some(known) if known.validated && known.to == to => {
                    known.expires = expires;
                    return;
                }
                Some(known) if known.validated && expires < known.expires => return,
                // Its connection is still being made.
                Some(known) if !known.validated && Instant::now() < known.expires => return,
                // shortcut: a validated alternative that's being replaced
                // goes at once, where Firefox keeps it until the new one is
                // proved; upgrade if sites are seen moving theirs often.
                _ => {}
            }
            alternatives.insert(
                key,
                Alternative {
                    to: to.clone(),
                    expires,
                    validated: false,
                    forced: false,
                },
            );
        }
        let handle = self.connection(partition, host, &to);
        if *handle.state.borrow() == State::Connected {
            self.proved(partition, host, &to);
        }
    }

    /// The alternative of a request to `origin` in `partition`, if it has
    /// a proved one that hasn't expired, to a host HTTP/3 hasn't failed to
    /// reach (AltSvcCache::LookupMapping).
    pub(crate) fn alternative(&self, partition: &str, origin: &str) -> Option<Route> {
        let key = (partition.to_string(), origin.to_string());
        let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
        let known = alternatives.get(&key)?;
        if !known.forced && (Instant::now() >= known.expires || self.excluded(host_of(&known.to))) {
            alternatives.remove(&key);
            return None;
        }
        known.validated.then(|| Route {
            partition: partition.to_string(),
            alternative: known.to.clone(),
            forced: known.forced,
        })
    }

    /// A connection to `alternative` made up from `host`'s alternatives
    /// is up: they're proved.
    fn proved(&self, partition: &str, host: &str, alternative: &str) {
        let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
        for ((p, origin), known) in alternatives.iter_mut() {
            let same_host = Url::parse(origin)
                .ok()
                .is_some_and(|u| u.host_str() == Some(host));
            if p == partition && same_host && known.to == alternative {
                known.validated = true;
            }
        }
    }

    /// A connection to `alternative` never came up: its unproved
    /// alternatives go (AltSvcMappingValidator, on a failed validation),
    /// and so does HTTP/3 to its host if Firefox would exclude it.
    fn failed(&self, alternative: &str, exclude: bool) {
        if exclude {
            self.excluded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .add(host_of(alternative), network);
        }
        self.alternatives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, known| known.validated || known.to != alternative);
    }

    /// The connection to `alternative` for `host` in `partition`, opened
    /// if there's none that is up or coming up. A new one is watched until
    /// it's up or has failed.
    fn connection(self: &Arc<Self>, partition: &str, host: &str, alternative: &str) -> Handle {
        let key = format!("{partition} {host} {alternative}");
        let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = connections.get(&key)
            && handle.usable()
        {
            return handle.clone();
        }
        let handle = open(host, alternative, &key, self.trust.clone(), &self.tickets);
        connections.insert(key, handle.clone());
        let (routes, mut state) = (Arc::downgrade(self), handle.state.clone());
        let (partition, host, alternative) = (
            partition.to_string(),
            host.to_string(),
            alternative.to_string(),
        );
        tokio::spawn(async move {
            let outcome = state
                .wait_for(|s| *s != State::Connecting)
                .await
                .map_or(State::Failed { exclude: false }, |s| *s);
            let Some(routes) = Weak::upgrade(&routes) else {
                return;
            };
            match outcome {
                State::Connected => routes.proved(&partition, &host, &alternative),
                State::Failed { exclude } => routes.failed(&alternative, exclude),
                State::Connecting | State::Closed => {}
            }
        });
        handle
    }

    /// Closes every connection, waiting until each has closed and left
    /// its session ticket for the next.
    pub(crate) async fn close(&self) {
        let handles: Vec<Handle> = self
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, h)| h)
            .collect();
        for handle in handles {
            let thread = handle
                .thread
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            drop(handle);
            if let Some(thread) = thread {
                let _ = tokio::task::spawn_blocking(move || thread.join()).await;
            }
        }
    }

    /// Sends `request` to `alternative` for `host`, on the connection
    /// already up to it if there is one, as Firefox does. A connection
    /// still being made gets the backup timer's 100 ms to come up before
    /// the request goes over TCP instead; one that fails sends it there
    /// too, and so does one that closes under a request that is safe to
    /// repeat before any of its response came. A forced route waits for
    /// its connection, and fails with it.
    pub(crate) async fn send(
        self: &Arc<Self>,
        route: &Route,
        host: &str,
        request: Request,
        deadline: tokio::time::Instant,
        idempotent: bool,
    ) -> Result<Answer, FetchError> {
        let alternative = route.alternative.as_str();
        let handle = self.connection(&route.partition, host, alternative);
        let mut state = handle.state.clone();
        // shortcut: the request moves to TCP when the backup timer fires,
        // where Firefox opens a TCP connection then and moves it only once
        // that one is up, so an HTTP/3 connection that comes up in between
        // still carries it. wreq can't connect without sending.
        let up = route.forced
            || matches!(
                tokio::time::timeout(
                    firefox::BACKUP_TIMER,
                    state.wait_for(|s| *s != State::Connecting),
                )
                .await,
                Ok(Ok(s)) if *s == State::Connected
            );
        if !up {
            return Ok(Answer::Tcp);
        }
        let (head, answer) = oneshot::channel();
        let job = Job {
            request,
            head,
            deadline,
        };
        let closed = || {
            FetchError::new(
                FetchErrorKind::Connect,
                format!("the HTTP/3 connection to {alternative} closed"),
            )
        };
        // A request safe to repeat that got nothing back goes again over
        // TCP when the connection went under it (nsHttpTransaction::Close).
        let repeat = |e: FetchError| {
            if idempotent && !route.forced {
                Ok(Answer::Tcp)
            } else {
                Err(e)
            }
        };
        // Never sent: it can go anywhere.
        if handle.jobs.send(job).is_err() {
            return if route.forced {
                Err(closed())
            } else {
                Ok(Answer::Tcp)
            };
        }
        match answer.await {
            Ok(Ok(response)) => Ok(Answer::Response(response)),
            Ok(Err(e)) if *handle.state.borrow() == State::Connected => Err(e),
            Ok(Err(e)) => repeat(e),
            Err(_) => repeat(closed()),
        }
    }
}

/// An `Alt-Svc` value as Firefox's ParsedHeaderValueListList reads it:
/// alternatives split at commas, each into `name=value` pairs split at
/// semicolons, outside quotes, with the quotes taken off.
fn alt_svc(header: &str) -> Vec<Vec<(String, String)>> {
    let mut entries = vec![Vec::new()];
    let mut pair = (String::new(), String::new());
    let (mut quoted, mut in_value) = (false, false);
    let end_pair = |entry: &mut Vec<(String, String)>, pair: &mut (String, String)| {
        let (name, value) = std::mem::take(pair);
        let name = name.trim().to_string();
        if !name.is_empty() {
            entry.push((name, value.trim().to_string()));
        }
    };
    for c in header.chars() {
        let entry = entries.last_mut().expect("never empty");
        match c {
            '"' => quoted = !quoted,
            ',' | ';' if !quoted => {
                end_pair(entry, &mut pair);
                in_value = false;
                if c == ',' {
                    entries.push(Vec::new());
                }
            }
            '=' if !quoted && !in_value => in_value = true,
            _ if in_value => pair.1.push(c),
            _ => pair.0.push(c),
        }
    }
    end_pair(entries.last_mut().expect("never empty"), &mut pair);
    entries.retain(|e| !e.is_empty());
    entries
}

/// The number `s` starts with, as C's atoi reads it.
fn leading_number(s: &str) -> i64 {
    let s = s.trim_start();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    sign * digits[..end]
        .parse::<i64>()
        .unwrap_or(0)
        .min(i64::from(i32::MAX))
}

fn open(host: &str, alternative: &str, key: &str, trust: Trust, tickets: &Tickets) -> Handle {
    let (jobs, queue) = mpsc::unbounded_channel();
    let (state, watching) = watch::channel(State::Connecting);
    let (host, alternative) = (host.to_string(), alternative.to_string());
    let ticket = tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(key);
    let (key, tickets) = (key.to_string(), tickets.clone());
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
            let ticket = Ticket {
                used: ticket,
                key,
                tickets,
            };
            runtime.block_on(run(host, alternative, trust, ticket, queue, &state));
            // However it ended, it isn't coming up any more.
            state.send_modify(|s| {
                *s = match *s {
                    State::Connecting => State::Failed { exclude: true },
                    State::Connected => State::Closed,
                    other => other,
                }
            });
        });
    // A thread that never started took the queue and the state with it, so
    // every send fails and the caller tries another.
    Handle {
        jobs,
        thread: Arc::new(Mutex::new(spawned.ok())),
        state: watching,
    }
}

fn fail_all(mut queue: mpsc::UnboundedReceiver<Job>, why: &str) {
    queue.close();
    while let Ok(job) = queue.try_recv() {
        let _ = job.head.send(Err(FetchError::new(
            FetchErrorKind::Connect,
            why.to_string(),
        )));
    }
}

/// A UDP socket as Firefox's: quinn-udp underneath, as `neqo_udp` has it,
/// for the same ECN marking and batching.
struct Socket {
    state: quinn_udp::UdpSocketState,
    inner: tokio::net::UdpSocket,
    local: SocketAddr,
}

impl Socket {
    fn bind(remote: SocketAddr) -> io::Result<Socket> {
        let any = match remote {
            SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        };
        let socket = std::net::UdpSocket::bind(any)?;
        let state = quinn_udp::UdpSocketState::new((&socket).into())?;
        if state.recv_buffer_size((&socket).into())? < firefox::RECV_BUFFER {
            let _ = state.set_recv_buffer_size((&socket).into(), firefox::RECV_BUFFER);
        }
        socket.set_nonblocking(true)?;
        let inner = tokio::net::UdpSocket::from_std(socket)?;
        let local = inner.local_addr()?;
        Ok(Socket {
            state,
            inner,
            local,
        })
    }

    async fn send(&self, batch: &datagram::Batch) -> io::Result<()> {
        loop {
            match self.inner.try_io(tokio::io::Interest::WRITABLE, || {
                neqo_udp::send_inner(&self.state, (&self.inner).into(), batch)
            }) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.inner.writable().await?,
                done => return done,
            }
        }
    }
}

/// A request in flight, by its stream.
struct Stream {
    head: Option<oneshot::Sender<Result<http::Response<Body>, FetchError>>>,
    deadline: tokio::time::Instant,
    body: Option<mpsc::UnboundedSender<Chunk>>,
    /// Request body neqo hasn't taken yet.
    unsent: Bytes,
}

/// The ticket a connection resumes with, and where it leaves the next.
struct Ticket {
    used: Option<ResumptionToken>,
    key: String,
    tickets: Tickets,
}

async fn run(
    host: String,
    alternative: String,
    trust: Trust,
    ticket: Ticket,
    mut queue: mpsc::UnboundedReceiver<Job>,
    state: &watch::Sender<State>,
) {
    let connect = || -> Result<(Socket, Http3Client), String> {
        let remote = alternative
            .to_socket_addrs()
            .map_err(|e| format!("{alternative}: {e}"))?
            .next()
            .ok_or_else(|| format!("{alternative}: no address"))?;
        let socket = Socket::bind(remote).map_err(|e| e.to_string())?;
        let client = firefox_client(&host, socket.local, remote, ticket.used.as_ref())?;
        Ok((socket, client))
    };
    let (socket, mut client) = match connect() {
        Ok(c) => c,
        Err(e) => return fail_all(queue, &format!("HTTP/3 to {alternative}: {e}")),
    };
    let mut recv = neqo_udp::RecvBuf::default();
    let mut streams: HashMap<StreamId, Stream> = HashMap::new();
    let mut waiting: Vec<Job> = Vec::new();
    // Known limit: a resumed connection whose ticket allows early data
    // waits for the handshake before sending, where Firefox sends a GET
    // or HEAD in 0-RTT.
    let mut ready = false;
    let mut accepting = true;
    let mut closing = false;
    let mut buf = vec![0; 64 << 10];
    loop {
        // What happened, then what to send.
        while let Some(event) = client.next_event() {
            match event {
                Http3ClientEvent::AuthenticationNeeded => {
                    let chain: Vec<Vec<u8>> = client
                        .peer_certificate()
                        .map(|c| c.iter().map(<[u8]>::to_vec).collect())
                        .unwrap_or_default();
                    client.authenticated(trust.check(&host, &chain), Instant::now());
                }
                Http3ClientEvent::StateChange(Http3State::Connected) => {
                    ready = true;
                    state.send_replace(State::Connected);
                }
                Http3ClientEvent::ResumptionToken(token) => {
                    ticket
                        .tickets
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(ticket.key.clone(), token);
                }
                Http3ClientEvent::StateChange(
                    Http3State::Closing(reason) | Http3State::Closed(reason),
                ) => {
                    let why = format!("HTTP/3 connection to {alternative} closed: {reason:?}");
                    let kind = if ready {
                        FetchErrorKind::Body
                    } else if format!("{reason:?}").contains("Crypto") {
                        FetchErrorKind::Tls
                    } else {
                        FetchErrorKind::Connect
                    };
                    state.send_replace(if ready {
                        State::Closed
                    } else {
                        State::Failed {
                            exclude: kind != FetchErrorKind::Tls,
                        }
                    });
                    for (_, s) in streams.drain() {
                        end(s, FetchError::new(kind, why.clone()));
                    }
                    for job in waiting.drain(..) {
                        let _ = job.head.send(Err(FetchError::new(kind, why.clone())));
                    }
                    return fail_all(queue, &why);
                }
                Http3ClientEvent::HeaderReady {
                    stream_id,
                    headers,
                    interim,
                    fin,
                } => {
                    if interim {
                        continue;
                    }
                    if let Some(s) = streams.get_mut(&stream_id) {
                        let (tx, rx) = mpsc::unbounded_channel();
                        let body = Body {
                            chunks: rx,
                            deadline: Box::pin(tokio::time::sleep_until(s.deadline)),
                        };
                        let response = response(&headers, body);
                        if let Some(head) = s.head.take() {
                            let _ = head.send(response);
                        }
                        s.body = Some(tx);
                        if fin {
                            streams.remove(&stream_id);
                        }
                    }
                }
                Http3ClientEvent::DataReadable { stream_id } => {
                    while let Some(s) = streams.get_mut(&stream_id) {
                        match client.read_data(Instant::now(), stream_id, &mut buf) {
                            Ok((n, fin)) => {
                                if n > 0
                                    && let Some(body) = &s.body
                                {
                                    let _ = body.send(Ok(Bytes::copy_from_slice(&buf[..n])));
                                }
                                if fin {
                                    streams.remove(&stream_id);
                                    break;
                                }
                                if n == 0 {
                                    break;
                                }
                            }
                            Err(e) => {
                                if let Some(s) = streams.remove(&stream_id) {
                                    end(s, FetchError::new(FetchErrorKind::Body, format!("{e:?}")));
                                }
                                break;
                            }
                        }
                    }
                }
                Http3ClientEvent::DataWritable { stream_id } => {
                    if let Some(s) = streams.get_mut(&stream_id) {
                        push_body(&mut client, stream_id, s);
                    }
                }
                Http3ClientEvent::Reset {
                    stream_id, error, ..
                }
                | Http3ClientEvent::StopSending { stream_id, error } => {
                    if let Some(s) = streams.remove(&stream_id) {
                        end(
                            s,
                            FetchError::new(
                                FetchErrorKind::Body,
                                format!("the server reset the HTTP/3 stream (error {error:#x})"),
                            ),
                        );
                    }
                }
                _ => {}
            }
        }
        if ready {
            for job in std::mem::take(&mut waiting) {
                start(&mut client, &mut streams, job);
            }
        }
        // A request whose caller has gone (a timeout, a body nobody reads
        // to the end) is cancelled, as Firefox cancels one, with
        // H3_REQUEST_CANCELLED.
        streams.retain(|&id, s| {
            let gone = s.head.as_ref().is_some_and(oneshot::Sender::is_closed)
                || s.body
                    .as_ref()
                    .is_some_and(mpsc::UnboundedSender::is_closed);
            if gone {
                let _ = client.cancel_fetch(id, 0x10c);
            }
            !gone
        });
        // The caller went and nothing is in flight: close as Firefox does
        // a connection it no longer needs, with H3_NO_ERROR.
        if !accepting && !closing && streams.is_empty() && waiting.is_empty() {
            closing = true;
            // A ticket neqo was still holding back for a NEW_TOKEN frame.
            if let Some(token) = client.take_resumption_token(Instant::now()) {
                ticket
                    .tickets
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(ticket.key.clone(), token);
            }
            client.close(Instant::now(), 0x100, "");
        }
        let timeout = loop {
            let max =
                NonZeroUsize::new(socket.state.max_gso_segments()).unwrap_or(NonZeroUsize::MIN);
            match client.process_multiple_output(Instant::now(), max) {
                OutputBatch::DatagramBatch(batch) => {
                    if let Err(e) = socket.send(&batch).await {
                        let why = format!("HTTP/3 to {alternative}: {e}");
                        state.send_replace(if ready {
                            State::Closed
                        } else {
                            State::Failed { exclude: true }
                        });
                        for (_, s) in streams.drain() {
                            end(s, FetchError::new(FetchErrorKind::Connect, why.clone()));
                        }
                        for job in waiting.drain(..) {
                            let _ = job
                                .head
                                .send(Err(FetchError::new(FetchErrorKind::Connect, why.clone())));
                        }
                        return fail_all(queue, &why);
                    }
                }
                OutputBatch::Callback(t) => break Some(t),
                OutputBatch::None => break None,
            }
        };
        if client.has_events() {
            continue;
        }
        if !accepting
            && matches!(
                client.state(),
                Http3State::Closing(_) | Http3State::Closed(_)
            )
        {
            return;
        }
        let sleep = tokio::time::sleep(timeout.unwrap_or(Duration::from_secs(3600)));
        tokio::select! {
            readable = socket.inner.readable() => {
                if readable.is_ok() {
                    loop {
                        let got = socket.inner.try_io(tokio::io::Interest::READABLE, || {
                            neqo_udp::recv_inner(socket.local, &socket.state, &socket.inner, &mut recv)
                                .map(|d| client.process_multiple_input(d, Instant::now()))
                        });
                        if got.is_err() {
                            break;
                        }
                    }
                }
            }
            () = sleep => {}
            job = queue.recv(), if accepting => match job {
                Some(job) if ready => start(&mut client, &mut streams, job),
                Some(job) => waiting.push(job),
                None => accepting = false,
            },
        }
    }
}

fn start(client: &mut Http3Client, streams: &mut HashMap<StreamId, Stream>, job: Job) {
    let Job {
        request,
        head,
        deadline,
    } = job;
    let headers: Vec<Header> = request
        .headers
        .iter()
        .map(|(k, v)| Header::new(k.as_str(), v.as_slice()))
        .collect();
    let now = Instant::now();
    let stream_id = match client.fetch(
        now,
        &request.method,
        ("https", request.authority.as_str(), request.path.as_str()),
        &headers,
        priority(&request.headers),
    ) {
        Ok(id) => id,
        Err(e) => {
            let _ = head.send(Err(FetchError::new(
                FetchErrorKind::Other,
                format!("HTTP/3 request: {e:?}"),
            )));
            return;
        }
    };
    let mut stream = Stream {
        head: Some(head),
        deadline,
        body: None,
        unsent: request.body.unwrap_or_default(),
    };
    push_body(client, stream_id, &mut stream);
    streams.insert(stream_id, stream);
}

/// Hands neqo as much of the request body as it takes, and ends the
/// request once all of it is in.
fn push_body(client: &mut Http3Client, stream_id: StreamId, stream: &mut Stream) {
    let now = Instant::now();
    while !stream.unsent.is_empty() {
        match client.send_data(stream_id, &stream.unsent, now) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let _ = stream.unsent.split_to(n);
            }
        }
    }
    let _ = client.stream_close_send(stream_id, now);
}

fn end(stream: Stream, error: FetchError) {
    if let Some(head) = stream.head {
        let _ = head.send(Err(error));
    } else if let Some(body) = stream.body {
        let _ = body.send(Err(io::Error::other(error.message)));
    }
}

fn response(headers: &[Header], body: Body) -> Result<http::Response<Body>, FetchError> {
    let bad = || FetchError::new(FetchErrorKind::Other, "a bad HTTP/3 response head");
    let status = headers
        .iter()
        .find(|h| h.name() == ":status")
        .and_then(|h| h.value_utf8().ok()?.parse::<u16>().ok())
        .ok_or_else(bad)?;
    let mut builder = http::Response::builder()
        .status(status)
        .version(http::Version::HTTP_3);
    for h in headers.iter().filter(|h| !h.name().starts_with(':')) {
        builder = builder.header(h.name(), h.value());
    }
    builder.body(body).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use bytes::Bytes;
    use http_body_util::BodyExt;
    use tokio::sync::mpsc;

    use super::{Body, Excluded, alt_svc, decompressed, leading_number, priority};

    #[test]
    fn alt_svc_is_read_as_firefox_reads_it() {
        let pairs = |v: &[&[(&str, &str)]]| -> Vec<Vec<(String, String)>> {
            v.iter()
                .map(|e| {
                    e.iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect()
                })
                .collect()
        };
        assert_eq!(
            alt_svc(r#"h3=":443"; ma=86400, h2="alt.example:8443";ma=60"#),
            pairs(&[
                &[("h3", ":443"), ("ma", "86400")],
                &[("h2", "alt.example:8443"), ("ma", "60")]
            ])
        );
        assert_eq!(alt_svc("clear"), pairs(&[&[("clear", "")]]));
        assert_eq!(alt_svc(r#"h3="a,b;c:1""#), pairs(&[&[("h3", "a,b;c:1")]]));
        assert_eq!(alt_svc(" , "), pairs(&[]));
        // atoi's reading of a port or a max-age.
        assert_eq!(
            ["86400", "12abc", "-5", "x", ""].map(leading_number),
            [86400, 12, -5, 0, 0]
        );
    }

    #[test]
    fn hosts_http3_failed_to_reach_stay_excluded_until_the_network_changes() {
        let home = (Some(std::net::IpAddr::from([192, 168, 1, 2])), None);
        let away = (Some(std::net::IpAddr::from([10, 0, 0, 2])), None);
        let mut excluded = Excluded::default();
        excluded.add("example.com", || home);
        assert!(excluded.has("example.com", || home));
        assert!(!excluded.has("example.org", || home));
        // Another network: every exclusion goes, as in Firefox.
        assert!(!excluded.has("example.com", || away));
        assert!(!excluded.has("example.com", || home));
    }

    #[test]
    fn the_priority_header_gives_neqo_its_urgency_and_increment() {
        let of = |v: &str| priority(&[("priority".into(), v.as_bytes().to_vec())]);
        assert_eq!(of("u=0, i"), neqo_http3::Priority::new(0, true));
        assert_eq!(of("u=6"), neqo_http3::Priority::new(6, false));
        assert_eq!(of("i"), neqo_http3::Priority::new(3, true));
        assert_eq!(priority(&[]), neqo_http3::Priority::default());
    }

    #[tokio::test]
    async fn a_compressed_body_is_decoded_as_over_tcp() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"hello over HTTP/3").unwrap();
        let (tx, chunks) = mpsc::unbounded_channel();
        tx.send(Ok(Bytes::from(gz.finish().unwrap()))).unwrap();
        drop(tx);
        let body = Body {
            chunks,
            deadline: Box::pin(tokio::time::sleep(std::time::Duration::from_secs(5))),
        };
        let response = http::Response::builder()
            .header("content-encoding", "gzip")
            .body(body)
            .unwrap();
        let accept = http::HeaderValue::from_static("gzip, deflate, br, zstd");
        let response = decompressed(response, Some(accept)).await;
        let body = wreq::Body::from(response).collect().await.unwrap();
        assert_eq!(&body.to_bytes()[..], b"hello over HTTP/3");
    }
}
