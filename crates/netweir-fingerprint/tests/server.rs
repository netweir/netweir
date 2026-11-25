//! The capture server against a plain rustls client, whose ClientHello we
//! know enough about to check the parser.

use std::sync::Arc;

use netweir_fingerprint::{Server, is_grease};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

#[derive(Debug)]
struct AcceptAnything;

impl ServerCertVerifier for AcceptAnything {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_config(alpn: &[u8]) -> rustls::ClientConfig {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAnything))
    .with_no_client_auth();
    config.alpn_protocols = vec![alpn.to_vec()];
    config
}

async fn connect(port: u16) -> tokio_rustls::client::TlsStream<TcpStream> {
    let config = client_config(b"http/1.1");
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap()
}

/// Sends one HTTP/1.1 request and reads exactly one response (by
/// Content-Length), leaving the connection open.
async fn exchange(tls: &mut tokio_rustls::client::TlsStream<TcpStream>, path: &str) -> String {
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nX-Probe: 1\r\nAccept: */*\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        tls.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut body = vec![0u8; length];
    tls.read_exact(&mut body).await.unwrap();
    head + &String::from_utf8(body).unwrap()
}

async fn request(port: u16) -> String {
    exchange(&mut connect(port).await, "/").await
}

#[tokio::test]
async fn records_the_client_hello_and_echoes_it() {
    let mut server = Server::start().await.unwrap();
    let response = request(server.port).await;
    let capture = server.next().await.unwrap();

    let hello = &capture.client_hello;
    assert_eq!(hello.server_name.as_deref(), Some("localhost"));
    assert_eq!(hello.alpn, ["http/1.1"]);
    assert!(hello.supported_versions.contains(&0x0304));
    assert!(
        hello.ciphers.contains(&0x1301),
        "TLS_AES_128_GCM_SHA256 missing"
    );
    assert!(
        !hello.ciphers.iter().any(|&c| is_grease(c)),
        "rustls does not send GREASE"
    );
    assert!(capture.ja4.starts_with("t13d"), "{}", capture.ja4);
    assert!(
        capture.ja4.contains("h1_"),
        "ALPN http/1.1 gives h1: {}",
        capture.ja4
    );

    assert_eq!(capture.protocol, "http/1.1");
    let names: Vec<&str> = capture.headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["Host", "X-Probe", "Accept"]);
    assert!(capture.http2.is_none());

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(
        response.contains(&capture.ja4),
        "the body echoes the capture"
    );
    assert_eq!(
        (capture.connection, capture.request, capture.path.as_str()),
        (0, 0, "/")
    );
}

#[tokio::test]
async fn records_every_request_on_a_kept_alive_connection() {
    let mut server = Server::start_http1(0).await.unwrap();
    let mut tls = connect(server.port).await;
    exchange(&mut tls, "/one").await;
    exchange(&mut tls, "/two").await;
    let (a, b) = (server.next().await.unwrap(), server.next().await.unwrap());
    assert_eq!((a.connection, a.request, a.path.as_str()), (0, 0, "/one"));
    assert_eq!((b.connection, b.request, b.path.as_str()), (0, 1, "/two"));
    assert_eq!(a.ja4, b.ja4);
}

#[tokio::test]
async fn sets_cookies_and_redirects_on_request() {
    let server = Server::start().await.unwrap();
    let mut tls = connect(server.port).await;
    let set = exchange(&mut tls, "/page?set=session").await;
    assert!(set.contains("set-cookie: session=1; Path=/\r\n"), "{set}");
    let moved = exchange(&mut tls, "/redirect?to=https://127.0.0.1:9/next&set=hop").await;
    assert!(moved.starts_with("HTTP/1.1 302 Found\r\n"), "{moved}");
    assert!(
        moved.contains("location: https://127.0.0.1:9/next\r\n"),
        "{moved}"
    );
    assert!(moved.contains("set-cookie: hop=1; Path=/\r\n"), "{moved}");
}

#[tokio::test]
async fn hangs_up_on_anything_that_is_not_tls() {
    let server = Server::start().await.unwrap();
    let mut tcp = TcpStream::connect(("127.0.0.1", server.port))
        .await
        .unwrap();
    tcp.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(std::time::Duration::from_secs(2), tcp.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "server kept a non-TLS connection open: {read:?}"
    );
}

#[tokio::test]
async fn records_the_method_and_body_of_a_post() {
    let mut server = Server::start_http1(0).await.unwrap();
    let mut tls = connect(server.port).await;
    let body = "q=rust&page=2";
    let request = format!(
        "POST /submitted HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes()).await.unwrap();
    // The answer comes once the body has arrived, and the connection
    // carries on.
    let mut answer = [0u8; 15];
    tls.read_exact(&mut answer).await.unwrap();
    assert_eq!(&answer, b"HTTP/1.1 200 OK");
    let capture = server.next().await.unwrap();
    assert_eq!(capture.method, "POST");
    assert_eq!(capture.body, body);
    assert_eq!(capture.path, "/submitted");
}

#[tokio::test]
async fn serves_pages_whose_scripts_post() {
    let server = Server::start().await.unwrap();
    let mut tls = connect(server.port).await;
    let form = exchange(&mut tls, "/form").await;
    assert!(form.contains("content-type: text/html"), "{form}");
    assert!(
        form.contains(r#"<form method="post" action="/submitted""#),
        "{form}"
    );
    let fetch = exchange(&mut tls, "/fetch").await;
    assert!(fetch.contains("fetch(\"/api\""), "{fetch}");
}

/// An HTTP/3 frame, from values below 64 so each varint is one byte.
fn h3_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![kind, payload.len() as u8];
    out.extend_from_slice(payload);
    out
}

#[tokio::test]
async fn records_the_first_quic_flight_and_http3_request() {
    let mut server = Server::start_h3(0, None).await.unwrap();
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_config(b"h3")).unwrap();
    client.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
    let conn = client
        .connect(([127, 0, 0, 1], server.port).into(), "localhost")
        .unwrap()
        .await
        .unwrap();

    // The control stream: SETTINGS QPACK_MAX_TABLE_CAPACITY 0,
    // H3_DATAGRAM 1, and the reserved id 0x21 set to 7.
    let mut control = conn.open_uni().await.unwrap();
    let mut opening = vec![0x00];
    opening.extend(h3_frame(0x04, &[0x01, 0x00, 0x33, 0x01, 0x21, 0x07]));
    control.write_all(&opening).await.unwrap();

    // GET /h3 with ":method GET" and ":scheme https" from the static
    // table, ":authority" and ":path" by static name, then a literal
    // "priority: u=0, i".
    let mut block = vec![0x00, 0x00, 0xd1, 0xd7, 0x50, 9];
    block.extend_from_slice(b"localhost");
    block.extend_from_slice(&[0x51, 3]);
    block.extend_from_slice(b"/h3");
    block.extend_from_slice(&[0x27, 0x01]);
    block.extend_from_slice(b"priority");
    block.push(6);
    block.extend_from_slice(b"u=0, i");
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let mut request = h3_frame(0x21, &[]); // a reserved frame type first
    request.extend(h3_frame(0x01, &block));
    send.write_all(&request).await.unwrap();
    send.finish().unwrap();
    let response = recv.read_to_end(1 << 20).await.unwrap();

    let capture = server.next().await.unwrap();
    assert_eq!(capture.protocol, "h3");
    assert!(capture.ja4.starts_with("q13d"), "{}", capture.ja4);
    assert!(capture.ja4.contains("h3_"), "{}", capture.ja4);
    assert_eq!(capture.client_hello.alpn, ["h3"]);
    assert!(
        String::from_utf8_lossy(&response).contains(&capture.ja4),
        "the body echoes the capture"
    );

    let quic = capture.quic.as_ref().unwrap();
    assert_eq!(quic.version, 1);
    assert!(quic.dcid_len >= 8, "{}", quic.dcid_len);
    let first = &quic.datagrams[0];
    assert!(
        first.size >= 1200,
        "an Initial datagram is padded: {}",
        first.size
    );
    assert_eq!(first.packets[0].kind, "initial");
    assert!(
        first.packets[0]
            .frames
            .iter()
            .any(|f| matches!(f, netweir_fingerprint::Frame::Crypto { offset: 0, .. }))
    );
    let ids: Vec<u64> = quic.transport_parameters.iter().map(|p| p.id).collect();
    assert!(
        ids.contains(&0x0f),
        "initial_source_connection_id: {ids:x?}"
    );
    assert!(
        quic.transport_parameters.iter().any(|p| p.grease),
        "quinn sends a reserved parameter: {ids:x?}"
    );

    let http3 = capture.http3.as_ref().unwrap();
    assert_eq!(http3.streams.first().map(|s| s.1), Some(0x00));
    let settings: Vec<(u64, u64, bool)> = http3
        .settings
        .iter()
        .map(|s| (s.id, s.value, s.grease))
        .collect();
    assert_eq!(
        settings,
        [(0x01, 0, false), (0x33, 1, false), (0x21, 7, true)]
    );
    assert_eq!(http3.request_frames, [(0x21, String::new())]);
    let names: Vec<&str> = capture.headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [":method", ":scheme", ":authority", ":path", "priority"]
    );
    assert_eq!(
        (capture.method.as_str(), capture.path.as_str()),
        ("GET", "/h3")
    );
}

#[tokio::test]
async fn the_tcp_side_points_browsers_at_http3() {
    let server = Server::start_h3(0, None).await.unwrap();
    let response = request(server.port).await;
    let alt_svc = format!("alt-svc: h3=\":{}\"; ma=86400\r\n", server.port);
    assert!(response.contains(&alt_svc), "{response}");
    let plain = Server::start().await.unwrap();
    assert!(!request(plain.port).await.contains("alt-svc"));
}

#[test]
fn grease_values() {
    for v in [0x0a0a, 0x1a1a, 0xfafa] {
        assert!(is_grease(v));
    }
    for v in [0x0a1a, 0x1301, 0x0000, 0xffff] {
        assert!(!is_grease(v));
    }
}

// Other tests' QUIC clients bind UDP ports from the same ephemeral range
// the TCP port comes from, and one took the server's UDP port on macOS:
// "Address already in use". A neighbour binding and freeing UDP ports
// as fast as it can is that, many times over.
#[tokio::test]
async fn a_busy_udp_neighbour_never_takes_the_quic_port() {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let neighbour = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut held = std::collections::VecDeque::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(socket) = std::net::UdpSocket::bind("127.0.0.1:0") {
                    held.push_back(socket);
                }
                if held.len() > 2000 {
                    held.pop_front();
                }
            }
        })
    };
    for i in 0..300 {
        if let Err(e) = Server::start_h3(0, None).await {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            panic!("start {i}: {e}");
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    neighbour.join().unwrap();
}
