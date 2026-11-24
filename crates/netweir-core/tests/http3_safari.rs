//! A request over HTTP/3 with the Safari profile must look, on the wire,
//! like Safari 27's: profiles/safari-27-macos.h3.capture.json recorded
//! Safari 27.2 on macOS moving to HTTP/3 on the fingerprint server's
//! Alt-Svc, opening /image, which loads /picture.png, on fresh and resumed
//! connections. Safari's QUIC is Apple's Network.framework, and netweir's
//! is its quiche fork set up from that recording: everything the
//! recording shows the same on every connection is compared exactly, the
//! QUIC Initial's layout included; what Safari draws at random (where the
//! transport parameters start, the GREASE values) is checked against the
//! range it was seen to draw from. Then how the fetcher moves a Safari
//! profile to HTTP/3, as Safari was seen to.

#![cfg(feature = "http3")]

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use netweir_core::{FetchOptions, Fetcher, Outgoing, Profile};
use netweir_fingerprint::{Capture, Server, differences};

const PROFILE: &str = "safari-27-macos";
const PRE_SHARED_KEY: u16 = 0x0029;
/// The transport parameters Safari sent, ascending; each connection
/// starts them at a random one.
const PARAMETERS: [u64; 7] = [4, 5, 6, 7, 9, 0xe, 0xf];

/// The recording, with the profile's User-Agent in place of Safari
/// 27.2's: the profile is Safari 27.0's headers, and 27.2's TCP
/// ClientHello and headers are 27.0's but for the version in it.
fn recording() -> Vec<Capture> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../profiles/{PROFILE}.h3.capture.json"));
    let mut captures: Vec<Capture> =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let profile = Profile::named(PROFILE).unwrap();
    let agent = profile
        .headers
        .iter()
        .find(|(k, _)| k == "User-Agent")
        .map(|(_, v)| v.clone())
        .unwrap();
    for c in &mut captures {
        for (k, v) in &mut c.headers {
            if k == "user-agent" {
                assert!(v.contains("Version/27.2 "), "{v}");
                *v = agent.clone();
            }
        }
    }
    captures
}

fn options() -> FetchOptions {
    let mut options = FetchOptions::new(Profile::named(PROFILE).unwrap());
    options.verify_certificates = false;
    options
}

fn fetcher() -> Fetcher {
    Fetcher::new(options()).unwrap()
}

fn resumed(c: &Capture) -> bool {
    c.client_hello.extensions.contains(&PRE_SHARED_KEY)
}

/// A recorded HTTP/3 request to `path` on a connection that resumed, or
/// didn't.
fn recorded<'a>(recording: &'a [Capture], path: &str, resumed_: bool) -> &'a Capture {
    recording
        .iter()
        .find(|c| c.protocol == "h3" && c.path == path && resumed(c) == resumed_)
        .unwrap_or_else(|| panic!("no recorded {path}, resumed {resumed_}"))
}

/// What Safari recorded on a fresh connection, carrying the request it
/// sent for `path` on a resumed one: Safari's only fresh HTTP/3
/// connections in the recording carried its favicon.
fn fresh_with(recording: &[Capture], path: &str) -> Capture {
    let mut c = recorded(recording, "/favicon.ico", false).clone();
    let request = recorded(recording, path, true);
    c.headers = request.headers.clone();
    c.path = request.path.clone();
    c
}

/// The transport parameters, with where they start made the same, as
/// each connection draws it anew; checked to be a rotation of Safari's
/// order first.
fn settle(c: &mut Capture, what: &str) -> u64 {
    let q = c.quic.as_mut().unwrap();
    let ids: Vec<u64> = q.transport_parameters.iter().map(|p| p.id).collect();
    let at = PARAMETERS
        .iter()
        .position(|id| Some(id) == ids.first())
        .unwrap_or_else(|| panic!("{what}: starts with {ids:?}"));
    let mut rotated = PARAMETERS.to_vec();
    rotated.rotate_left(at);
    assert_eq!(ids, rotated, "{what}: Safari's order, from a random place");
    q.transport_parameters.sort_by_key(|p| p.id);
    ids[0]
}

/// `actual` against `expected` field for field, the Initial's layout
/// included. Gives where `actual`'s transport parameters started.
fn check(expected: &Capture, actual: &Capture, what: &str) -> u64 {
    let (mut expected, mut actual) = (expected.clone(), actual.clone());
    settle(&mut expected, "the recording");
    let start = settle(&mut actual, what);
    let diff = differences(&expected, &actual);
    assert!(
        diff.is_empty(),
        "{what} differs from Safari's:\n{}",
        diff.join("\n")
    );
    let h3 = actual.http3.as_ref().unwrap();
    assert!(h3.control_frames.is_empty(), "{what}: no GREASE frame");
    start
}

/// Leaves `cookie` out, for a request on a fresh connection that Safari
/// made with the cookie its earlier ones had set.
fn without_cookie(mut c: Capture) -> Capture {
    c.headers.retain(|(k, _)| k != "cookie");
    c
}

#[tokio::test]
async fn requests_over_http3_are_safaris() {
    let recording = recording();
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();
    fetcher
        .use_http3(&origin, &format!("127.0.0.1:{port}"))
        .unwrap();

    let page = format!("{origin}/image?set=nw");
    let response = fetcher.get(&page, &[]).await.unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let navigation = server.next().await.unwrap();
    let response = fetcher
        .send(
            &format!("{origin}/picture.png"),
            &Outgoing::image(Some(format!("{origin}/image"))),
            &[],
        )
        .await
        .unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let image = server.next().await.unwrap();
    assert_eq!(image.connection, navigation.connection);
    assert!(!resumed(&navigation));
    check(
        &without_cookie(fresh_with(&recording, "/image")),
        &navigation,
        "the navigation",
    );
    check(&fresh_with(&recording, "/picture.png"), &image, "the image");
}

#[tokio::test]
async fn a_second_connection_resumes_like_safari() {
    let recording = recording();
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();
    fetcher
        .use_http3(&origin, &format!("127.0.0.1:{port}"))
        .unwrap();
    fetcher
        .get(&format!("{origin}/image?set=nw"), &[])
        .await
        .unwrap();
    let fresh = server.next().await.unwrap();
    fetcher.close_http3().await;
    fetcher.get(&format!("{origin}/image"), &[]).await.unwrap();
    let again = server.next().await.unwrap();
    assert_ne!(fresh.connection, again.connection);
    assert!(resumed(&again), "the second connection resumed");
    check(
        recorded(&recording, "/image", true),
        &again,
        "a resumed connection",
    );
}

/// Safari keeps its ClientHello's extensions in one order, but starts its
/// transport parameters at a random one and draws new GREASE.
#[tokio::test]
async fn many_connections_vary_as_safaris_do() {
    let recording = recording();
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let (mut starts, mut extensions, mut settings) =
        (HashSet::new(), HashSet::new(), HashSet::new());
    const N: usize = 24;
    for i in 0..N {
        let fetcher = fetcher();
        fetcher
            .use_http3(&origin, &format!("127.0.0.1:{port}"))
            .unwrap();
        fetcher.get(&format!("{origin}/image"), &[]).await.unwrap();
        let c = server.next().await.unwrap();
        starts.insert(check(
            &without_cookie(fresh_with(&recording, "/image")),
            &c,
            &format!("connection {i}"),
        ));
        extensions.insert(
            c.client_hello
                .extensions
                .iter()
                .map(|&e| if e & 0x0f0f == 0x0a0a { 0 } else { e })
                .collect::<Vec<_>>(),
        );
        let h3 = c.http3.as_ref().unwrap();
        settings.extend(h3.settings.iter().filter(|s| s.grease).map(|s| s.id));
    }
    assert!(starts.len() > 3, "where the parameters start: {starts:?}");
    assert_eq!(extensions.len(), 1, "one extension order");
    assert_eq!(settings.len(), N, "a new GREASE setting each time");
}

/// An HTTPS record for origin.test saying h3 on `port`.
fn h3_record(port: u16) -> Vec<u8> {
    let alpn = b"\x02h3".to_vec();
    netweir_core::https_answer(
        "origin.test",
        &[(1, ".", &[(1, alpn), (3, port.to_be_bytes().to_vec())])],
        300,
    )
}

#[tokio::test]
async fn an_https_record_takes_safari_to_http3_on_first_contact() {
    // Safari asks for HTTPS records alongside the addresses, and an h3
    // one sends its first connection over QUIC.
    let h3 = Server::start_h3(0, None).await.unwrap();
    let tcp = Server::start().await.unwrap();
    let mut options = options();
    options.resolve = vec![("origin.test".into(), ([127, 0, 0, 1], tcp.port).into())];
    options
        .https_records
        .insert("origin.test".into(), h3_record(h3.port));
    let fetcher = Fetcher::new(options).unwrap();
    let response = fetcher.get("https://origin.test/", &[]).await.unwrap();
    assert_eq!(response.version, "HTTP/3");
}

#[tokio::test]
async fn alt_svc_waits_for_a_new_connection() {
    // Safari kept its HTTP/2 connection after learning an alternative,
    // and went to HTTP/3 only on a new one.
    let server = Server::start_h3(0, None).await.unwrap();
    let origin = format!("https://localhost:{}", server.port);
    let fetcher = fetcher();
    let first = fetcher.get(&format!("{origin}/"), &[]).await.unwrap();
    assert_eq!(first.version, "HTTP/2");
    for _ in 0..3 {
        let next = fetcher.get(&format!("{origin}/"), &[]).await.unwrap();
        assert_eq!(next.version, "HTTP/2", "the open connection carries it");
    }
}

#[tokio::test]
async fn a_silent_alternative_loses_the_race_after_250_ms() {
    // Safari started TCP 250 ms after its first QUIC Initial, and kept to
    // the TCP connection after that.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let tcp = Server::start().await.unwrap();
    let mut options = options();
    options.resolve = vec![("origin.test".into(), ([127, 0, 0, 1], tcp.port).into())];
    options.https_records.insert(
        "origin.test".into(),
        h3_record(silent.local_addr().unwrap().port()),
    );
    let fetcher = Fetcher::new(options).unwrap();
    let started = Instant::now();
    let response = fetcher.get("https://origin.test/", &[]).await.unwrap();
    let took = started.elapsed();
    assert_eq!(response.version, "HTTP/2");
    assert!(took >= Duration::from_millis(250), "{took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");
    let mut buf = [0u8; 2048];
    silent.set_nonblocking(true).unwrap();
    let (n, _) = silent
        .recv_from(&mut buf)
        .expect("a QUIC Initial came first");
    assert_eq!(n, 1200);
    let started = Instant::now();
    let again = fetcher.get("https://origin.test/again", &[]).await.unwrap();
    assert_eq!(again.version, "HTTP/2");
    assert!(started.elapsed() < Duration::from_millis(250));
}

#[tokio::test]
async fn a_connection_up_over_http3_from_a_record_keeps_the_origin() {
    // Safari opened no TCP connection once QUIC was up first: the next
    // requests went on the QUIC connection, whatever Alt-Svc it answered.
    // cloudflare-quic.com's record carries an ECH configuration too,
    // which Safari's QUIC hello doesn't use.
    let h3 = Server::start_h3(0, None).await.unwrap();
    let tcp = Server::start().await.unwrap();
    let mut options = options();
    options.resolve = vec![("origin.test".into(), ([127, 0, 0, 1], tcp.port).into())];
    let alpn = b"\x02h3".to_vec();
    let ech = vec![0, 4, 0xfe, 0x0d, 0, 0];
    options.https_records.insert(
        "origin.test".into(),
        netweir_core::https_answer(
            "origin.test",
            &[(
                1,
                ".",
                &[(1, alpn), (3, h3.port.to_be_bytes().to_vec()), (5, ech)],
            )],
            300,
        ),
    );
    let fetcher = Fetcher::new(options).unwrap();
    for path in ["/", "/second", "/third"] {
        let response = fetcher
            .get(&format!("https://origin.test{path}"), &[])
            .await
            .unwrap();
        assert_eq!(response.version, "HTTP/3", "{path}");
    }
}

#[tokio::test]
async fn a_new_connection_after_http3_goes_over_quic_again() {
    // Safari went back to QUIC first on each new connection: a response
    // over HTTP/3 leaves no TCP connection open to keep the origin on.
    let h3 = Server::start_h3(0, None).await.unwrap();
    let tcp = Server::start().await.unwrap();
    let mut options = options();
    options.resolve = vec![("origin.test".into(), ([127, 0, 0, 1], tcp.port).into())];
    options
        .https_records
        .insert("origin.test".into(), h3_record(h3.port));
    let fetcher = Fetcher::new(options).unwrap();
    let first = fetcher.get("https://origin.test/", &[]).await.unwrap();
    assert_eq!(first.version, "HTTP/3");
    fetcher.close_http3().await;
    let next = fetcher.get("https://origin.test/next", &[]).await.unwrap();
    assert_eq!(next.version, "HTTP/3", "a new QUIC connection carries it");
}
