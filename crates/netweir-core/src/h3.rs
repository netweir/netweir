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
/// The longest a Safari request waits for its origin's HTTPS records
/// before it goes over TCP without them. A guess: the lookup runs beside
/// the address lookup a connection waits for anyway, and Apple gives no
/// limit.
pub(crate) const RECORD_WAIT: Duration = Duration::from_secs(2);

/// What Safari 27.2 was seen doing on macOS, against the fingerprint
/// server: Apple documents neither number.
mod safari {
    use std::time::Duration;

    /// How long TCP waits for a QUIC connection still being made: Safari
    /// started TCP 250 ms after its first QUIC Initial each time it was
    /// timed, the Connection Attempt Delay Happy Eyeballs v3 (an Apple
    /// draft) recommends.
    pub const RACE_DELAY: Duration = Duration::from_millis(250);
    /// How long an origin's TCP connection is taken to stay open after
    /// its last response: wreq's idle pool timeout. Safari kept sending
    /// over an open HTTP/2 connection after learning an alternative, and
    /// moved to HTTP/3 only for a new connection.
    pub const TCP_KEPT: Duration = Duration::from_secs(90);
}

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
    /// network.dns.httpssvc.http3_fast_fallback_timeout: the same for a
    /// connection made from an HTTPS record.
    pub const FAST_FALLBACK: std::time::Duration = std::time::Duration::from_millis(50);
    /// network.dnsCacheExpirationGracePeriod: how long past its TTL Firefox
    /// still uses a record it has (nsHostRecord::SetExpiration).
    pub const DNS_GRACE: std::time::Duration = std::time::Duration::from_secs(600);
    /// network.dns.negative_ttl_for_type_record: how long a lookup that
    /// found no HTTPS record stands.
    pub const DNS_NEGATIVE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
    /// An Alt-Svc alternative's lifetime without `ma=`, in seconds
    /// (AltSvcMapping::ProcessHeader).
    pub const ALT_SVC_MAX_AGE: i64 = 86_400;
    /// How long an alternative whose connection failed holds off another
    /// offer of it (AltSvcMappingValidator::OnTransactionDestroy).
    pub const ALT_SVC_RETRY: std::time::Duration = std::time::Duration::from_secs(2);
    /// How long a connection may stay in 0-RTT before it fails
    /// (network.http.http3.0rtt_timeout).
    pub const ZERO_RTT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(5000);
    /// The ports Firefox never connects to, an alternative's included
    /// (gBadPortList in netwerk/base/nsIOService.cpp, which
    /// NS_CheckPortSafety reads; nsHttpHandler::AllowPort overrides none,
    /// and network.security.ports.banned adds none by default).
    pub(crate) const BANNED_PORTS: [u16; 82] = [
        1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101,
        102, 103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389,
        427, 465, 512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636,
        989, 990, 993, 995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665,
        6666, 6667, 6668, 6669, 6679, 6697, 10080,
    ];
}

/// Whether Firefox refuses to connect to `port` (NS_CheckPortSafety).
pub(crate) fn banned_port(port: u16) -> bool {
    port == 0 || firefox::BANNED_PORTS.contains(&port)
}

/// The connection settings Firefox 160 makes, line for line in the order
/// `NeqoHttp3Conn::new` makes them.
fn firefox_client(
    server_name: &str,
    local: SocketAddr,
    remote: SocketAddr,
    resume: Option<&ResumptionToken>,
    ech: Option<&[u8]>,
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
    // Http3Session::Init: the ECH configuration of the HTTPS record the
    // connection came from (DoSetEchConfig); with none,
    // security.tls.ech.grease_http3 (true) and grease_probability (100)
    // make every connection GREASE ECH, which an empty configuration does.
    client
        .enable_ech(ech.unwrap_or_default())
        .map_err(|e| format!("{e:?}"))?;
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
    /// The store a chain is checked against, the roots the profile's
    /// browser builds in (DER), and, for Mozilla's, the distrust-after
    /// dates given beside NSS's own.
    Roots(
        Arc<X509Store>,
        Arc<std::collections::HashSet<Vec<u8>>>,
        Option<Arc<crate::roots::DistrustAfter>>,
    ),
    Anything,
}

impl Trust {
    /// Checks against `pem`'s CAs if given, else `roots`, the profile's
    /// browser's own, with `extra` (DER) counted among them and, for
    /// Mozilla's, `distrust` dates beside NSS's.
    pub(crate) fn new(
        verify: bool,
        pem: Option<&[u8]>,
        roots: crate::profile::Roots,
        extra: &[Vec<u8>],
        distrust: &crate::roots::DistrustAfter,
    ) -> Result<Trust, FetchError> {
        if !verify {
            return Ok(Trust::Anything);
        }
        let bad = |e: btls::error::ErrorStack| FetchError::invalid(format!("CA certificates: {e}"));
        let built_in: std::collections::HashSet<Vec<u8>> = crate::roots::bundled(roots)
            .map(<[u8]>::to_vec)
            .chain(extra.iter().cloned())
            .collect();
        let mut store = X509StoreBuilder::new().map_err(bad)?;
        match pem {
            Some(pem) => {
                for cert in X509::stack_from_pem(pem).map_err(bad)? {
                    store.add_cert(cert).map_err(bad)?;
                }
            }
            None => {
                for der in &built_in {
                    // A root BoringSSL can't read is one no connection
                    // could be checked against anyway.
                    if let Ok(cert) = X509::from_der(der) {
                        let _ = store.add_cert(cert);
                    }
                }
            }
        }
        let dates =
            (roots == crate::profile::Roots::Mozilla && pem.is_none()).then(|| Arc::from(distrust));
        Ok(Trust::Roots(
            Arc::new(store.build()),
            Arc::new(built_in),
            dates,
        ))
    }

    /// Whether `chain` (leaf first, DER) is good for `host`, and whether
    /// the root it ends at is a built-in one (`Trust::verify`).
    fn check(&self, host: &str, chain: &[Vec<u8>]) -> (AuthenticationStatus, bool) {
        match self.verify(host, chain) {
            Some(built_in) => (AuthenticationStatus::Ok, built_in),
            None => (AuthenticationStatus::CertUntrusted, true),
        }
    }

    /// Whether `chain` (leaf first, DER) is good for `host`, checked with
    /// BoringSSL against the roots the TCP connections use.
    pub(crate) fn trusts(&self, host: &str, chain: &[Vec<u8>]) -> bool {
        self.verify(host, chain).is_some()
    }

    /// None if `chain` isn't good for `host`; else whether the root it
    /// ends at is one of the roots the profile's browser builds in, as
    /// Firefox asks of Mozilla's
    /// (NSSSocketControl::IsBuiltCertChainRootBuiltInRoot): a chain to a
    /// CA the fetch was told to trust that isn't also one of them ends at
    /// a third-party root.
    fn verify(&self, host: &str, chain: &[Vec<u8>]) -> Option<bool> {
        let Trust::Roots(store, built_ins, dates) = self else {
            return Some(true);
        };
        let mut built_in = true;
        let mut check = || -> Result<bool, btls::error::ErrorStack> {
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
                let mut ok = ctx.verify_cert()?;
                // The dates only add a refusal to BoringSSL's verdict.
                if let (Some(dates), Some(chain)) = (dates, ctx.chain()) {
                    ok = ok && !crate::roots::distrusted(chain, dates);
                }
                if let Some(root) = ctx.chain().and_then(|c| c.iter().next_back()) {
                    built_in = built_ins.contains(&root.to_der()?);
                }
                Ok(ok)
            })
        };
        let ok = matches!(check(), Ok(true));
        ok.then_some(built_in)
    }
}

/// One request, its headers as Firefox hands them to neqo: in order,
/// lowercase, with no pseudo-headers (neqo writes those) and none that
/// HTTP/3 forbids.
#[derive(Clone)]
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

pub(crate) type Chunk = Result<Bytes, io::Error>;

/// A response body as it arrives on its stream, until the request's
/// deadline.
pub(crate) struct Body {
    chunks: mpsc::UnboundedReceiver<Chunk>,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl Body {
    /// A body whose chunks come from `chunks`, until `deadline`.
    pub(crate) fn new(
        chunks: mpsc::UnboundedReceiver<Chunk>,
        deadline: tokio::time::Instant,
    ) -> Body {
        Body {
            chunks,
            deadline: Box::pin(tokio::time::sleep_until(deadline)),
        }
    }
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
pub(crate) struct Job {
    pub request: Request,
    pub head: oneshot::Sender<Result<http::Response<Body>, FetchError>>,
    pub deadline: tokio::time::Instant,
}

/// How far an HTTP/3 connection has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Connecting,
    /// Resumed with a ticket that allows early data, its handshake not
    /// done yet: it takes requests, and sends those safe to repeat in
    /// 0-RTT (Firefox's ZERORTT, which Http3Session::Init reports to the
    /// connection manager as ready).
    ZeroRtt,
    Connected,
    /// It never connected. Firefox stops using HTTP/3 to the alternative's
    /// host after any such failure but a TLS one (Http3Session::Shutdown).
    Failed {
        exclude: bool,
    },
    /// Its socket or client couldn't be set up, before any packet went
    /// out: HTTP/3 to the alternative's host stops, but the request may
    /// still go on to another h3 record (DnsAndConnectSocket::SetupConn
    /// calls DisableHttp3(true) and ExcludeHttp3).
    NotSetUp,
    /// It connected, and has closed since.
    Closed,
    /// It connected, then closed with an HTTP/3 or QUIC error, which
    /// stops HTTP/3 to the alternative's host as a failure to connect
    /// does (Http3Session::ProcessEvents sets
    /// NS_ERROR_NET_HTTP3_PROTOCOL_ERROR on ConnectionClosing, and
    /// Http3Session::Shutdown excludes the host for it).
    Broken,
}

/// An open HTTP/3 connection, or one opening.
#[derive(Clone)]
pub(crate) struct Handle {
    pub jobs: mpsc::UnboundedSender<Job>,
    pub thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    pub state: watch::Receiver<State>,
    /// The ECH configurations a server that rejected the connection's
    /// sent to retry with (Http3Session's SetRetryEchConfig).
    pub ech_retry: Arc<Mutex<Option<Vec<u8>>>>,
    /// Its certificate chain ended at a root Firefox doesn't build in,
    /// which ends HTTP/3 before anything is sent: a request waiting for
    /// it goes over TCP, whatever its method (Http3Session::Authenticated
    /// closes its transaction with NS_ERROR_NET_RESET).
    pub third_party: Arc<std::sync::atomic::AtomicBool>,
}

impl Handle {
    /// Up, or resumed and in 0-RTT: a session a request can go on.
    pub(crate) fn taking_requests(&self) -> bool {
        matches!(*self.state.borrow(), State::Connected | State::ZeroRtt)
    }

    pub(crate) fn usable(&self) -> bool {
        !self.jobs.is_closed()
            && matches!(
                *self.state.borrow(),
                State::Connecting | State::ZeroRtt | State::Connected
            )
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
    /// A newer alternative being proved, `host:port` and its expiry. This
    /// one stays in use until that one's connection is up, as Firefox
    /// syncs a replacement only once it validates
    /// (AltSvcCache::UpdateAltServiceMapping, SetSyncOnlyOnSuccess).
    next: Option<(String, Instant)>,
}

/// Where a request goes over HTTP/3, or over HTTP/2 to an `h2`
/// alternative.
pub(crate) struct Route {
    /// The request's network partition.
    pub partition: String,
    /// `host:port`.
    pub alternative: String,
    pub forced: bool,
    /// HTTP/3, not an `h2` alternative.
    pub h3: bool,
    /// From an HTTPS record, with its ECH configuration if it has one:
    /// used without proving it, and without Alt-Used.
    pub record: Option<Option<Vec<u8>>>,
    /// The records a route from an HTTPS record came from, for where its
    /// TCP connection goes (Firefox's routes only).
    pub from: Option<Arc<FromRecords>>,
}

/// The HTTPS records a request's route came from, and its origin: where
/// its TCP connections go when they don't go to the origin.
pub(crate) struct FromRecords {
    found: Found,
    host: String,
    port: u16,
    /// For a request over TCP, the record it goes to
    /// (nsHttpConnectionInfo::CloneAndAdoptHTTPSSVCRecord); None when the
    /// record sends it to the origin as it was.
    pub tcp: Option<crate::https_rr::Route>,
    /// The records still to go on to, once a failed connection has made
    /// the list (nsHttpTransaction::mRecordsForRetry): it's made once.
    left: Option<Vec<crate::https_rr::Route>>,
    /// The connections the request has made so far, which
    /// network.http.request.max-attempts bounds for the whole request.
    pub attempts: usize,
}

impl FromRecords {
    /// Where a fast fallback's TCP connection goes, for an HTTP/3 route
    /// that `ech_used` or not (nsHttpTransaction::PrepareFastFallbackConnInfo):
    /// the best record without h3, which has to have ECH if the HTTP/3 one
    /// did. `Some(None)` is the origin; None is no fallback connection at
    /// all, when ECH was used and no record without h3 has it.
    pub(crate) fn fast_fallback(&self, ech_used: bool) -> Option<Option<crate::https_rr::Route>> {
        let records = self.found.records.as_ref()?;
        let tcp = crate::https_rr::choose_tcp(
            records,
            &self.host,
            self.port,
            self.found.cname.as_deref(),
        )
        .filter(|r| !ech_used || r.ech.is_some());
        match tcp {
            Some(r) => Some(Some(r)),
            None if ech_used => None,
            None => Some(None),
        }
    }

    /// The records a request goes on to after a connection with ECH
    /// failed, and whether every record has ECH
    /// (nsHttpTransaction::PrepareSVCBRecordsForRetry): those after the
    /// failed one, in order, but none `skipped_after` it, and no h3 one if HTTP/3 has failed for the request (`no_h3`)
    /// or is excluded from its target. Once made, the list is what's left
    /// of it.
    pub(crate) fn retries(
        &self,
        failed: &crate::https_rr::Route,
        no_h3: bool,
        h3_excluded: &dyn Fn(&str) -> bool,
    ) -> (bool, Vec<crate::https_rr::Route>) {
        if let Some(left) = &self.left {
            return (true, left.clone());
        }
        let Some(records) = self.found.records.as_ref() else {
            return (false, Vec::new());
        };
        let (all, mut list) = crate::https_rr::all_with_ech(
            records,
            &self.host,
            self.port,
            self.found.cname.as_deref(),
            no_h3,
            h3_excluded,
        );
        list.retain(|r| !crate::https_rr::skipped_after(failed, r, &self.host, self.port));
        (all, list)
    }

    /// Whether `to` keeps the origin's host and port (`https_rr::keeps_origin`).
    pub(crate) fn keeps_origin(&self, to: &crate::https_rr::Route) -> bool {
        crate::https_rr::keeps_origin(to, &self.host, self.port)
    }

    /// The HTTP/3 route to `to`, an h3 record a request goes on to with
    /// `left` still to go, if the list has been made
    /// (CloneAndAdoptHTTPSSVCRecord on a retry).
    pub(crate) fn h3_retry(
        &self,
        partition: &str,
        to: crate::https_rr::Route,
        left: Option<Vec<crate::https_rr::Route>>,
        attempts: usize,
    ) -> Route {
        Route {
            partition: partition.to_string(),
            alternative: format!("{}:{}", to.host, to.port),
            forced: false,
            h3: true,
            record: Some(to.ech),
            from: Some(Arc::new(FromRecords {
                found: self.found.clone(),
                host: self.host.clone(),
                port: self.port,
                tcp: None,
                left,
                attempts,
            })),
        }
    }

    /// These records once the request has made `attempts` connections.
    pub(crate) fn after(&self, attempts: usize) -> FromRecords {
        FromRecords {
            found: self.found.clone(),
            host: self.host.clone(),
            port: self.port,
            tcp: self.tcp.clone(),
            left: self.left.clone(),
            attempts,
        }
    }
}

/// What an origin's HTTPS records came to, once its lookup is done: the
/// records and the canonical name, if any, and when it stops standing.
#[derive(Clone)]
pub(crate) struct Found {
    records: Option<crate::https_rr::Records>,
    cname: Option<String>,
    expires: Instant,
}

/// An origin's HTTPS record lookup, done or under way.
pub(crate) struct Lookup {
    found: watch::Receiver<Option<Found>>,
    origin: Url,
    partition: String,
}

impl Lookup {
    /// Waits for the lookup, and says whether it found any record.
    pub(crate) async fn has_records(mut self) -> bool {
        let Ok(found) = self.found.wait_for(Option::is_some).await else {
            return false;
        };
        found
            .as_ref()
            .and_then(|f| f.records.as_ref())
            .is_some_and(|r| !r.records.is_empty())
    }
}

/// An origin's alternatives are kept by partition, origin and whether
/// they're HTTP/3 ones, as AltSvcMapping::MakeHashKey keys them.
type Key = (String, String, bool);

/// Proves an `h2` alternative (`host:port`) for an origin: a TCP and TLS
/// connection to it, with a certificate for the origin
/// (AltSvcTransaction::MaybeValidate). Given by the fetcher, whose TCP
/// connections these are.
pub(crate) type Validate =
    Arc<dyn Fn(Url, String) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

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

/// `known` once proving `to` for it has ended: proved, it's in use, in
/// place of the one it was to replace if any; not, it stays unproved and
/// holds off another offer of it for two seconds, or the one it was to
/// replace stays.
fn settled(known: &mut Alternative, to: &str, ok: bool) {
    if known.to == to && !known.validated {
        if ok {
            known.validated = true;
        } else {
            known.expires = Instant::now() + firefox::ALT_SVC_RETRY;
        }
    }
    if let Some((_, expires)) = known.next.take_if(|(next, _)| next == to)
        && ok
    {
        (known.to, known.expires) = (to.to_string(), expires);
    }
}

/// The host part of an alternative's `host:port`.
pub(crate) fn host_of(alternative: &str) -> &str {
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
    /// The connection was still being made when the backup timer fired:
    /// the request goes to whichever comes up first, it or a TCP
    /// connection opened now (nsHttpTransaction::OnHttp3BackupTimer,
    /// OnBackupConnectionReady).
    Race(Pending),
}

/// A request waiting for an HTTP/3 connection still being made, or sent
/// on one still in 0-RTT, whose answer it then waits for.
pub(crate) struct Pending {
    request: Option<Request>,
    answer: Option<oneshot::Receiver<Result<http::Response<Body>, FetchError>>>,
    state: watch::Receiver<State>,
    /// The connection's Handle::third_party.
    third_party: Arc<std::sync::atomic::AtomicBool>,
}

impl Pending {
    /// Waits for the connection: whether it came up, or failed. One that a
    /// request went on in 0-RTT is up once its handshake is done.
    pub(crate) async fn up(&mut self) -> bool {
        let sent = self.answer.is_some();
        matches!(
            self.state
                .wait_for(|s| match s {
                    State::Connecting => false,
                    State::ZeroRtt => !sent,
                    _ => true,
                })
                .await,
            Ok(s) if matches!(*s, State::Connected | State::ZeroRtt)
        )
    }
}

/// Whose QUIC and HTTP/3 a Fetcher sends, and whose rules it follows to
/// move to it and off it.
pub(crate) enum Stack {
    /// Firefox 160: neqo and NSS (this file).
    Firefox,
    /// Chrome 157: netweir's quiche on btls (src/h3_chrome.rs), with the
    /// profile's TLS settings.
    Chrome(Arc<crate::profile::Tls>),
    /// Safari 27: the same engine with Safari's settings, and the
    /// profile's TLS settings with its `[http3]` extension order. Safari
    /// follows Chrome's rules here but where `Routes` says otherwise.
    Safari(Arc<crate::profile::Tls>),
}

/// What a connection to `alternative` for `host` in `partition`, with
/// `ech`, is kept under.
fn connection_key(partition: &str, host: &str, alternative: &str, ech: Option<&[u8]>) -> String {
    let mut key = format!("{partition} {host} {alternative}");
    if let Some(ech) = ech {
        key.push_str(&format!(" ech {}", data_encoding::HEXLOWER.encode(ech)));
    }
    key
}

/// Where each origin's requests go over HTTP/3, and the connections to
/// them, shared by every clone of a Fetcher.
pub(crate) struct Routes {
    stack: Stack,
    trust: Trust,
    /// Each origin's alternative, by network partition (the request's
    /// top-level site) and origin (`https://host:port`), as Firefox keys
    /// them: the origin attributes in AltSvcMapping::MakeHashKey carry the
    /// partition key when network state is partitioned, as it is by
    /// default. They live as long as the Fetcher, as in a private window:
    /// nothing is written to disk.
    alternatives: Mutex<HashMap<Key, Alternative>>,
    excluded: Mutex<Excluded>,
    validate: Validate,
    /// Addresses given for names, in place of the system's resolver.
    hosts: Arc<HashMap<String, Vec<IpAddr>>>,
    /// HTTPS record lookups by origin (`host:port`), kept as Firefox's
    /// host cache keeps them: for the records' TTL and then the grace
    /// period, and an empty answer for its negative TTL and the grace.
    records: Mutex<HashMap<String, watch::Receiver<Option<Found>>>>,
    /// Given answers to HTTPS queries, for tests.
    seam: Option<Arc<crate::https_rr::Seam>>,
    /// Firefox's host cache, which the TCP connections' lookups share.
    host_cache: Option<Arc<crate::https_rr::HostCache>>,
    /// Connections by partition, server name and alternative.
    connections: Mutex<HashMap<String, Handle>>,
    /// The newest session ticket from each server, for the next
    /// connection to it, as Firefox's SSLTokensCache keeps them. Each is
    /// used once.
    tickets: Tickets,
    /// The same for Chrome's connections, with each server's RTT.
    chrome_tickets: crate::h3_chrome::Tickets,
    /// Chrome's: each origin's Alt-Svc entries, by partition and origin,
    /// in the header's order, with when each expires
    /// (HttpServerProperties::SetAlternativeServices).
    chrome_alternatives: Mutex<HashMap<(String, String), ChromeAlternatives>>,
    /// Chrome's broken and recently broken alternatives, by partition and
    /// `host:port` (BrokenAlternativeServices).
    broken: Mutex<HashMap<(String, String), crate::h3_chrome::Broken>>,
    /// Alternatives whose race the TCP job won while their QUIC job was
    /// still connecting: broken if that one then fails
    /// (JobController::MaybeReportBrokenAlternativeService).
    main_won: Mutex<HashSet<(String, String)>>,
    /// Whether a QUIC connection has come up yet, which Chrome waits for
    /// before it delays a TCP job behind one
    /// (QuicSessionPool::has_quic_ever_worked_on_current_network_).
    quic_worked: std::sync::atomic::AtomicBool,
    /// Safari's: when each origin, by partition, was last answered over
    /// TCP, standing in for its TCP connection being open.
    tcp_used: Mutex<HashMap<(String, String), Instant>>,
}

type Tickets = Arc<Mutex<HashMap<String, ResumptionToken>>>;

/// An origin's Alt-Svc entries as Chrome keeps them, each with its expiry.
type ChromeAlternatives = Vec<(crate::h3_chrome::AltSvc, Instant)>;

impl Routes {
    pub(crate) fn new(
        trust: Trust,
        validate: Validate,
        hosts: Arc<HashMap<String, Vec<IpAddr>>>,
        seam: Option<Arc<crate::https_rr::Seam>>,
        stack: Stack,
        host_cache: Option<Arc<crate::https_rr::HostCache>>,
    ) -> Routes {
        Routes {
            stack,
            trust,
            alternatives: Mutex::default(),
            excluded: Mutex::default(),
            validate,
            hosts,
            records: Mutex::default(),
            seam,
            host_cache,
            connections: Mutex::default(),
            tickets: Tickets::default(),
            chrome_tickets: crate::h3_chrome::Tickets::default(),
            chrome_alternatives: Mutex::default(),
            broken: Mutex::default(),
            main_won: Mutex::default(),
            quic_worked: std::sync::atomic::AtomicBool::new(false),
            tcp_used: Mutex::default(),
        }
    }

    /// How many address lookups Firefox's host cache has started.
    #[cfg(test)]
    pub(crate) fn address_lookups(&self) -> usize {
        self.host_cache
            .as_ref()
            .map_or(0, |c| c.lookups.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Whether the Fetcher follows Chrome's rules, as Safari's profiles
    /// also do, with the exceptions `is_safari` marks.
    pub(crate) fn is_chrome(&self) -> bool {
        matches!(self.stack, Stack::Chrome(_) | Stack::Safari(_))
    }

    fn is_safari(&self) -> bool {
        matches!(self.stack, Stack::Safari(_))
    }

    /// Chrome's half of `learn` (HttpNetworkTransaction::ProcessAltSvcHeader,
    /// HttpStreamFactory::ProcessAlternativeServices): a header Chrome
    /// can't parse changes nothing; one it can replaces the origin's whole
    /// list with its `h3` and `h2` entries, `h3-29` and the like left out;
    /// `clear`, or a list left empty, forgets them.
    fn learn_chrome(&self, partition: &str, origin: &Url, header: &str) {
        let Some(entries) = crate::h3_chrome::parse_alt_svc(header) else {
            return;
        };
        let key = (partition.to_string(), origin.origin().ascii_serialization());
        let forced = (key.0.clone(), key.1.clone(), true);
        if self
            .alternatives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&forced)
            .is_some_and(|a| a.forced)
        {
            return;
        }
        let now = Instant::now();
        let kept: Vec<_> = entries
            .into_iter()
            .filter(|e| e.protocol == "h3" || e.protocol == "h2")
            .map(|e| {
                let expires = now + Duration::from_secs(u64::from(e.max_age));
                (e, expires)
            })
            .collect();
        let mut all = self
            .chrome_alternatives
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if kept.is_empty() {
            all.remove(&key);
        } else {
            all.insert(key, kept);
        }
    }

    /// Chrome's half of `alternative`
    /// (JobController::GetAdvertisedAltSvcInternal): the first `h3` entry
    /// that hasn't expired, isn't broken, isn't on a port Chrome refuses
    /// or a high port for a low one; or, before it, any whose connection
    /// is already up. An expired broken mark takes its alternative out of
    /// every origin's list in the partition, which then needs a new
    /// Alt-Svc (HttpServerProperties::OnExpireBrokenAlternativeService).
    fn alternative_chrome(&self, partition: &str, origin: &str) -> Option<Route> {
        let now = Instant::now();
        let url = Url::parse(origin).ok()?;
        let (host, port) = (url.host_str()?.to_string(), url.port_or_known_default()?);
        let mut all = self
            .chrome_alternatives
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        {
            let mut broken = self.broken.lock().unwrap_or_else(|e| e.into_inner());
            for ((p, to), b) in broken.iter_mut() {
                if p == partition && b.until.is_some_and(|u| u <= now) {
                    b.until = None;
                    for ((q, o), list) in all.iter_mut() {
                        if q == partition {
                            let origin_host = Url::parse(o)
                                .ok()
                                .and_then(|u| u.host_str().map(str::to_string))
                                .unwrap_or_default();
                            list.retain(|(e, _)| {
                                let h = if e.host.is_empty() {
                                    &origin_host
                                } else {
                                    &e.host
                                };
                                format!("{h}:{}", e.port) != *to
                            });
                        }
                    }
                }
            }
        }
        let key = (partition.to_string(), origin.to_string());
        let list = all.get_mut(&key)?;
        list.retain(|(_, expires)| now < *expires);
        let broken = self.broken.lock().unwrap_or_else(|e| e.into_inner());
        let connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let mut chosen = None;
        for (e, _) in list.iter().filter(|(e, _)| e.protocol == "h3") {
            let to_host = if e.host.is_empty() {
                host.as_str()
            } else {
                e.host.as_str()
            };
            let to = format!("{to_host}:{}", e.port);
            if broken
                .get(&(partition.to_string(), to.clone()))
                .is_some_and(|b| b.is_broken(now))
                || crate::h3_chrome::RESTRICTED_PORTS.contains(&e.port)
                || (e.port >= 1024 && port < 1024)
            {
                continue;
            }
            let up = connections
                .get(&format!("{partition} {host} {to}"))
                .is_some_and(|h| h.usable() && h.taking_requests());
            if up {
                chosen = Some(to);
                break;
            }
            chosen.get_or_insert(to);
        }
        chosen.map(|alternative| Route {
            partition: partition.to_string(),
            alternative,
            forced: false,
            h3: true,
            record: None,
            from: None,
        })
    }

    /// Whether a request waits for its origin's HTTPS records before
    /// connecting, as Safari's do, rather than racing them as Firefox's.
    pub(crate) fn waits_for_records(&self) -> bool {
        self.is_safari()
    }

    /// Whether the Fetcher is Chrome's: an Alt-Svc alternative goes before
    /// an HTTPS record, and a request waits for its origin's records as
    /// long as Chrome's resolver does.
    pub(crate) fn alt_svc_first(&self) -> bool {
        self.is_chrome() && !self.is_safari()
    }

    /// A response from `origin` came, over TCP if `tcp`. Safari's next
    /// requests stay there while that connection would still be open, and
    /// Chrome's don't wait for a QUIC connection from an HTTPS record. One
    /// over HTTP/3 leaves no TCP connection open.
    pub(crate) fn tcp_used(&self, partition: &str, origin: &Url, tcp: bool) {
        if tcp && self.is_chrome() {
            self.tcp_used
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (partition.to_string(), origin.origin().ascii_serialization()),
                    Instant::now(),
                );
        }
    }

    /// Whether Safari would still send `origin`'s requests over its TCP
    /// connection rather than to `route`, found by Alt-Svc or an HTTPS
    /// record alike: one answered within
    /// [`safari::TCP_KEPT`], and no HTTP/3 connection to `route` up.
    pub(crate) fn tcp_open(&self, partition: &str, origin: &str, route: &Route) -> bool {
        if !self.is_safari() || route.forced {
            return false;
        }
        let Some(host) = Url::parse(origin)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
        else {
            return false;
        };
        let ech = route.record.as_ref().and_then(Option::as_deref);
        let up = self
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&connection_key(partition, &host, &route.alternative, ech))
            .is_some_and(|h| h.usable() && h.taking_requests());
        self.tcp_recent(partition, origin) && !up
    }

    /// Whether `origin`'s TCP connection is taken as still open: it
    /// answered within [`safari::TCP_KEPT`], wreq's idle timeout.
    fn tcp_recent(&self, partition: &str, origin: &str) -> bool {
        let mut used = self.tcp_used.lock().unwrap_or_else(|e| e.into_inner());
        let key = (partition.to_string(), origin.to_string());
        let recent = used
            .get(&key)
            .is_some_and(|at| at.elapsed() < safari::TCP_KEPT);
        if !recent {
            used.remove(&key);
        }
        recent
    }

    /// Takes every origin's TCP connection as closed, as if
    /// [`safari::TCP_KEPT`] had passed since its last response.
    pub(crate) fn expire_tcp(&self) {
        self.tcp_used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Chrome's main-job delay for a request to `alternative`.
    fn chrome_delay(&self, partition: &str, host: &str, alternative: &str) -> Duration {
        let worked = self.quic_worked.load(std::sync::atomic::Ordering::Relaxed);
        let recently = self.recently_broken(partition, alternative);
        let rtt = self
            .chrome_tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&format!("{partition} {host} {alternative}"))
            .map(|t| t.rtt);
        crate::h3_chrome::main_job_delay(worked, recently, rtt)
    }

    /// Whether a Chrome session to `alternative` in `partition` may send
    /// requests before its handshake is confirmed. It requires confirmation
    /// until QUIC has worked on this network, and to an alternative broken
    /// or recently broken (QuicSessionPool::CreateSessionSync,
    /// QuicSessionAttempt::DoCreateSession).
    fn chrome_sends_early(&self, partition: &str, alternative: &str) -> bool {
        self.quic_worked.load(std::sync::atomic::Ordering::Relaxed)
            && !self.recently_broken(partition, alternative)
    }

    /// Whether Chrome takes `alternative` in `partition` as broken or
    /// recently broken (HttpServerProperties::
    /// WasAlternativeServiceRecentlyBroken).
    fn recently_broken(&self, partition: &str, alternative: &str) -> bool {
        self.broken
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(partition.to_string(), alternative.to_string()))
            .is_some_and(|b| b.until.is_some() || b.count > 0)
    }

    /// Marks `alternative` broken in `partition`
    /// (HttpServerProperties::MarkAlternativeServiceBroken).
    fn mark_broken(&self, partition: &str, alternative: &str) {
        self.broken
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry((partition.to_string(), alternative.to_string()))
            .or_default()
            .mark(Instant::now());
    }

    /// A request routed to `route` was answered over TCP instead: for
    /// Chrome, the TCP job succeeded, which makes a QUIC job that failed
    /// (or fails later) to the same alternative broken
    /// (JobController::MaybeReportBrokenAlternativeService), as does a
    /// retry over TCP after a QUIC error
    /// (HttpNetworkTransaction::HandleIOError, `retried_alternative_service_`).
    ///
    /// Safari marks nothing broken: it went back to QUIC first on each new
    /// connection after one failed, every time it was timed.
    pub(crate) fn tcp_answered(&self, route: &Route, host: &str) {
        if !self.is_chrome() || self.is_safari() || route.forced {
            return;
        }
        let key = format!("{} {host} {}", route.partition, route.alternative);
        let state = self
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|h| *h.state.borrow());
        match state {
            Some(State::Failed { .. } | State::NotSetUp | State::Broken) => {
                self.mark_broken(&route.partition, &route.alternative)
            }
            Some(State::Connecting) => {
                self.main_won
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert((route.partition.clone(), route.alternative.clone()));
            }
            _ => {}
        }
    }

    /// Whether a response with `status` is read for Alt-Svc: Firefox
    /// skips a 421 and any 5xx (nsHttpChannel::ProcessAltService); Chrome
    /// reads every final response but a 421, which it sends again
    /// instead (HttpNetworkTransaction::DoReadHeadersComplete).
    pub(crate) fn learns_from(&self, status: u16) -> bool {
        status != 421 && (self.is_chrome() || status < 500)
    }

    /// Whether requests over HTTP/3 carry `Alt-Used`, as Firefox's do
    /// and Chrome's don't.
    pub(crate) fn alt_used(&self) -> bool {
        matches!(self.stack, Stack::Firefox)
    }

    /// Whether HTTP/3 is excluded from `host`, as Firefox's
    /// IsHttp3Excluded says.
    pub(crate) fn excluded(&self, host: &str) -> bool {
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
            next: None,
        };
        self.alternatives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((partition, origin, true), alternative);
    }

    /// Takes in an `Alt-Svc` header that came with a response from
    /// `origin` to a request in `partition`, as Firefox's
    /// AltSvcMapping::ProcessHeader and AltSvcCache::UpdateAltServiceMapping
    /// do: `clear` forgets the origin's alternatives; the first `h3` one
    /// whose host HTTP/3 hasn't failed to reach, and each `h2` one, is kept
    /// for its `ma=` seconds (a day without one), and proved at once: a
    /// connection is made to it, and the origin's requests use it once
    /// that is up.
    pub(crate) fn learn(self: &Arc<Self>, partition: &str, origin: &Url, header: &str) {
        if self.is_chrome() {
            return self.learn_chrome(partition, origin, header);
        }
        let (Some(host), Some(port)) = (origin.host_str(), origin.port_or_known_default()) else {
            return;
        };
        let serialized = origin.origin().ascii_serialization();
        let key = |h3| (partition.to_string(), serialized.clone(), h3);
        // AltSvcCache::ClearHostMapping: HTTP/3 and HTTP/2 alike.
        let forget = || {
            let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
            for h3 in [true, false] {
                if alternatives.get(&key(h3)).is_some_and(|a| !a.forced) {
                    alternatives.remove(&key(h3));
                }
            }
        };
        let (mut h3, mut h2) = (None, Vec::new());
        for entry in alt_svc(header) {
            let Some(((protocol, authority), parameters)) = entry.split_first() else {
                continue;
            };
            if protocol == "clear" {
                forget();
                continue;
            }
            // Firefox takes the port after the first colon, as atoi reads
            // it, and the host before it, the origin's if empty.
            let (to_host, to_port) = match authority.split_once(':') {
                Some((h, p)) => (h, leading_number(p)),
                None => ("", i64::from(port)),
            };
            let to_host = if to_host.is_empty() { host } else { to_host };
            // NS_CheckPortSafety: a port, and not one Firefox bans.
            let Some(to_port) = u16::try_from(to_port).ok().filter(|&p| !banned_port(p)) else {
                continue;
            };
            let is_h3 = protocol == "h3";
            if !(is_h3 && !self.excluded(to_host) || protocol == "h2") {
                continue;
            }
            let max_age = parameters
                .iter()
                .rev()
                .find(|(k, _)| k == "ma")
                .map_or(firefox::ALT_SVC_MAX_AGE, |(_, v)| leading_number(v));
            // HTTP/2 to the origin itself is no alternative, and Firefox
            // treats it as one with no lifetime (AltSvcMapping's
            // constructor): either clears what was there, as accepting it
            // would.
            let itself = !is_h3 && to_port == port && to_host.eq_ignore_ascii_case(host);
            if max_age <= 0 || itself {
                forget();
                continue;
            }
            let chosen = (
                format!("{to_host}:{to_port}"),
                Instant::now() + Duration::from_secs(max_age.unsigned_abs()),
            );
            if !is_h3 {
                h2.push(chosen);
            } else if h3.is_none() {
                h3 = Some(chosen);
            }
        }
        if let Some((to, expires)) = h3
            && self.update(&key(true), &to, expires)
        {
            let handle = self.connection(partition, host, &to, None, false);
            if *handle.state.borrow() == State::Connected {
                self.proved(partition, host, &to);
            }
        }
        for (to, expires) in h2 {
            if self.update(&key(false), &to, expires) {
                let (routes, key, proving) = (Arc::downgrade(self), key(false), to.clone());
                let proof = (self.validate)(origin.clone(), to);
                tokio::spawn(async move {
                    let ok = proof.await;
                    if let Some(routes) = Weak::upgrade(&routes) {
                        routes.settle(&key, &proving, ok);
                    }
                });
            }
        }
    }

    /// Takes in a new alternative `to` for `key` as
    /// AltSvcCache::UpdateAltServiceMapping does, and says whether to prove
    /// it now. A repeat of the one in use only renews it; one that
    /// expires sooner than it is ignored; any other is proved, and takes
    /// over once it is; nothing new is taken while one is being proved.
    fn update(&self, key: &Key, to: &str, expires: Instant) -> bool {
        let now = Instant::now();
        let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
        match alternatives.get_mut(key) {
            Some(known) if known.forced => false,
            // An expired one is as good as none (AltSvcCache::LookupMapping).
            Some(known) if known.validated && now < known.expires => {
                if known.to == to {
                    known.expires = expires;
                    false
                } else if expires < known.expires {
                    false
                } else {
                    known.next = Some((to.to_string(), expires));
                    true
                }
            }
            // Its connection is still being made.
            Some(known) if now < known.expires => false,
            _ => {
                let fresh = Alternative {
                    to: to.to_string(),
                    expires,
                    validated: false,
                    forced: false,
                    next: None,
                };
                alternatives.insert(key.clone(), fresh);
                true
            }
        }
    }

    /// How proving `to` for `key` ended: it takes over, or is held off
    /// for two seconds (AltSvcMappingValidator::OnTransactionDestroy).
    fn settle(&self, key: &Key, to: &str, ok: bool) {
        let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(known) = alternatives.get_mut(key) {
            settled(known, to, ok);
        }
    }

    /// The HTTPS records of `origin`, looked up as Firefox's nsHttpTransaction
    /// looks them up for each request (HTTPSRecordResolver, with
    /// network.dns.use_https_rr_as_altsvc): the HTTPS query and an address
    /// lookup for the canonical name, side by side, answered from the cache
    /// while they stand.
    pub(crate) fn lookup(self: &Arc<Self>, partition: &str, origin: &Url) -> Option<Lookup> {
        let (host, port) = (origin.host_str()?, origin.port_or_known_default()?);
        let key = format!("{host}:{port}");
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let standing = records.get(&key).filter(|found| {
            found
                .borrow()
                .as_ref()
                .is_none_or(|f| Instant::now() < f.expires)
        });
        let found = match standing {
            Some(found) => found.clone(),
            None => {
                let (tell, found) = watch::channel(None);
                records.insert(key, found.clone());
                let (host, seam) = (host.to_string(), self.seam.clone());
                let chrome = self.alt_svc_first();
                let given = self.hosts.contains_key(&host);
                let cache = self.host_cache.clone();
                tokio::spawn(async move {
                    // The canonical name comes from the address lookup the
                    // connection uses too, Firefox's host cache entry
                    // (network.dns.always_ai_canonname). A name given an
                    // address has none.
                    let cname = async {
                        match &cache {
                            _ if given => None,
                            Some(cache) => cache.lookup(&host).await?.cname.clone(),
                            None => crate::https_rr::canonical_name(&host).await,
                        }
                    };
                    let (records, cname) = if chrome {
                        chrome_resolve(&host, port, seam.as_deref(), given).await
                    } else {
                        tokio::join!(crate::https_rr::query(&host, port, seam.as_deref()), cname)
                    };
                    let expires = Instant::now()
                        + match (&records, chrome) {
                            // Chrome's host cache keeps an answer for its
                            // TTL, at least a minute, with no grace.
                            // shortcut: the addresses' TTL, which also
                            // bounds Chrome's entry, isn't known here, so
                            // one without records stands for the minimum.
                            (r, true) => r
                                .as_ref()
                                .map_or(Duration::ZERO, |r| r.ttl)
                                .max(crate::h3_chrome::DNS_MIN_TTL),
                            (r, false) => r
                                .as_ref()
                                .map_or(firefox::DNS_NEGATIVE_TTL, |r| r.ttl)
                                .saturating_add(firefox::DNS_GRACE),
                        };
                    tell.send_replace(Some(Found {
                        records,
                        cname,
                        expires,
                    }));
                });
                found
            }
        };
        Some(Lookup {
            found,
            origin: origin.clone(),
            partition: partition.to_string(),
        })
    }

    /// Where `lookup`'s records send a request, once they're in: to the
    /// record Firefox picks (HTTPSSVC.cpp GetServiceModeRecordWithCname,
    /// nsHttpConnectionInfo::CloneAndAdoptHTTPSSVCRecord), over HTTP/3
    /// when that one offers h3, else over TCP to its target and port,
    /// with its ECH configuration. None: no record, or one that changes
    /// nothing, the origin itself with no ECH and no ALPN of its own.
    fn adopt(&self, lookup: &Lookup, found: &Found) -> Option<Route> {
        let records = found.records.as_ref()?;
        let (host, port) = (
            lookup.origin.host_str()?,
            lookup.origin.port_or_known_default()?,
        );
        // Chrome's DNS_ALPN_H3 job: QUIC to the origin itself, unless that
        // is broken (JobController::DoCreateJobs).
        if self.alt_svc_first() {
            let canonical = found.cname.as_deref().unwrap_or(host);
            let ech = crate::https_rr::choose_chrome(records, port, canonical)?;
            let alternative = format!("{host}:{port}");
            let broken = self
                .broken
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(lookup.partition.clone(), alternative.clone()))
                .is_some_and(|b| b.is_broken(Instant::now()));
            return (!broken).then(|| Route {
                partition: lookup.partition.clone(),
                alternative,
                forced: false,
                h3: true,
                record: Some(ech),
                from: None,
            });
        }
        let chosen =
            crate::https_rr::choose(records, host, port, found.cname.as_deref(), &|target| {
                self.excluded(target)
            })?;
        let h3 = chosen.alpn == "h3";
        // Chrome's and Safari's TCP connections stay with the origin.
        if !h3 && self.is_chrome() {
            return None;
        }
        if chosen.port == 0 {
            return None;
        }
        // A record that sends TCP to the origin as it was still makes the
        // request one from HTTPS records (mOrigConnInfo), which a failed
        // handshake is made again from (PrepareConnInfoForRetry); its
        // connection is the origin's own.
        let to_origin = !h3
            && chosen.host.eq_ignore_ascii_case(host)
            && chosen.port == port
            && chosen.ech.is_none()
            && chosen.alpn.is_empty();
        Some(Route {
            partition: lookup.partition.clone(),
            alternative: format!("{}:{}", chosen.host, chosen.port),
            forced: false,
            h3,
            record: Some(chosen.ech.clone()),
            from: Some(Arc::new(FromRecords {
                found: found.clone(),
                host: host.to_string(),
                port,
                tcp: (!h3 && !to_origin).then_some(chosen),
                left: None,
                attempts: 0,
            })),
        })
    }

    /// The route `lookup` gives now if it's done, or `Err` with it if not.
    pub(crate) fn adopted(&self, lookup: Lookup) -> Result<Option<Route>, Box<Lookup>> {
        let found = lookup.found.borrow().clone();
        match found {
            Some(found) => Ok(self.adopt(&lookup, &found)),
            None => Err(Box::new(lookup)),
        }
    }

    /// Waits for `lookup` to be done, and gives the route it gives.
    pub(crate) async fn adopt_when_found(&self, mut lookup: Box<Lookup>) -> Option<Route> {
        let found = lookup.found.wait_for(Option::is_some).await.ok()?.clone()?;
        self.adopt(&lookup, &found)
    }

    /// The alternative of a request to `origin` in `partition`, if it has
    /// a proved one that hasn't expired, to a host HTTP/3 hasn't failed to
    /// reach (AltSvcCache::LookupMapping).
    pub(crate) fn alternative(&self, partition: &str, origin: &str) -> Option<Route> {
        let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
        let forced = |h3: bool| {
            alternatives
                .get(&(partition.to_string(), origin.to_string(), h3))
                .is_some_and(|a| a.forced)
        };
        if self.is_chrome() && !forced(true) && !forced(false) {
            drop(alternatives);
            return self.alternative_chrome(partition, origin);
        }
        // HTTP/3 first, then HTTP/2 (AltSvcCache::GetAltServiceMapping).
        for h3 in [true, false] {
            let key = (partition.to_string(), origin.to_string(), h3);
            let Some(known) = alternatives.get(&key) else {
                continue;
            };
            let gone = Instant::now() >= known.expires || h3 && self.excluded(host_of(&known.to));
            if !known.forced && gone {
                alternatives.remove(&key);
                continue;
            }
            if known.validated {
                return Some(Route {
                    partition: partition.to_string(),
                    alternative: known.to.clone(),
                    forced: known.forced,
                    h3,
                    record: None,
                    from: None,
                });
            }
        }
        None
    }

    /// A connection to `alternative` made up from `host`'s alternatives
    /// is up: they're proved.
    fn proved(&self, partition: &str, host: &str, alternative: &str) {
        let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
        for ((p, origin, h3), known) in alternatives.iter_mut() {
            let same_host = Url::parse(origin)
                .ok()
                .is_some_and(|u| u.host_str() == Some(host));
            if *h3 && p == partition && same_host {
                settled(known, alternative, true);
            }
        }
    }

    /// A connection to `alternative` never came up: its unproved
    /// alternatives stay unproved, and hold off another offer of the same
    /// for two seconds (AltSvcMappingValidator::OnTransactionDestroy); one
    /// that was to replace a proved alternative goes, leaving that one.
    /// HTTP/3 to its host stops too if Firefox would exclude it.
    fn failed(&self, alternative: &str, exclude: bool) {
        if exclude {
            self.exclude(alternative);
        }
        let mut alternatives = self.alternatives.lock().unwrap_or_else(|e| e.into_inner());
        for ((_, _, h3), known) in alternatives.iter_mut() {
            if *h3 {
                settled(known, alternative, false);
            }
        }
    }

    /// Stops HTTP/3 to `alternative`'s host (nsHttpHandler::ExcludeHttp3).
    fn exclude(&self, alternative: &str) {
        self.excluded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add(host_of(alternative), network);
    }

    /// The connection to `alternative` for `host` in `partition`, opened
    /// if there's none that is up or coming up. A new one is watched until
    /// it's up or has failed.
    fn connection(
        self: &Arc<Self>,
        partition: &str,
        host: &str,
        alternative: &str,
        ech: Option<&[u8]>,
        for_request: bool,
    ) -> Handle {
        let key = connection_key(partition, host, alternative, ech);
        let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = connections.get(&key)
            && handle.usable()
        {
            return handle.clone();
        }
        let given = self
            .hosts
            .get(host_of(alternative))
            .and_then(|a| a.first().copied());
        let handle = match &self.stack {
            Stack::Firefox => open(
                host,
                alternative,
                given,
                ech.map(<[u8]>::to_vec),
                &key,
                self.trust.clone(),
                &self.tickets,
                self.host_cache.clone(),
                for_request,
            ),
            Stack::Chrome(tls) => crate::h3_chrome::open(
                host,
                alternative,
                given,
                ech.map(<[u8]>::to_vec),
                &key,
                tls.clone(),
                crate::h3_chrome::Browser::Chrome,
                self.trust.clone(),
                &self.chrome_tickets,
                self.chrome_sends_early(partition, alternative),
            ),
            Stack::Safari(tls) => crate::h3_chrome::open(
                host,
                alternative,
                given,
                None,
                &key,
                tls.clone(),
                crate::h3_chrome::Browser::Safari,
                self.trust.clone(),
                &self.chrome_tickets,
                // shortcut: Safari's 0-RTT isn't recorded, so it waits for
                // the handshake; upgrade once a recording shows its rules.
                false,
            ),
        };
        connections.insert(key, handle.clone());
        let (routes, mut state) = (Arc::downgrade(self), handle.state.clone());
        let (partition, host, alternative) = (
            partition.to_string(),
            host.to_string(),
            alternative.to_string(),
        );
        tokio::spawn(async move {
            let outcome = state
                .wait_for(|s| !matches!(s, State::Connecting | State::ZeroRtt))
                .await
                .map_or(State::Failed { exclude: false }, |s| *s);
            let Some(now) = Weak::upgrade(&routes) else {
                return;
            };
            // Chrome: a handshake confirms the alternative, forgetting
            // that it was ever broken (QuicSessionPool::
            // ProcessGoingAwaySession, ConfirmAlternativeService); a job
            // that fails after the TCP one won makes it broken.
            if now.is_safari() {
                if outcome == State::Connected {
                    now.quic_worked
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return;
            }
            if now.is_chrome() {
                let key = (partition.clone(), alternative.clone());
                let won = now
                    .main_won
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
                match outcome {
                    State::Connected => {
                        now.quic_worked
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        now.broken
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&key);
                    }
                    State::Failed { .. } if won => now.mark_broken(&partition, &alternative),
                    _ => {}
                }
                return;
            }
            match outcome {
                State::Connected => now.proved(&partition, &host, &alternative),
                State::Failed { exclude } => return now.failed(&alternative, exclude),
                State::NotSetUp => return now.failed(&alternative, true),
                State::Connecting | State::ZeroRtt | State::Closed | State::Broken => return,
            }
            drop(now);
            // Then, once it's up, whether it goes down with an error.
            let broken = state
                .wait_for(|s| matches!(s, State::Closed | State::Broken))
                .await
                .is_ok_and(|s| *s == State::Broken);
            if broken && let Some(now) = Weak::upgrade(&routes) {
                now.exclude(&alternative);
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

    /// Whether `route`'s connection for `host` failed so that Firefox
    /// takes HTTP/3 away from the request (NS_HTTP_DISALLOW_HTTP3):
    /// Http3Session::Shutdown's DisableHttp3 for a failure it excludes the
    /// host for, which a TLS one isn't.
    pub(crate) fn ruled_out_h3(&self, route: &Route, host: &str) -> bool {
        let ech = route.record.as_ref().and_then(Option::as_deref);
        let key = connection_key(&route.partition, host, &route.alternative, ech);
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .is_some_and(|h| {
                matches!(
                    *h.state.borrow(),
                    State::Failed { exclude: true } | State::Broken
                )
            })
    }

    /// The ECH configurations the server that rejected `route`'s
    /// connection for `host` sent to retry with, if it sent any
    /// (SSL_ERROR_ECH_RETRY_WITH_ECH).
    pub(crate) fn ech_retry(&self, route: &Route, host: &str) -> Option<Vec<u8>> {
        let ech = route.record.as_ref().and_then(Option::as_deref);
        let key = connection_key(&route.partition, host, &route.alternative, ech);
        let connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let handle = connections.get(&key)?;
        let configs = handle.ech_retry.lock().unwrap_or_else(|e| e.into_inner());
        configs.clone()
    }

    /// Sends `request` to `alternative` for `host`, on the connection
    /// already up to it if there is one, as Firefox does. A connection
    /// still being made gets the backup timer's 100 ms to come up before
    /// the request races a TCP connection for it; one that fails sends it
    /// to TCP, and so does one that closes under a request that is safe to
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
        let ech = route.record.as_ref().and_then(Option::as_deref);
        let handle = self.connection(&route.partition, host, alternative, ech, true);
        let mut state = handle.state.clone();
        // How long the request waits for a connection still coming up
        // before TCP races it: Chrome's main-job delay; Firefox's backup
        // timer, sooner for a route from an HTTPS record
        // (nsHttpTransaction::OnFastFallbackTimer).
        let timer = match route.record {
            _ if self.is_safari() => safari::RACE_DELAY,
            // Chrome's TCP job isn't held for the DNS_ALPN_H3 one when
            // the origin has a TCP connection open (JobController::
            // DoCreateJobs).
            Some(_)
                if self.alt_svc_first()
                    && Url::parse(&format!("https://{alternative}")).is_ok_and(|u| {
                        self.tcp_recent(&route.partition, &u.origin().ascii_serialization())
                    }) =>
            {
                Duration::ZERO
            }
            _ if self.is_chrome() => self.chrome_delay(&route.partition, host, alternative),
            Some(_) => firefox::FAST_FALLBACK,
            None => firefox::BACKUP_TIMER,
        };
        let waited = tokio::time::Instant::now();
        let up = route.forced
            || matches!(
                tokio::time::timeout(
                    timer,
                    state.wait_for(|s| *s != State::Connecting),
                )
                .await,
                Ok(Ok(s)) if matches!(*s, State::Connected | State::ZeroRtt)
            );
        if !up {
            if *state.borrow() == State::Connecting {
                return Ok(Answer::Race(Pending {
                    request: Some(request),
                    answer: None,
                    state,
                    third_party: handle.third_party.clone(),
                }));
            }
            return Ok(Answer::Tcp);
        }
        let (head, answer) = oneshot::channel();
        let job = Job {
            request,
            head,
            deadline,
        };
        // Never sent: it can go anywhere.
        if handle.jobs.send(job).is_err() {
            return if route.forced {
                Err(closed(alternative))
            } else {
                Ok(Answer::Tcp)
            };
        }
        // A request on a connection still in 0-RTT keeps Firefox's backup
        // timer (nsHttpTransaction::MaybeStartHttp3BackupTimer, while the
        // session isn't connected; Finish0RTT cancels it): if the
        // handshake isn't done when it fires, TCP races it. Chrome's QUIC
        // job is done once its session can send early data.
        if !self.is_chrome() && !route.forced && *state.borrow() == State::ZeroRtt {
            let left = timer.saturating_sub(waited.elapsed());
            let timed_out = tokio::time::timeout(left, state.wait_for(|s| *s != State::ZeroRtt))
                .await
                .is_err();
            if timed_out {
                return Ok(Answer::Race(Pending {
                    request: None,
                    answer: Some(answer),
                    state,
                    third_party: handle.third_party.clone(),
                }));
            }
        }
        self.finish(
            route,
            answer,
            &handle.state,
            &handle.third_party,
            idempotent,
        )
        .await
    }

    /// Sends a request that waited in a race, or waits for the answer to
    /// one already sent in 0-RTT.
    pub(crate) async fn send_pending(
        self: &Arc<Self>,
        route: &Route,
        host: &str,
        pending: Pending,
        deadline: tokio::time::Instant,
        idempotent: bool,
    ) -> Result<Answer, FetchError> {
        match (pending.answer, pending.request) {
            (Some(answer), _) => {
                let third_party = pending.third_party.clone();
                self.finish(route, answer, &pending.state, &third_party, idempotent)
                    .await
            }
            (None, Some(request)) => self.send(route, host, request, deadline, idempotent).await,
            (None, None) => Ok(Answer::Tcp),
        }
    }

    /// What became of a request sent on a connection whose state is
    /// `state`.
    async fn finish(
        &self,
        route: &Route,
        answer: oneshot::Receiver<Result<http::Response<Body>, FetchError>>,
        state: &watch::Receiver<State>,
        third_party: &std::sync::atomic::AtomicBool,
        idempotent: bool,
    ) -> Result<Answer, FetchError> {
        // A request safe to repeat that got nothing back goes again over
        // TCP when the connection went under it (nsHttpTransaction::Close).
        // Chrome sends any request again over TCP after a QUIC error that
        // came before its response (HttpNetworkTransaction::HandleIOError,
        // for every method).
        // Safari: only what's safe to repeat, as for Firefox (a guess;
        // Apple doesn't say).
        let chrome = self.is_chrome() && !self.is_safari();
        let repeat = |e: FetchError| {
            if (idempotent || chrome) && !route.forced {
                Ok(Answer::Tcp)
            } else {
                Err(e)
            }
        };
        match answer.await {
            Ok(Ok(response)) => Ok(Answer::Response(response)),
            // Its connection reached a root Firefox doesn't build in, and
            // nothing was sent (Handle::third_party).
            Ok(Err(_)) | Err(_) if third_party.load(std::sync::atomic::Ordering::Relaxed) => {
                Ok(Answer::Tcp)
            }
            Ok(Err(e)) if *state.borrow() == State::Connected => Err(e),
            Ok(Err(e)) => repeat(e),
            Err(_) => repeat(closed(&route.alternative)),
        }
    }
}

/// The error for a request whose HTTP/3 connection to `alternative` went
/// before it was answered.
fn closed(alternative: &str) -> FetchError {
    FetchError::new(
        FetchErrorKind::Connect,
        format!("the HTTP/3 connection to {alternative} closed"),
    )
}

/// An origin's HTTPS records and canonical name, looked up as Chrome's
/// HostResolverDnsTask does: the HTTPS query beside the address one, and
/// given at most [`crate::h3_chrome::https_extra_time`] once the addresses
/// are in; then it goes without them. Addresses Netweir was `given` are in
/// at once.
async fn chrome_resolve(
    host: &str,
    port: u16,
    seam: Option<&crate::https_rr::Seam>,
    given: bool,
) -> (Option<crate::https_rr::Records>, Option<String>) {
    let started = Instant::now();
    let query = crate::https_rr::query(host, port, seam);
    let addresses = async {
        if given {
            None
        } else {
            crate::https_rr::canonical_name(host).await
        }
    };
    tokio::pin!(query, addresses);
    tokio::select! {
        biased;
        records = &mut query => (records, addresses.await),
        cname = &mut addresses => {
            let extra = crate::h3_chrome::https_extra_time(started.elapsed());
            (tokio::time::timeout(extra, query).await.ok().flatten(), cname)
        }
    }
}

/// Whether a connection that closes for `reason` closed with an error, as
/// Http3Session::ProcessEvents reads ConnectionClosing: anything but a
/// stateless reset, no error at either layer (H3_NO_ERROR for HTTP/3), or
/// a server asking to retry with ECH.
fn protocol_error(reason: &neqo_transport::CloseReason) -> bool {
    use neqo_transport::{CloseReason, Error};
    !matches!(
        reason,
        CloseReason::Transport(
            Error::None
                | Error::IdleTimeout
                | Error::StatelessReset
                | Error::Peer(0)
                | Error::PeerApplication(0x100)
                | Error::EchRetry(_)
                | Error::Crypto(nss_rs::Error::EchRetry(_))
        ) | CloseReason::Application(0x100)
    )
}

/// An `Alt-Svc` value as Firefox's ParsedHeaderValueListList reads it:
/// alternatives split at commas, each into `name=value` pairs split at
/// semicolons, outside quotes, with the quotes taken off.
pub(crate) fn alt_svc(header: &str) -> Vec<Vec<(String, String)>> {
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
pub(crate) fn leading_number(s: &str) -> i64 {
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

/// Opens a connection to `alternative`, at the address `given` for its
/// host if there is one.
#[allow(clippy::too_many_arguments, reason = "each is the connection's own")]
fn open(
    host: &str,
    alternative: &str,
    given: Option<IpAddr>,
    ech: Option<Vec<u8>>,
    key: &str,
    trust: Trust,
    tickets: &Tickets,
    host_cache: Option<Arc<crate::https_rr::HostCache>>,
    for_request: bool,
) -> Handle {
    let (jobs, queue) = mpsc::unbounded_channel();
    let (state, watching) = watch::channel(State::Connecting);
    let ech_retry = Arc::new(Mutex::new(None));
    let retry_with = ech_retry.clone();
    let third_party = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refused = third_party.clone();
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
            // The alternative's address, from Firefox's host cache as the
            // TCP connections' are.
            let given = given.or_else(|| {
                let cache = host_cache.as_ref()?;
                let name = host_of(&alternative);
                if name.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
                    return None;
                }
                let answer = runtime.block_on(cache.lookup(name))?;
                answer.ips.first().copied()
            });
            runtime.block_on(run(
                host,
                alternative,
                given,
                ech,
                trust,
                ticket,
                queue,
                &state,
                &retry_with,
                &refused,
                for_request,
            ));
            // However it ended, it isn't coming up any more.
            state.send_modify(|s| {
                *s = match *s {
                    State::Connecting | State::ZeroRtt => State::Failed { exclude: true },
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
        ech_retry,
        third_party,
    }
}

pub(crate) fn fail_all(mut queue: mpsc::UnboundedReceiver<Job>, why: &str) {
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
    /// The request, while it went in 0-RTT and the server may still turn
    /// that down, when it's sent again once the handshake is done.
    early: Option<Request>,
}

/// How long a connection resumed in 0-RTT waits for the request it was
/// made for before its first flight goes.
const EARLY_JOB_WAIT: Duration = Duration::from_millis(20);

/// The ticket a connection resumes with, and where it leaves the next.
struct Ticket {
    used: Option<ResumptionToken>,
    key: String,
    tickets: Tickets,
}

#[allow(clippy::too_many_arguments, reason = "each is the connection's own")]
async fn run(
    host: String,
    alternative: String,
    given: Option<IpAddr>,
    ech: Option<Vec<u8>>,
    trust: Trust,
    ticket: Ticket,
    mut queue: mpsc::UnboundedReceiver<Job>,
    state: &watch::Sender<State>,
    ech_retry: &Mutex<Option<Vec<u8>>>,
    third_party: &std::sync::atomic::AtomicBool,
    for_request: bool,
) {
    let given = given.zip(
        alternative
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse().ok()),
    );
    let remote = match given {
        Some((ip, port)) => Ok(SocketAddr::new(ip, port)),
        None => alternative
            .to_socket_addrs()
            .map_err(|e| format!("{alternative}: {e}"))
            .and_then(|mut a| a.next().ok_or_else(|| format!("{alternative}: no address"))),
    };
    let remote = match remote {
        Ok(remote) => remote,
        Err(e) => return fail_all(queue, &format!("HTTP/3 to {alternative}: {e}")),
    };
    let connect = || -> Result<(Socket, Http3Client), String> {
        let socket = Socket::bind(remote).map_err(|e| e.to_string())?;
        let client = firefox_client(
            &host,
            socket.local,
            remote,
            ticket.used.as_ref(),
            ech.as_deref(),
        )?;
        Ok((socket, client))
    };
    let (socket, mut client) = match connect() {
        Ok(c) => c,
        Err(e) => {
            state.send_replace(State::NotSetUp);
            return fail_all(queue, &format!("HTTP/3 to {alternative}: {e}"));
        }
    };
    let mut recv = neqo_udp::RecvBuf::default();
    let mut streams: HashMap<StreamId, Stream> = HashMap::new();
    let mut waiting: Vec<Job> = Vec::new();
    let mut ready = false;
    // In 0-RTT since when, and whether a request went that way
    // (Http3Session's mZeroRttStarted and mHad0RttStream).
    let mut early: Option<Instant> = None;
    let mut had_early = false;
    // Whether the request this connection was made for has yet to have
    // its chance to join the first flight.
    let mut first_flight = for_request;
    // Whether the certificate check failed, which Firefox reads as an NSS
    // error (Http3Session::Authenticated).
    let mut cert_rejected = false;
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
                    let (checked, built_in) = trust.check(&host, &chain);
                    cert_rejected = checked != AuthenticationStatus::Ok;
                    // network.http.http3.disable_when_third_party_roots_found:
                    // a chain to a root Firefox didn't ship ends HTTP/3
                    // here, before the handshake goes on, and the request
                    // goes over TCP (Http3Session::Authenticated:
                    // DisableHttp3, NS_ERROR_NET_RESET; Shutdown then
                    // excludes the host).
                    if !built_in {
                        let why = format!(
                            "HTTP/3 to {alternative}: the certificate chain ends at a root Firefox doesn't build in"
                        );
                        third_party.store(true, std::sync::atomic::Ordering::Relaxed);
                        state.send_replace(State::Failed { exclude: true });
                        client.close(Instant::now(), 0x100, "");
                        while let OutputBatch::DatagramBatch(batch) = client
                            .process_multiple_output(
                                Instant::now(),
                                NonZeroUsize::new(socket.state.max_gso_segments())
                                    .unwrap_or(NonZeroUsize::MIN),
                            )
                        {
                            let _ = socket.send(&batch).await;
                        }
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
                    client.authenticated(checked, Instant::now());
                }
                // The server rejected ECH: its certificate is checked
                // against the configuration's public name, and the
                // handshake then fails with ech_required
                // (Http3Session::CallCertVerification).
                Http3ClientEvent::EchFallbackAuthenticationNeeded { public_name } => {
                    let chain: Vec<Vec<u8>> = client
                        .peer_certificate()
                        .map(|c| c.iter().map(<[u8]>::to_vec).collect())
                        .unwrap_or_default();
                    let name = if public_name.is_empty() {
                        &host
                    } else {
                        &public_name
                    };
                    let (checked, _) = trust.check(name, &chain);
                    cert_rejected = checked != AuthenticationStatus::Ok;
                    client.authenticated(checked, Instant::now());
                }
                Http3ClientEvent::StateChange(Http3State::ZeroRtt) => {
                    early = Some(Instant::now());
                    state.send_replace(State::ZeroRtt);
                }
                // The server turned the early data down: neqo has dropped
                // the streams, and Firefox sends their requests again once
                // connected, without 0-RTT (Http3Session::Finish0Rtt).
                Http3ClientEvent::ZeroRttRejected => {
                    early = None;
                    let sent: Vec<StreamId> = streams
                        .iter()
                        .filter(|(_, s)| s.early.is_some())
                        .map(|(&id, _)| id)
                        .collect();
                    for id in sent {
                        let Some(s) = streams.remove(&id) else {
                            continue;
                        };
                        if let (Some(head), Some(request)) = (s.head, s.early) {
                            waiting.push(Job {
                                request,
                                head,
                                deadline: s.deadline,
                            });
                        }
                    }
                }
                Http3ClientEvent::StateChange(Http3State::Connected) => {
                    ready = true;
                    early = None;
                    for s in streams.values_mut() {
                        s.early = None;
                    }
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
                    closed @ (Http3State::Closing(_) | Http3State::Closed(_)),
                ) => {
                    let (Http3State::Closing(reason) | Http3State::Closed(reason)) = &closed else {
                        unreachable!()
                    };
                    let why = format!("HTTP/3 connection to {alternative} closed: {reason:?}");
                    // A server that rejected ECH and sent configurations
                    // to retry with (CloseError::EchRetry).
                    let retry_with = match reason {
                        neqo_transport::CloseReason::Transport(
                            neqo_transport::Error::EchRetry(configs)
                            | neqo_transport::Error::Crypto(nss_rs::Error::EchRetry(configs)),
                        ) if !ready => Some(configs.clone()),
                        _ => None,
                    };
                    let kind = if ready {
                        FetchErrorKind::Body
                    } else if retry_with.is_some() || format!("{reason:?}").contains("Crypto") {
                        FetchErrorKind::Tls
                    } else {
                        FetchErrorKind::Connect
                    };
                    // Only a close Firefox didn't start, seen while closing
                    // rather than after (ConnectionClosing, not Closed).
                    let broken = matches!(closed, Http3State::Closing(_))
                        && !closing
                        && protocol_error(reason);
                    state.send_replace(match (ready, broken) {
                        (true, true) => State::Broken,
                        (true, false) => State::Closed,
                        // Http3Session::Shutdown excludes the host for a
                        // failure before it connected unless it was an NSS
                        // error, which only a rejected certificate is, or
                        // an ECH retry. Any other close, ech_required with
                        // nothing to retry with among them, is
                        // NS_ERROR_NET_HTTP3_PROTOCOL_ERROR.
                        // Nor after a request went in 0-RTT, since the next
                        // connection makes a full handshake.
                        (false, _) => State::Failed {
                            exclude: !cert_rejected && retry_with.is_none() && !had_early,
                        },
                    });
                    if let Some(configs) = retry_with {
                        *ech_retry.lock().unwrap_or_else(|e| e.into_inner()) = Some(configs);
                    }
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
        // Firefox makes a session for the transaction that asked for it, so
        // a resumed one sends that request in its first flight, coalesced
        // after the Initial. Here the request is on its way once the
        // connection says it can take requests: it's taken before anything
        // goes out, waiting at most EARLY_JOB_WAIT for it.
        if first_flight && early.is_some() && accepting {
            first_flight = false;
            match tokio::time::timeout(EARLY_JOB_WAIT, queue.recv()).await {
                Ok(Some(job)) => waiting.push(job),
                Ok(None) => accepting = false,
                Err(_) => {}
            }
            while let Ok(job) = queue.try_recv() {
                waiting.push(job);
            }
        }
        if ready {
            for job in std::mem::take(&mut waiting) {
                start(&mut client, &mut streams, job, false);
            }
        } else if early.is_some() {
            for job in std::mem::take(&mut waiting) {
                if safe_method(&job.request.method) {
                    had_early = true;
                    start(&mut client, &mut streams, job, true);
                } else {
                    waiting.push(job);
                }
            }
        }
        // A connection stuck in 0-RTT past network.http.http3.0rtt_timeout
        // fails (Http3Session::ProcessOutputAndEvents), and with a request
        // sent in 0-RTT, isn't excluded.
        if let Some(since) = early
            && !ready
            && since.elapsed() >= firefox::ZERO_RTT_TIMEOUT
        {
            let why = format!("HTTP/3 to {alternative}: stuck in 0-RTT");
            client.close(Instant::now(), 0x100, "");
            state.send_replace(State::Failed {
                exclude: !had_early,
            });
            for (_, s) in streams.drain() {
                end(s, FetchError::new(FetchErrorKind::Timeout, why.clone()));
            }
            for job in waiting.drain(..) {
                let _ = job
                    .head
                    .send(Err(FetchError::new(FetchErrorKind::Timeout, why.clone())));
            }
            return fail_all(queue, &why);
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
        let mut timeout = timeout.unwrap_or(Duration::from_secs(3600));
        if let Some(since) = early {
            timeout = timeout.min(firefox::ZERO_RTT_TIMEOUT.saturating_sub(since.elapsed()));
        }
        let sleep = tokio::time::sleep(timeout);
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
                Some(job) if ready => start(&mut client, &mut streams, job, false),
                Some(job) => waiting.push(job),
                None => accepting = false,
            },
        }
    }
}

/// Starts `job`'s request; `in_early_data`, in 0-RTT, keeping it to send
/// again should the server turn that down.
fn start(
    client: &mut Http3Client,
    streams: &mut HashMap<StreamId, Stream>,
    job: Job,
    in_early_data: bool,
) {
    let Job {
        request,
        head,
        deadline,
    } = job;
    let early = in_early_data.then(|| request.clone());
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
        early,
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

/// Whether a request may go in 0-RTT: its method is safe
/// (nsHttpTransaction::Do0RTT, nsHttpRequestHead::IsSafeMethod).
fn safe_method(method: &str) -> bool {
    matches!(
        method,
        "GET" | "HEAD" | "OPTIONS" | "TRACE" | "PROPFIND" | "REPORT" | "SEARCH"
    )
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

    use super::{Body, Excluded, Trust, alt_svc, decompressed, leading_number, priority};

    #[test]
    fn distrust_dates_never_pass_a_chain_boringssl_refused() {
        // A self-signed certificate no store holds, with a distrust-after
        // date it passes: BoringSSL's refusal stands for Firefox's roots.
        let names = vec!["localhost".to_string()];
        let made = rcgen::generate_simple_self_signed(names).unwrap();
        let der = made.cert.der().to_vec();
        let trust = Trust::new(
            true,
            None,
            crate::profile::Roots::Mozilla,
            &[],
            &[(der.clone(), i64::MAX)],
        )
        .unwrap();
        assert!(!trust.trusts("localhost", &[der]));
    }

    #[test]
    fn a_chrome_session_waits_for_confirmation_as_chrome_says() {
        use super::{Routes, Stack, Trust, Validate};
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use std::time::Instant;
        let validate: Validate = Arc::new(|_, _| Box::pin(async { false }));
        let tls = Arc::new(
            crate::profile::Profile::named("chrome-157-macos")
                .unwrap()
                .tls
                .clone(),
        );
        let routes = Routes::new(
            Trust::new(false, None, crate::profile::Roots::Chromium, &[], &[]).unwrap(),
            validate,
            Arc::default(),
            None,
            Stack::Chrome(tls),
            None,
        );
        let (site, to) = ("site", "h3.example:443");
        // Until QUIC has worked on the network, every session waits.
        assert!(!routes.chrome_sends_early(site, to));
        routes.quic_worked.store(true, Ordering::Relaxed);
        assert!(routes.chrome_sends_early(site, to));
        // Broken, and still recently broken once that has expired.
        routes.mark_broken(site, to);
        assert!(!routes.chrome_sends_early(site, to));
        let key = (site.to_string(), to.to_string());
        routes.broken.lock().unwrap().get_mut(&key).unwrap().until = Some(Instant::now());
        assert!(!routes.chrome_sends_early(site, to));
        assert!(routes.chrome_sends_early(site, "other.example:443"));
        // A handshake that confirms it forgets it was ever broken.
        routes.broken.lock().unwrap().remove(&key);
        assert!(routes.chrome_sends_early(site, to));
    }

    #[test]
    fn http3_is_taken_from_the_request_as_firefox_takes_it() {
        // DisableHttp3 for what Http3Session::Shutdown excludes the host
        // for, a failure to connect or an error once up; not for a TLS
        // failure, nor for a socket that couldn't be set up
        // (DnsAndConnectSocket::SetupConn's DisableHttp3(true)).
        use super::{Handle, Route, Routes, Stack, State, Trust, Validate, connection_key};
        use std::sync::Arc;
        let validate: Validate = Arc::new(|_, _| Box::pin(async { false }));
        let routes = Routes::new(
            Trust::new(false, None, crate::profile::Roots::Chromium, &[], &[]).unwrap(),
            validate,
            Arc::default(),
            None,
            Stack::Firefox,
            None,
        );
        let route = Route {
            partition: "site".into(),
            alternative: "h3.example:443".into(),
            forced: false,
            h3: true,
            record: Some(Some(vec![1])),
            from: None,
        };
        assert!(!routes.ruled_out_h3(&route, "origin.example"));
        let key = connection_key("site", "origin.example", "h3.example:443", Some(&[1]));
        for (state, ruled_out) in [
            (State::Failed { exclude: true }, true),
            (State::Broken, true),
            (State::Failed { exclude: false }, false),
            (State::NotSetUp, false),
            (State::Connecting, false),
            (State::Closed, false),
        ] {
            let (_, state_now) = tokio::sync::watch::channel(state);
            let (jobs, _) = mpsc::unbounded_channel();
            let handle = Handle {
                jobs,
                thread: Arc::default(),
                state: state_now,
                ech_retry: Arc::default(),
                third_party: Arc::default(),
            };
            routes
                .connections
                .lock()
                .unwrap()
                .insert(key.clone(), handle);
            assert_eq!(
                routes.ruled_out_h3(&route, "origin.example"),
                ruled_out,
                "{state:?}"
            );
        }
    }

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
