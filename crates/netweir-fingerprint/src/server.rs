//! A local HTTPS server that records how each client connects and what it
//! sends: the ClientHello, the opening HTTP/2 frames, and every request's
//! headers. Each response body is the recorded request as JSON.
//!
//! Requests can ask for behaviour through the query string:
//! `?set=NAME` adds `Set-Cookie: NAME=1; Path=/`, and
//! `/redirect?to=URL` answers 302 to URL, and `/loop` redirects to itself
//! with a relative `Location`. `/form` serves a page whose script submits a
//! form, POST to `/submitted`; `/fetch` serves one whose script POSTs JSON
//! to `/api` with `fetch()`. `/form-redirect` is a form that POSTs to a 302
//! (as a login does), and `/link` a page with a link to `/linked`.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

use crate::client_hello::{ClientHello, bytes_needed};
use crate::h2::{self, Http2};
use crate::ja4::ja4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    /// Which connection to this server, counting from 0.
    pub connection: u64,
    /// Which request on that connection, counting from 0.
    pub request: u32,
    pub ja4: String,
    pub client_hello: ClientHello,
    /// "h2" or "http/1.1".
    pub protocol: String,
    pub http2: Option<Http2>,
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
        Server::bind_with(port, alpn, Some(lasting_certificate(dir)?)).await
    }

    async fn bind(port: u16, alpn: &[&[u8]]) -> io::Result<Server> {
        Server::bind_with(port, alpn, None).await
    }

    async fn bind_with(
        port: u16,
        alpn: &[&[u8]],
        certificate: Option<(Vec<u8>, Vec<u8>)>,
    ) -> io::Result<Server> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let port = listener.local_addr()?.port();
        let acceptor = TlsAcceptor::from(Arc::new(tls_config(alpn, certificate)));
        let (tx, captures) = mpsc::unbounded_channel();
        let connections = Arc::new(AtomicU64::new(0));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                let id = connections.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    // A broken or non-TLS connection is just dropped.
                    let _ = handle(id, stream, acceptor, tx).await;
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

fn tls_config(alpn: &[&[u8]], certificate: Option<(Vec<u8>, Vec<u8>)>) -> rustls::ServerConfig {
    let (cert_der, key_der) = certificate.unwrap_or_else(|| {
        let names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        let made = rcgen::generate_simple_self_signed(names).expect("self-signed cert");
        (made.cert.der().to_vec(), made.signing_key.serialize_der())
    });
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

/// The status, the headers, and a body of its own, or `None` to echo the
/// capture as JSON.
type Answer = (u16, Vec<(&'static str, String)>, Option<Vec<u8>>);

/// What to answer a request with, from its path and query.
fn route(path: &str) -> Answer {
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
        ("/link", _) => {
            let page = html(&mut headers, LINK_PAGE);
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

async fn handle(
    connection: u64,
    mut stream: TcpStream,
    acceptor: TlsAcceptor,
    tx: mpsc::UnboundedSender<Capture>,
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
            let capture = capture_of(Some(req.http2), method, path.clone(), req.headers, req.body);
            let (status, headers, page) = route(&path);
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
            let capture = capture_of(None, method, path.clone(), headers, request_body);
            let (status, mut reply_headers, page) = route(&path);
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
            let reason = if status == 302 { "Found" } else { "OK" };
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
    fn today_is_a_real_date() {
        let (year, month, day) = today();
        assert!(year >= 2026 && (1..=12).contains(&month) && (1..=31).contains(&day));
    }
}
