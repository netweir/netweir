//! A local HTTPS server that records how each client connects and what it
//! sends: the ClientHello, the opening HTTP/2 frames, and every request's
//! headers. Each response body is the recorded request as JSON.
//!
//! Requests can ask for behaviour through the query string:
//! `?set=NAME` adds `Set-Cookie: NAME=1; Path=/`, `?alt-svc=VALUE` adds
//! `Alt-Svc: VALUE` (percent-decoded) in place of the one
//! [`Server::start_h3`] sends, `/redirect?to=URL` answers 302 to URL,
//! `/close?code=N` over HTTP/3 answers, then closes the QUIC connection
//! with application error N, and `/loop` redirects to itself
//! with a relative `Location`. `/form` serves a page whose script submits a
//! form, POST to `/submitted`; `/fetch` serves one whose script POSTs JSON
//! to `/api` with `fetch()`. `/form-redirect` is a form that POSTs to a 302
//! (as a login does), and `/link` a page with a link to `/linked`.
//! `/auth` asks for a login (401, Basic) until a request carries
//! Authorization, then serves a page that loads an image, POSTs JSON with
//! `fetch()` and submits a form, so each kind of request a browser sends
//! with the login it holds is captured.
//!
//! [`Server::start_h3`] also listens on UDP, on the same port, and its TCP
//! replies carry `Alt-Svc: h3`, so a browser moves to HTTP/3 as it would
//! for a real site. A capture over HTTP/3 adds the client's first QUIC
//! flight and what it sent on the HTTP/3 connection.

use std::collections::HashMap;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

use crate::client_hello::{ClientHello, bytes_needed};
use crate::h2::{self, Http2};
use crate::h3::{self, Http3};
use crate::ja4::{ja4, ja4_quic};
use crate::quic::{self, Quic};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    /// Which connection to this server, counting from 0.
    pub connection: u64,
    /// Which request on that connection, counting from 0.
    pub request: u32,
    pub ja4: String,
    pub client_hello: ClientHello,
    /// "h3", "h2" or "http/1.1".
    pub protocol: String,
    pub http2: Option<Http2>,
    /// The client's first QUIC flight, for a request over HTTP/3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quic: Option<Quic>,
    /// What the client sent on the HTTP/3 connection and this request's
    /// stream, for a request over HTTP/3. A QUIC connection that closes
    /// without a request still gives one capture, with no `http3`, an
    /// empty method and path, and its first flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http3: Option<Http3>,
    /// Path and query of the request.
    pub path: String,
    /// "GET", "POST" and so on. Empty in captures made before it was kept.
    #[serde(default)]
    pub method: String,
    /// The request body, as text.
    #[serde(default)]
    pub body: String,
    /// Request headers in the order and case they were sent. For HTTP/2
    /// this includes the pseudo-headers.
    pub headers: Vec<(String, String)>,
}

pub struct Server {
    pub port: u16,
    captures: mpsc::UnboundedReceiver<Capture>,
}

impl Server {
    /// Listens on 127.0.0.1 on a free port, with a self-signed certificate
    /// for `localhost` and `127.0.0.1`.
    pub async fn start() -> io::Result<Server> {
        Server::start_on(0).await
    }

    pub async fn start_on(port: u16) -> io::Result<Server> {
        Server::bind(port, &[b"h2", b"http/1.1"]).await
    }

    /// Offers only HTTP/1.1, to capture how a browser sends it.
    pub async fn start_http1(port: u16) -> io::Result<Server> {
        Server::bind(port, &[b"http/1.1"]).await
    }

    /// Like `start_on` or `start_http1`, but with the certificate in
    /// `dir` (see [`lasting_certificate`]), for a browser that has to be
    /// told to trust it once rather than every run.
    pub async fn start_trusted(port: u16, http1: bool, dir: &Path) -> io::Result<Server> {
        let alpn: &[&[u8]] = if http1 {
            &[b"http/1.1"]
        } else {
            &[b"h2", b"http/1.1"]
        };
        Server::bind_with(
            port,
            alpn,
            Some(lasting_certificate(dir)?),
            false,
            Early::No,
        )
        .await
    }

    /// Like `start_on`, and listens for QUIC on the same UDP port, on
    /// 127.0.0.1 and ::1, with every TCP reply carrying
    /// `Alt-Svc: h3=":PORT"`. The certificate is the one in `dir` if given
    /// (see [`lasting_certificate`]), else a new self-signed one.
    pub async fn start_h3(port: u16, dir: Option<&Path>) -> io::Result<Server> {
        let certificate = dir.map(lasting_certificate).transpose()?;
        Server::bind_with(port, &[b"h2", b"http/1.1"], certificate, true, Early::No).await
    }

    /// Like `start_h3`, but its QUIC session tickets allow early data, and
    /// it takes requests in 0-RTT: each capture's `http3.early` says
    /// whether its request came that way, read from the client's 0-RTT
    /// packets with the early secret rustls logs. `browser` sets it up for a
    /// browser's `/zero-rtt`: TCP offers only HTTP/1.1, which a browser
    /// can't coalesce an HTTP/3 request onto, and a QUIC connection with
    /// nothing on it times out after 3 s, a close that leaves the browser's
    /// Alt-Svc in place, so the next request resumes. Each QUIC datagram
    /// to the browser is held back by the duration given: on localhost a
    /// resumed handshake is otherwise done before the next request is made.
    pub async fn start_h3_early(
        port: u16,
        dir: Option<&Path>,
        browser: Option<std::time::Duration>,
    ) -> io::Result<Server> {
        let certificate = dir.map(lasting_certificate).transpose()?;
        let (alpn, early): (&[&[u8]], _) = match browser {
            Some(hold) => (&[b"http/1.1"], Early::ForBrowser(hold)),
            None => (&[b"h2", b"http/1.1"], Early::Take),
        };
        Server::bind_with(port, alpn, certificate, true, early).await
    }

    /// Like `start_h3_early`, but no session it gave a ticket for is ever
    /// resumed: a client that tries 0-RTT has its early data turned down,
    /// and makes a full handshake.
    pub async fn start_h3_turning_early_down(port: u16) -> io::Result<Server> {
        Server::bind_with(port, &[b"h2", b"http/1.1"], None, true, Early::TurnDown).await
    }

    async fn bind(port: u16, alpn: &[&[u8]]) -> io::Result<Server> {
        Server::bind_with(port, alpn, None, false, Early::No).await
    }

    /// A TCP listener on `port`, or on a free port for 0. With `h3`, a
    /// UDP socket on the same port too, kept and returned for QUIC:
    /// freed, another socket could take its port first. For port 0 the
    /// number is picked at random. Windows gives out TCP and UDP ports
    /// one after another, each blind to the other's reserved ranges, so
    /// in CI 51 picks in a row from either one were taken or reserved
    /// for the other.
    ///
    /// Chrome sends QUIC for `localhost` to ::1 and doesn't fall back to
    /// 127.0.0.1 as it does over TCP, so where the machine has IPv6 the
    /// port must be free on ::1 too, or a Chrome test would find nothing
    /// there and fail with no word of why.
    async fn listen(port: u16, h3: bool) -> io::Result<(TcpListener, u16, Option<QuicSockets>)> {
        if !h3 {
            let listener = TcpListener::bind(("127.0.0.1", port)).await?;
            let got = listener.local_addr()?.port();
            return Ok((listener, got, None));
        }
        let ipv6 = std::net::UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, 0)).is_ok();
        let mut tries = 0;
        loop {
            tries += 1;
            let got = if port == 0 { random_port() } else { port };
            let all = (|| -> io::Result<_> {
                let v4 = std::net::UdpSocket::bind(("127.0.0.1", got))?;
                let v6 = match ipv6 {
                    true => Some(std::net::UdpSocket::bind((
                        std::net::Ipv6Addr::LOCALHOST,
                        got,
                    ))?),
                    false => None,
                };
                Ok(QuicSockets { v4, v6 })
            })();
            let both = match all {
                Ok(quic) => TcpListener::bind(("127.0.0.1", got))
                    .await
                    .map(|listener| (listener, quic)),
                Err(e) => Err(e),
            };
            match both {
                Ok((listener, quic)) => return Ok((listener, got, Some(quic))),
                Err(e) if port != 0 || tries == 100 => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("port {got}, try {tries}: {e}"),
                    ));
                }
                Err(_) => {}
            }
        }
    }

    async fn bind_with(
        port: u16,
        alpn: &[&[u8]],
        certificate: Option<(Vec<u8>, Vec<u8>)>,
        h3: bool,
        early: Early,
    ) -> io::Result<Server> {
        let (listener, port, udp) = Server::listen(port, h3).await?;
        let certificate = certificate.unwrap_or_else(|| {
            let names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
            let made = rcgen::generate_simple_self_signed(names).expect("self-signed cert");
            (made.cert.der().to_vec(), made.signing_key.serialize_der())
        });
        let (tx, captures) = mpsc::unbounded_channel();
        let connections = Arc::new(AtomicU64::new(0));
        let alt_svc = if let Some(QuicSockets { v4, v6 }) = udp {
            let mut config = tls_config(&[b"h3"], certificate.clone());
            if early != Early::No {
                // What quinn requires of a server that takes 0-RTT.
                config.max_early_data_size = u32::MAX;
            }
            if early == Early::TurnDown {
                config.session_storage = Arc::new(Forgetful);
            }
            let hold = match early {
                Early::ForBrowser(hold) => Some(hold),
                _ => None,
            };
            let idle = hold.map(|_| EARLY_IDLE);
            let early = early != Early::No;
            let quic = (early, idle, hold);
            listen_quic(v4, config.clone(), quic, connections.clone(), tx.clone())?;
            // A machine without IPv6 just doesn't get this one.
            if let Some(v6) = v6 {
                listen_quic(v6, config, quic, connections.clone(), tx.clone())?;
            }
            Some(format!("h3=\":{port}\"; ma=86400"))
        } else {
            None
        };
        let acceptor = TlsAcceptor::from(Arc::new(tls_config(alpn, certificate)));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                let alt_svc = alt_svc.clone();
                let id = connections.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    // A broken or non-TLS connection is just dropped.
                    let _ = handle(id, stream, acceptor, tx, alt_svc).await;
                });
            }
        });
        Ok(Server { port, captures })
    }

    /// The next request's capture, in the order requests completed.
    pub async fn next(&mut self) -> Option<Capture> {
        self.captures.recv().await
    }
}

/// Whether a server's QUIC session tickets allow early data, and what
/// becomes of it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Early {
    No,
    Take,
    /// Set up for a browser, each QUIC datagram to it held back so long.
    ForBrowser(std::time::Duration),
    TurnDown,
}

/// A session store that keeps nothing, so every resumption fails and
/// early data with it.
#[derive(Debug)]
struct Forgetful;

impl rustls::server::StoresServerSessions for Forgetful {
    fn put(&self, _key: Vec<u8>, _value: Vec<u8>) -> bool {
        true
    }
    fn get(&self, _key: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn take(&self, _key: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn can_cache(&self) -> bool {
        true
    }
}

/// The UDP sockets QUIC listens on: 127.0.0.1, and ::1 where the
/// machine has IPv6.
struct QuicSockets {
    v4: std::net::UdpSocket,
    v6: Option<std::net::UdpSocket>,
}

/// A port in the dynamic range, 49152 to 65535.
fn random_port() -> u16 {
    use std::hash::{BuildHasher, RandomState};
    49152 + (RandomState::new().hash_one(()) % 16384) as u16
}

/// A certificate for `localhost` and `127.0.0.1` kept in `dir`, made the
/// first time: `cert.pem` to trust, and the DER the server loads. It's
/// valid for a year and marked for TLS servers, which macOS requires
/// before it will trust one. Returns (certificate, key) as DER.
pub fn lasting_certificate(dir: &Path) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
    if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        return Ok((cert, key));
    }
    let failed = |e: rcgen::Error| io::Error::other(e.to_string());
    let names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let mut params = rcgen::CertificateParams::new(names).map_err(failed)?;
    let (year, month, day) = today();
    params.not_before = rcgen::date_time_ymd(year, month, day);
    params.not_after = rcgen::date_time_ymd(year + 1, month, day.min(28));
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "netweir capture (localhost)");
    let key = rcgen::KeyPair::generate().map_err(failed)?;
    let cert = params.self_signed(&key).map_err(failed)?;
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("cert.pem"), cert.pem())?;
    std::fs::write(&cert_path, cert.der())?;
    std::fs::write(&key_path, key.serialize_der())?;
    // The key is nobody else's business, test certificate or not.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((cert.der().to_vec(), key.serialize_der()))
}

/// Today's date, (year, month, day), in UTC.
fn today() -> (i32, u8, u8) {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year as i32, month as u8, day as u8)
}

fn tls_config(alpn: &[&[u8]], (cert_der, key_der): (Vec<u8>, Vec<u8>)) -> rustls::ServerConfig {
    let cert = rustls::pki_types::CertificateDer::from(cert_der);
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into());
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .expect("certificate");
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    config
}

/// The page `/form` serves: a form a driver submits by clicking its
/// button, as a person would, so the browser marks it user-activated.
const FORM_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>form</title>
<form method="post" action="/submitted"><input name="q" value="rust"><input name="page" value="2"><button id="go" type="submit">Search</button></form>"#;

/// The page `/form-redirect` serves: a form whose POST is answered with a
/// redirect to `/after`, as a login's is.
const FORM_REDIRECT_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>form</title>
<form method="post" action="/redirect?to=/after"><input name="user" value="ada"><button id="go" type="submit">Sign in</button></form>"#;

/// The page `/link` serves: a link a driver clicks, as a person would.
const LINK_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>link</title>
<a id="go" href="/linked">Next page</a>"#;

/// The page `/fetch` serves: its script POSTs JSON as a page's own code
/// would, then says so in the title.
const FETCH_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>fetch</title>
<script>fetch("/api", {method: "POST", headers: {"Content-Type": "application/json"}, body: JSON.stringify({q: "rust", page: 2})}).then(() => { document.title = "posted" })</script>"#;

/// The page `/image` serves: an `<img>`, so the browser's request for a
/// picture is captured.
const IMAGE_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>image</title>
<img src="/picture.png" alt="">"#;

/// The page `/zero-rtt` serves: its script asks until a response came
/// over HTTP/3, waits for that connection to time out, then goes to
/// `/again`, whose request the browser may send in 0-RTT on the
/// connection that resumes.
const ZERO_RTT_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>zero-rtt</title>
<script>
(async () => {
  // With ?close, each probe comes after a TCP connection closed, for a
  // browser that moves to QUIC only on a new connection (Safari).
  const close = location.search.includes("close");
  for (let i = 0; i < 50; i++) {
    if (close) await fetch("/close").catch(() => {});
    const seen = await (await fetch("/probe?" + i)).json();
    if (seen.protocol === "h3") break;
    await new Promise(r => setTimeout(r, 200));
  }
  // The connection's ticket is taken, then the connection times out
  // (EARLY_IDLE), a close that leaves the browser's Alt-Svc in place.
  await new Promise(r => setTimeout(r, 6000));
  // With ?post, a POST opens the connection that resumes.
  if (location.search.includes("post")) {
    await fetch("/again", {method: "POST", body: "q=rust"}).catch(() => {});
    await new Promise(r => setTimeout(r, 6000));
    await fetch("/again").catch(() => {});
    return;
  }
  location.href = "/again";
})();
</script>"#;

/// The status, the headers, and a body of its own, or `None` to echo the
/// capture as JSON.
type Answer = (u16, Vec<(&'static str, String)>, Option<Vec<u8>>);

/// The page `/auth` serves once logged in: an image, a script's POST, then
/// a form the script submits, answered with a redirect, so each kind of
/// request goes out with the login the browser now holds. The URLs are
/// made from `location.origin`: relative ones would keep a login written
/// into the page's URL (`https://user:pass@host/auth`), and browsers
/// refuse a script's request to such a URL.
const AUTH_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>auth</title>
<form id="f" method="post"><input name="q" value="rust"></form>
<script>
const o = location.origin, f = document.getElementById("f");
f.action = o + "/redirect?to=/after-auth";
const img = document.createElement("img");
img.src = o + "/picture.png";
document.body.append(img);
fetch(o + "/api", {method: "POST", headers: {"Content-Type": "application/json"}, body: JSON.stringify({q: "rust"})}).then(() => setTimeout(() => f.submit(), 500));
</script>"#;

/// What to answer a request with, from its path and query, and whether it
/// carried Authorization.
/// `s` with each `%XX` turned into its byte.
fn percent_decoded(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        let hex = |b: Option<u8>| (b? as char).to_digit(16);
        out.push(match b {
            b'%' => {
                let (hi, lo) = (hex(bytes.next()), hex(bytes.next()));
                match hi.zip(lo) {
                    Some((hi, lo)) => (hi * 16 + lo) as u8,
                    None => b'%',
                }
            }
            b => b,
        });
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn route(path: &str, authorized: bool) -> Answer {
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let param = |name: &str| {
        query
            .split('&')
            .find_map(|kv| {
                kv.strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('='))
            })
            .map(str::to_string)
    };
    let mut headers = Vec::new();
    if let Some(cookie) = param("set") {
        headers.push(("set-cookie", format!("{cookie}=1; Path=/")));
    }
    if let Some(alt_svc) = param("alt-svc") {
        headers.push(("alt-svc", percent_decoded(&alt_svc)));
    }
    let html = |headers: &mut Vec<(&'static str, String)>, page: &str| {
        headers.push(("content-type", "text/html; charset=utf-8".to_string()));
        Some(page.as_bytes().to_vec())
    };
    match (route, param("to")) {
        ("/redirect", Some(to)) => {
            headers.push(("location", to));
            (302, headers, Some(Vec::new()))
        }
        ("/loop", _) => {
            headers.push(("location", "/loop".to_string()));
            (302, headers, Some(Vec::new()))
        }
        ("/form", _) => {
            let page = html(&mut headers, FORM_PAGE);
            (200, headers, page)
        }
        ("/fetch", _) => {
            let page = html(&mut headers, FETCH_PAGE);
            (200, headers, page)
        }
        ("/form-redirect", _) => {
            let page = html(&mut headers, FORM_REDIRECT_PAGE);
            (200, headers, page)
        }
        ("/auth", _) if !authorized => {
            headers.push(("www-authenticate", "Basic realm=\"netweir\"".to_string()));
            (401, headers, Some(b"log in".to_vec()))
        }
        ("/auth", _) => {
            let page = html(&mut headers, AUTH_PAGE);
            (200, headers, page)
        }
        ("/link", _) => {
            let page = html(&mut headers, LINK_PAGE);
            (200, headers, page)
        }
        ("/zero-rtt", _) => {
            let page = html(&mut headers, ZERO_RTT_PAGE);
            (200, headers, page)
        }
        ("/image", _) => {
            let page = html(&mut headers, IMAGE_PAGE);
            (200, headers, page)
        }
        _ => {
            headers.push(("content-type", "application/json".to_string()));
            (200, headers, None)
        }
    }
}

/// Every datagram that opens with a long header, by sender, kept until
/// its connection's first flight has been read from them.
type Seen = Arc<Mutex<HashMap<SocketAddr, Vec<Vec<u8>>>>>;

/// A UDP socket that copies what it receives into `seen` before quinn
/// reads it, so the client's first flight can be read back as it was sent.
#[derive(Debug)]
struct Tee {
    inner: Arc<dyn quinn::AsyncUdpSocket>,
    seen: Seen,
    /// The same datagrams, kept after the first flight is read from
    /// `seen`, for the 0-RTT packets among them.
    early: Seen,
    /// How long each datagram to the client is held back, if it is.
    hold: Option<std::time::Duration>,
}

impl quinn::AsyncUdpSocket for Tee {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        self.inner.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &quinn::udp::Transmit) -> io::Result<()> {
        let Some(hold) = self.hold else {
            return self.inner.try_send(transmit);
        };
        // Sent later, as over a network with that much delay one way; one
        // the socket has no room for then is lost, and QUIC sends it again.
        let inner = self.inner.clone();
        let (destination, ecn, segment_size, src_ip) = (
            transmit.destination,
            transmit.ecn,
            transmit.segment_size,
            transmit.src_ip,
        );
        let contents = transmit.contents.to_vec();
        tokio::spawn(async move {
            tokio::time::sleep(hold).await;
            let _ = inner.try_send(&quinn::udp::Transmit {
                destination,
                ecn,
                contents: &contents,
                segment_size,
                src_ip,
            });
        });
        Ok(())
    }
    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let poll = self.inner.poll_recv(cx, bufs, meta);
        if let Poll::Ready(Ok(n)) = poll {
            let (mut seen, mut early) = (self.seen.lock().unwrap(), self.early.lock().unwrap());
            for (buf, meta) in bufs.iter().zip(meta.iter()).take(n) {
                // One buffer can hold several datagrams of `stride` bytes.
                for datagram in buf[..meta.len].chunks(meta.stride.max(1)) {
                    if datagram.first().is_none_or(|b| b & 0x80 == 0) {
                        continue;
                    }
                    // A test tool's bound on what clients that never finish
                    // a handshake can make it hold.
                    if seen.len() > 1024 {
                        seen.clear();
                    }
                    if early.len() > 1024 {
                        early.clear();
                    }
                    let kept = early.entry(meta.addr).or_default();
                    if kept.len() == 64 {
                        kept.remove(0);
                    }
                    kept.push(datagram.to_vec());
                    // The newest 32: a first flight is read from the
                    // Initial with its connection ID on, so older ones a
                    // connection left are pushed out rather than in the way.
                    let kept = seen.entry(meta.addr).or_default();
                    if kept.len() == 32 {
                        kept.remove(0);
                    }
                    kept.push(datagram.to_vec());
                }
            }
        }
        poll
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }
    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }
    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// How long a connection to a server set up for a browser's `/zero-rtt`
/// lasts with nothing on it, so the next request opens one that resumes.
const EARLY_IDLE: std::time::Duration = std::time::Duration::from_secs(3);

/// Serves QUIC on UDP `addr`: each connection's first flight is read from
/// what the socket received, and its HTTP/3 requests are answered as the
/// TCP side answers them.
/// `quic`: whether it takes 0-RTT, how long a connection with nothing on
/// it lasts, and how long each datagram to the client is held back.
fn listen_quic(
    udp: std::net::UdpSocket,
    tls: rustls::ServerConfig,
    (early, idle, hold): (
        bool,
        Option<std::time::Duration>,
        Option<std::time::Duration>,
    ),
    connections: Arc<AtomicU64>,
    tx: mpsc::UnboundedSender<Capture>,
) -> io::Result<()> {
    let mut tls = tls;
    let secrets = Arc::new(EarlySecrets::default());
    if early {
        tls.key_log = secrets.clone();
    }
    let crypto =
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(io::Error::other)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    if let Some(idle) = idle {
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(idle.try_into().map_err(io::Error::other)?));
        config.transport_config(Arc::new(transport));
    }
    let runtime = Arc::new(quinn::TokioRuntime);
    let (seen, early_seen) = (Seen::default(), Seen::default());
    let socket = Arc::new(Tee {
        inner: quinn::Runtime::wrap_udp_socket(&*runtime, udp)?,
        seen: seen.clone(),
        early: early_seen.clone(),
        hold,
    });
    let endpoint = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(config),
        socket,
        runtime,
    )?;
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let (seen, connections, tx) = (seen.clone(), connections.clone(), tx.clone());
            let early_seen = early_seen.clone();
            let secrets = secrets.clone();
            tokio::spawn(async move {
                // Its datagrams were taken out when its first flight was
                // read. Taking out what's under its address now could take
                // a later connection's from the same address, as through a
                // relay; what an unfinished one left is pushed out instead.
                let early = early.then_some((&early_seen, &*secrets));
                let _ = handle_quic(incoming, early, &seen, &connections, tx).await;
            });
        }
    });
    Ok(())
}

/// Each connection's 0-RTT secret, by its ClientHello's random, as rustls
/// logs them.
#[derive(Debug, Default)]
struct EarlySecrets(Mutex<HashMap<Vec<u8>, Vec<u8>>>);

impl rustls::KeyLog for EarlySecrets {
    fn log(&self, label: &str, client_random: &[u8], secret: &[u8]) {
        if label == "CLIENT_EARLY_TRAFFIC_SECRET" {
            let mut secrets = self.0.lock().unwrap();
            // A test tool's bound, as on what the socket keeps.
            if secrets.len() > 1024 {
                secrets.clear();
            }
            secrets.insert(client_random.to_vec(), secret.to_vec());
        }
    }
    fn will_log(&self, label: &str) -> bool {
        label == "CLIENT_EARLY_TRAFFIC_SECRET"
    }
}

/// `early`, on a server that takes 0-RTT: what its socket received, kept
/// for the 0-RTT packets, and the secrets they are read with.
async fn handle_quic(
    incoming: quinn::Incoming,
    early: Option<(&Seen, &EarlySecrets)>,
    seen: &Seen,
    connections: &AtomicU64,
    tx: mpsc::UnboundedSender<Capture>,
) -> io::Result<()> {
    let (addr, odcid) = (incoming.remote_address(), incoming.orig_dst_cid());
    let conn = if early.is_some() {
        // A server's connection is always taken before its handshake is
        // done, so a request in 0-RTT can be read at once.
        let Ok((conn, _)) = incoming.accept().map_err(io::Error::other)?.into_0rtt() else {
            return Err(io::Error::other("a server connection always converts"));
        };
        conn
    } else {
        // The handshake is done, so the whole ClientHello has arrived.
        incoming.await.map_err(io::Error::other)?
    };
    let connection = connections.fetch_add(1, Ordering::Relaxed);
    // The first flight, read from what the socket received once all of the
    // ClientHello is in: at once when the handshake is done, and for a
    // connection taken before that, when its first request comes (which
    // the server can only read once it has the whole ClientHello) or it
    // ends. Its requests are read meanwhile: a client whose 0-RTT packets
    // follow its ClientHello's datagrams (quiche's) could otherwise finish
    // its handshake before the first one was read.
    let read_flight = || {
        let mut seen = seen.lock().unwrap();
        let flight = quic::first_flight(seen.get(&addr)?, &odcid)?;
        seen.remove(&addr);
        Some(flight)
    };
    let mut flight = if early.is_some() {
        None
    } else {
        Some(read_flight().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?)
    };
    let capture_of = |(quic, client_hello): &(Quic, ClientHello),
                      request,
                      http3,
                      method,
                      path,
                      headers,
                      body: &[u8]| Capture {
        connection,
        request,
        ja4: ja4_quic(client_hello),
        client_hello: client_hello.clone(),
        protocol: "h3".to_string(),
        http2: None,
        quic: Some(quic.clone()),
        http3,
        path,
        method,
        body: String::from_utf8_lossy(body).into_owned(),
        headers,
    };
    let mut request = 0u32;
    // Whether a request came in 0-RTT is read from the packets its stream
    // came in, not from how far the handshake had got by the time it was
    // taken: on a quick machine a client's Finished can be in before the
    // stream that came ahead of it is.
    let in_early_data = |stream: u64, hello: &ClientHello| {
        let Some((received, secrets)) = early else {
            return false;
        };
        let Some(secret) = secrets.0.lock().unwrap().get(&hello.random).cloned() else {
            return false;
        };
        let received = received.lock().unwrap();
        received
            .get(&addr)
            .is_some_and(|d| quic::zero_rtt_streams(d, &secret).contains(&stream))
    };
    let served = h3::serve(conn, |mut req| {
        let pseudo = |name: &str| {
            req.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let (method, path) = (pseudo(":method"), pseudo(":path"));
        let authorized = req.headers.iter().any(|(k, _)| k == "authorization");
        if flight.is_none() {
            flight = read_flight();
        }
        let Some(first) = flight.as_ref() else {
            return (
                500,
                Vec::new(),
                b"the first flight couldn't be read".to_vec(),
            );
        };
        req.http3.early = in_early_data(req.stream, &first.1);
        let capture = capture_of(
            first,
            request,
            Some(req.http3),
            method,
            path.clone(),
            req.headers,
            &req.body,
        );
        request += 1;
        let (status, headers, page) = route(&path, authorized);
        let body =
            page.unwrap_or_else(|| serde_json::to_vec(&capture).expect("capture serialises"));
        let _ = tx.send(capture);
        (status, headers, body)
    })
    .await;
    // Browsers open connections they never send a request on (Chrome's
    // next one then resumes its session); this keeps their first flights.
    if request == 0
        && let Some(first) = flight.or_else(read_flight)
    {
        let _ = tx.send(capture_of(
            &first,
            0,
            None,
            String::new(),
            String::new(),
            Vec::new(),
            &[],
        ));
    }
    served
}

async fn handle(
    connection: u64,
    mut stream: TcpStream,
    acceptor: TlsAcceptor,
    tx: mpsc::UnboundedSender<Capture>,
    alt_svc: Option<String>,
) -> io::Result<()> {
    let mut hello_bytes = Vec::with_capacity(2048);
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        hello_bytes.extend_from_slice(&buf[..n]);
        match bytes_needed(&hello_bytes) {
            Err(()) => return Err(io::ErrorKind::InvalidData.into()),
            Ok(Some(need)) if hello_bytes.len() >= need => break,
            Ok(_) if hello_bytes.len() > 64 * 1024 => return Err(io::ErrorKind::InvalidData.into()),
            Ok(_) => {}
        }
    }
    let client_hello = ClientHello::parse(&hello_bytes)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let ja4 = ja4(&client_hello);

    let mut tls = acceptor
        .accept(Replay {
            prefix: hello_bytes,
            pos: 0,
            inner: stream,
        })
        .await?;
    let protocol = match tls.get_ref().1.alpn_protocol() {
        Some(b"h2") => "h2",
        _ => "http/1.1",
    };
    let mut request = 0u32;
    let mut capture_of = |http2: Option<Http2>,
                          method: String,
                          path: String,
                          headers: Vec<(String, String)>,
                          body: Vec<u8>| {
        let capture = Capture {
            connection,
            request,
            ja4: ja4.clone(),
            client_hello: client_hello.clone(),
            protocol: protocol.to_string(),
            http2,
            quic: None,
            http3: None,
            path,
            method,
            body: String::from_utf8_lossy(&body).into_owned(),
            headers,
        };
        request += 1;
        capture
    };

    if protocol == "h2" {
        h2::serve(&mut tls, |req| {
            let pseudo = |name: &str| {
                req.headers
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            let (method, path) = (pseudo(":method"), pseudo(":path"));
            let authorized = req.headers.iter().any(|(k, _)| k == "authorization");
            let capture = capture_of(Some(req.http2), method, path.clone(), req.headers, req.body);
            let (status, mut headers, page) = route(&path, authorized);
            if !headers.iter().any(|(k, _)| *k == "alt-svc") {
                headers.extend(alt_svc.clone().map(|v| ("alt-svc", v)));
            }
            let body =
                page.unwrap_or_else(|| serde_json::to_vec(&capture).expect("capture serialises"));
            let _ = tx.send(capture);
            (status, headers, body)
        })
        .await
    } else {
        loop {
            let Some((method, path, headers)) = read_http1_head(&mut tls).await? else {
                return Ok(());
            };
            let length = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.parse::<usize>().ok())
                .unwrap_or(0);
            if length > 1 << 20 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut request_body = vec![0u8; length];
            tls.read_exact(&mut request_body).await?;
            let authorized = headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
            let capture = capture_of(None, method, path.clone(), headers, request_body);
            let (status, mut reply_headers, page) = route(&path, authorized);
            if !reply_headers.iter().any(|(k, _)| *k == "alt-svc") {
                reply_headers.extend(alt_svc.clone().map(|v| ("alt-svc", v)));
            }
            let body =
                page.unwrap_or_else(|| serde_json::to_vec(&capture).expect("capture serialises"));
            let _ = tx.send(capture);
            // `/close` answers and hangs up, so the client's next request
            // opens a new connection, which may resume this one's session.
            let close = path.split('?').next() == Some("/close");
            if close {
                reply_headers.push(("connection", "close".to_string()));
            }
            reply_headers.push(("content-length", body.len().to_string()));
            let reason = match status {
                302 => "Found",
                401 => "Unauthorized",
                _ => "OK",
            };
            let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
            for (k, v) in &reply_headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            tls.write_all(head.as_bytes()).await?;
            tls.write_all(&body).await?;
            tls.flush().await?;
            if close {
                tls.shutdown().await?;
                return Ok(());
            }
        }
    }
}

/// Reads one request head. `None` when the client closed the connection
/// between requests.
async fn read_http1_head<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> io::Result<Option<(String, String, Vec<(String, String)>)>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await? == 0 {
            return if head.is_empty() {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        if head.len() > 64 * 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or("GET").to_string();
    let path = first.next().unwrap_or("/").to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect();
    Ok(Some((method, path, headers)))
}

/// Serves `prefix` before reading from `inner`, so the TLS handshake sees
/// the ClientHello bytes we already consumed.
struct Replay<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Replay<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Replay<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod lasting {
    use super::*;

    #[test]
    fn the_lasting_certificate_is_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("netweir-capture-cert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = lasting_certificate(&dir).unwrap();
        assert!(dir.join("cert.pem").is_file());
        assert_eq!(
            lasting_certificate(&dir).unwrap(),
            first,
            "kept, not made again"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auth_asks_for_a_login_until_a_request_carries_one() {
        let (status, headers, _) = route("/auth", false);
        assert_eq!(status, 401);
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "www-authenticate" && v.starts_with("Basic"))
        );
        let (status, _, page) = route("/auth?again", true);
        assert_eq!(status, 200);
        assert!(String::from_utf8(page.unwrap()).unwrap().contains("fetch("));
    }

    #[test]
    fn today_is_a_real_date() {
        let (year, month, day) = today();
        assert!(year >= 2026 && (1..=12).contains(&month) && (1..=31).contains(&day));
    }
}
