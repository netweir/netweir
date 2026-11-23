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
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
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
use tokio::sync::{mpsc, oneshot};

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

/// An open HTTP/3 connection, or one opening.
#[derive(Clone)]
struct Handle {
    jobs: mpsc::UnboundedSender<Job>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

/// Where each origin's requests go over HTTP/3, and the connections to
/// them, shared by every clone of a Fetcher.
pub(crate) struct Routes {
    trust: Trust,
    /// origin (`https://host:port`) to the alternative (`host:port`) its
    /// requests go to.
    alternatives: Mutex<HashMap<String, String>>,
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
            connections: Mutex::default(),
            tickets: Tickets::default(),
        }
    }

    pub(crate) fn add(&self, origin: String, alternative: String) {
        self.alternatives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(origin, alternative);
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

    pub(crate) fn alternative(&self, origin: &str) -> Option<String> {
        self.alternatives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(origin)
            .cloned()
    }

    /// Sends `request` to `alternative` for `host`, on the connection
    /// already open to it if there is one, as Firefox does.
    pub(crate) async fn send(
        &self,
        host: &str,
        alternative: &str,
        request: Request,
        deadline: tokio::time::Instant,
    ) -> Result<http::Response<Body>, FetchError> {
        let key = format!("{host} {alternative}");
        let (head, answer) = oneshot::channel();
        let mut job = Job {
            request,
            head,
            deadline,
        };
        for _ in 0..2 {
            let handle = {
                let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
                connections
                    .entry(key.clone())
                    .or_insert_with(|| {
                        open(host, alternative, &key, self.trust.clone(), &self.tickets)
                    })
                    .clone()
            };
            match handle.jobs.send(job) {
                Ok(()) => {
                    return answer.await.unwrap_or_else(|_| {
                        Err(FetchError::new(
                            FetchErrorKind::Connect,
                            format!("the HTTP/3 connection to {alternative} closed"),
                        ))
                    });
                }
                // That connection has closed since: open another.
                Err(mpsc::error::SendError(back)) => {
                    job = back;
                    self.connections
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&key);
                }
            }
        }
        Err(FetchError::new(
            FetchErrorKind::Connect,
            format!("couldn't open an HTTP/3 connection to {alternative}"),
        ))
    }
}

fn open(host: &str, alternative: &str, key: &str, trust: Trust, tickets: &Tickets) -> Handle {
    let (jobs, queue) = mpsc::unbounded_channel();
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
                Err(e) => return fail_all(queue, &format!("{e}")),
            };
            let ticket = Ticket {
                used: ticket,
                key,
                tickets,
            };
            runtime.block_on(run(host, alternative, trust, ticket, queue));
        });
    // A thread that never started took the queue with it, so every send
    // fails and the caller tries another.
    Handle {
        jobs,
        thread: Arc::new(Mutex::new(spawned.ok())),
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
                Http3ClientEvent::StateChange(Http3State::Connected) => ready = true,
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

    use super::{Body, decompressed, priority};

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
