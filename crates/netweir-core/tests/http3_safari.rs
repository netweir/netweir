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
use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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

#[tokio::test]
async fn an_alt_svc_route_stays_on_http3_once_it_moves() {
    // Safari learned h3 from Alt-Svc over HTTP/2, moved on a new
    // connection, and from then on opened no TCP connection to the
    // origin: not while QUIC was up, and not when it opened a new one.
    let server = Server::start_h3(0, None).await.unwrap();
    let origin = format!("https://localhost:{}", server.port);
    let fetcher = fetcher();
    let first = fetcher.get(&format!("{origin}/"), &[]).await.unwrap();
    assert_eq!(first.version, "HTTP/2");
    fetcher.expire_tcp();
    for path in ["/moved", "/second", "/third"] {
        let response = fetcher.get(&format!("{origin}{path}"), &[]).await.unwrap();
        assert_eq!(response.version, "HTTP/3", "{path}");
    }
    fetcher.close_http3().await;
    let next = fetcher.get(&format!("{origin}/next"), &[]).await.unwrap();
    assert_eq!(next.version, "HTTP/3", "a new QUIC connection carries it");
}

/// When each datagram reached a socket, with its size.
type Arrivals = Arc<Mutex<Vec<(Instant, usize)>>>;

/// A UDP socket at `at` that answers nothing, and when each datagram
/// reached it, with its size; None where it can't be bound.
fn silent(at: SocketAddr) -> Option<Arrivals> {
    let socket = std::net::UdpSocket::bind(at).ok()?;
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while let Ok((n, _)) = socket.recv_from(&mut buf) {
            log.lock().unwrap().push((Instant::now(), n));
        }
    });
    Some(seen)
}

fn has_ipv6() -> bool {
    std::net::UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

/// `origin.test` at ::1 and then 127.0.0.1, as `localhost` resolves on
/// macOS, over TCP on `port`.
fn both_families(options: &mut FetchOptions, port: u16) {
    options.resolve = vec![
        ("origin.test".into(), (Ipv6Addr::LOCALHOST, port).into()),
        ("origin.test".into(), ([127, 0, 0, 1], port).into()),
    ];
}

#[tokio::test]
async fn quic_tries_the_other_family_250_ms_after_the_first() {
    // Safari tried QUIC to ::1, then to 127.0.0.1 250 ms later, for a
    // server that listened on 127.0.0.1 only, and its request went over
    // QUIC: TCP waited its turn after both.
    if !has_ipv6() {
        eprintln!("no IPv6 here");
        return;
    }
    let h3 = Server::start_h3_ipv4(0).await.unwrap();
    let v6 = silent((Ipv6Addr::LOCALHOST, h3.port).into()).unwrap();
    let tcp = Server::start().await.unwrap();
    let mut options = options();
    both_families(&mut options, tcp.port);
    options
        .https_records
        .insert("origin.test".into(), h3_record(h3.port));
    let fetcher = Fetcher::new(options).unwrap();
    let started = Instant::now();
    let response = fetcher.get("https://origin.test/", &[]).await.unwrap();
    let took = started.elapsed();
    assert_eq!(response.version, "HTTP/3");
    assert!(took >= Duration::from_millis(250), "{took:?}");
    let seen = v6.lock().unwrap().clone();
    assert_eq!(seen.first().map(|s| s.1), Some(1200), "{seen:?}");
}

#[tokio::test]
async fn tcp_waits_for_quic_on_both_families() {
    // Neither family answers QUIC: the second attempt goes 250 ms after
    // the first, to the other family, and TCP 250 ms after that.
    if !has_ipv6() {
        eprintln!("no IPv6 here");
        return;
    }
    let (port, v4, v6) = loop {
        let free = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let v4 = silent(([127, 0, 0, 1], port).into());
        let v6 = silent((Ipv6Addr::LOCALHOST, port).into());
        if let (Some(v4), Some(v6)) = (v4, v6) {
            break (port, v4, v6);
        }
    };
    let tcp = Server::start().await.unwrap();
    let mut options = options();
    both_families(&mut options, tcp.port);
    options
        .https_records
        .insert("origin.test".into(), h3_record(port));
    let fetcher = Fetcher::new(options).unwrap();
    let started = Instant::now();
    let response = fetcher.get("https://origin.test/", &[]).await.unwrap();
    let took = started.elapsed();
    assert_eq!(response.version, "HTTP/2");
    assert!(took >= Duration::from_millis(500), "{took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");
    let first = |seen: &Arrivals| seen.lock().unwrap().first().copied();
    let (Some((at6, n6)), Some((at4, n4))) = (first(&v6), first(&v4)) else {
        panic!("QUIC went to one family only");
    };
    assert_eq!((n6, n4), (1200, 1200));
    let gap = at4.saturating_duration_since(at6);
    assert!(gap >= Duration::from_millis(240), "{gap:?}");
    assert!(gap < Duration::from_millis(400), "{gap:?}");
}

/// profiles/safari-27-macos.h3-0rtt.capture.json and its -post twin:
/// Safari 27.2 on the fingerprint server's `/zero-rtt?close` (and
/// `?close,post`), from a server whose session tickets allow early data
/// and which held each datagram to Safari back 50 ms. Its script had TCP
/// closed under each probe, as Safari moves to QUIC only on a new
/// connection, asked until a response came over HTTP/3, let that
/// connection time out, and then went to /again: a navigation, sent in
/// 0-RTT on the connection that resumed, or a POST, which waited for the
/// handshake on it.
fn zero_rtt_recording(post: bool) -> Vec<Capture> {
    let name = if post { "h3-0rtt-post" } else { "h3-0rtt" };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../profiles/{PROFILE}.{name}.capture.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A Safari Fetcher whose next connection to `server` resumes, with a
/// ticket that allows early data.
async fn resuming(server: &mut Server) -> (Fetcher, String) {
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();
    fetcher
        .use_http3(&origin, &format!("127.0.0.1:{port}"))
        .unwrap();
    fetcher.get(&format!("{origin}/"), &[]).await.unwrap();
    assert!(!resumed(&server.next().await.unwrap()));
    fetcher.close_http3().await;
    (fetcher, origin)
}

/// Its ClientHello's extensions but GREASE, sorted, and its first
/// flight's datagrams.
fn zero_rtt_shape(c: &Capture) -> (Vec<u16>, Vec<(usize, Vec<String>)>) {
    let mut extensions: Vec<u16> = c
        .client_hello
        .extensions
        .iter()
        .copied()
        .filter(|&e| !netweir_fingerprint::is_grease(e))
        .collect();
    extensions.sort_unstable();
    let flight = c
        .quic
        .as_ref()
        .unwrap()
        .datagrams
        .iter()
        .map(|d| (d.size, d.packets.iter().map(|p| p.kind.clone()).collect()))
        .collect();
    (extensions, flight)
}

#[tokio::test]
async fn a_resumed_connection_sends_a_get_in_0rtt_as_safari_did() {
    let recording = zero_rtt_recording(false);
    let recorded = recording.iter().find(|c| c.path == "/again").unwrap();
    assert!(recorded.http3.as_ref().unwrap().early);
    let mut server = Server::start_h3_early(0, None, None).await.unwrap();
    let (fetcher, origin) = resuming(&mut server).await;
    let response = fetcher.get(&format!("{origin}/again"), &[]).await.unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let again = server.next().await.unwrap();
    assert!(resumed(&again));
    assert!(again.client_hello.extensions.contains(&0x2a), "early_data");
    assert!(again.http3.as_ref().unwrap().early, "the GET went in 0-RTT");
    assert_eq!(zero_rtt_shape(&again), zero_rtt_shape(recorded));
}

#[tokio::test]
async fn a_post_on_a_resumed_connection_waits_as_safaris_did() {
    let recording = zero_rtt_recording(true);
    let recorded = recording.iter().find(|c| c.method == "POST").unwrap();
    assert!(resumed(recorded));
    assert!(!recorded.http3.as_ref().unwrap().early);
    let mut server = Server::start_h3_early(0, None, None).await.unwrap();
    let (fetcher, origin) = resuming(&mut server).await;
    let form = Outgoing::form(
        b"q=rust".to_vec(),
        Some("application/x-www-form-urlencoded".into()),
        None,
    );
    let response = fetcher
        .send(&format!("{origin}/again"), &form, &[])
        .await
        .unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let post = server.next().await.unwrap();
    assert!(resumed(&post));
    assert_eq!(post.method, "POST");
    assert!(!post.http3.as_ref().unwrap().early, "the POST waited");
    assert_eq!(zero_rtt_shape(&post), zero_rtt_shape(recorded));
}

/// A UDP port on 127.0.0.1 that relays to and from `to`, holding back
/// the client's datagrams that start with a 0-RTT packet until it has sent
/// one that starts with a Handshake packet (its Finished), and sending
/// them after it, on each connection: what a quick machine's server sees
/// when its connection task gets to the request stream only once the
/// handshake is done.
async fn finished_first(to: u16) -> u16 {
    use std::sync::{Arc, Mutex};
    let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let port = socket.local_addr().unwrap().port();
    let upstream = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    upstream.connect(("127.0.0.1", to)).await.unwrap();
    let client = Arc::new(Mutex::new(None));
    {
        let (socket, upstream, client) = (socket.clone(), upstream.clone(), client.clone());
        tokio::spawn(async move {
            let (mut buf, mut held, mut finished) = (vec![0; 65536], Vec::new(), false);
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                *client.lock().unwrap() = Some(from);
                let kind = buf[0] & 0xf0;
                // An Initial: a new connection's, so far.
                if kind == 0xc0 {
                    finished = false;
                }
                if !finished && kind == 0xd0 {
                    held.push(buf[..n].to_vec());
                    continue;
                }
                let _ = upstream.send(&buf[..n]).await;
                if !finished && kind == 0xe0 {
                    finished = true;
                    for datagram in held.drain(..) {
                        let _ = upstream.send(&datagram).await;
                    }
                }
            }
        });
    }
    tokio::spawn(async move {
        let mut buf = vec![0; 65536];
        loop {
            let n = match upstream.recv(&mut buf).await {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(_) => return,
            };
            let to = *client.lock().unwrap();
            if let Some(to) = to {
                let _ = socket.send_to(&buf[..n], to).await;
            }
        }
    });
    port
}

#[tokio::test]
async fn a_get_sent_in_0rtt_is_seen_as_early_when_the_finished_overtakes_it() {
    // What the GET came in, not how far the handshake had got when the
    // server read it: here the server has the client's Finished before
    // the 0-RTT packets that carry the GET.
    let mut server = Server::start_h3_early(0, None, None).await.unwrap();
    let relay = finished_first(server.port).await;
    let origin = format!("https://localhost:{}", server.port);
    let fetcher = fetcher();
    fetcher
        .use_http3(&origin, &format!("127.0.0.1:{relay}"))
        .unwrap();
    fetcher.get(&format!("{origin}/"), &[]).await.unwrap();
    assert!(!resumed(&server.next().await.unwrap()));
    fetcher.close_http3().await;
    let response = fetcher.get(&format!("{origin}/again"), &[]).await.unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let again = server.next().await.unwrap();
    assert!(resumed(&again));
    assert!(again.http3.as_ref().unwrap().early, "the GET went in 0-RTT");
}

/// What Safari 27.2 sent a server offering a QPACK dynamic table, recorded
/// on the capture server's `/qpack` page (TCP offering HTTP/1.1 only, as Safari keeps to HTTP/2 rather than move to QUIC, and `?close`): `variant` "" offered
/// 64 KiB and 100 blocked streams, "-unblocked" 64 KiB and none,
/// "-1mib" 1 MiB and 100.
fn qpack_recording(variant: &str) -> Vec<Capture> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../profiles/{PROFILE}.h3-qpack{variant}.capture.json"
    ));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// What each recording offered.
const QPACK_OFFERS: [(&str, u64, u64); 3] = [
    ("", 65536, 100),
    ("-unblocked", 65536, 0),
    ("-1mib", 1 << 20, 100),
];

/// Over one HTTP/3 connection to a server offering `offer`: `page`
/// navigated to, then each of `requests` from it, one after another,
/// then a last fetch, once whatever the encoder still had to send is in.
/// Returns what the server saw, in order.
async fn qpack_run(
    offer: netweir_fingerprint::QpackOffer,
    page: &str,
    requests: &[(&str, bool)],
) -> Vec<Capture> {
    let mut server = Server::start_h3_qpack(0, None, offer, false).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();
    fetcher
        .use_http3(&origin, &format!("127.0.0.1:{port}"))
        .unwrap();
    let page = format!("{origin}{page}");
    fetcher.get(&page, &[]).await.unwrap();
    let script = Outgoing::fetch("GET", None, None, Some(page.clone()));
    let image = Outgoing::image(Some(page.clone()));
    for (path, is_image) in requests {
        let outgoing = if *is_image { &image } else { &script };
        let response = fetcher
            .send(&format!("{origin}{path}"), outgoing, &[])
            .await;
        assert_eq!(response.unwrap().version, "HTTP/3", "{path}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    fetcher
        .send(&format!("{origin}/qpack/end"), &script, &[])
        .await
        .unwrap();
    let mut seen = Vec::new();
    for _ in 0..requests.len() + 2 {
        seen.push(server.next().await.unwrap());
    }
    seen
}

/// The last request over HTTP/3 in `captures`.
fn last_h3(captures: &[Capture]) -> &netweir_fingerprint::Http3 {
    captures
        .iter()
        .rev()
        .find(|c| c.protocol == "h3")
        .and_then(|c| c.http3.as_ref())
        .unwrap()
}

#[tokio::test]
async fn requests_use_a_servers_qpack_table_as_safaris_did() {
    // The quiche fork's Mode::Safari. Safari's first request over HTTP/3,
    // the probe, went before the server's SETTINGS, as Netweir's
    // navigation does; neither counts toward what's sent twice. Its
    // favicon went over another connection.
    for (variant, capacity, blocked_streams) in QPACK_OFFERS {
        let recording = qpack_recording(variant);
        let offer = netweir_fingerprint::QpackOffer {
            capacity,
            blocked_streams,
        };
        let got = qpack_run(
            offer,
            "/qpack?close",
            &[
                ("/qpack/1", false),
                ("/qpack/2", false),
                ("/qpack/3", false),
                ("/qpack/4", false),
                ("/picture.png?1", true),
                ("/picture.png?2", true),
            ],
        )
        .await;
        let differences = netweir_fingerprint::qpack_differences(
            &recording,
            &got,
            &[
                "/qpack/1",
                "/qpack/2",
                "/qpack/3",
                "/qpack/4",
                "/picture.png?1",
                "/picture.png?2",
            ],
            6,
        );
        assert!(differences.is_empty(), "{variant}: {differences:#?}");
        let (recorded, sent) = (last_h3(&recording), last_h3(&got));
        assert_eq!(
            netweir_fingerprint::qpack_capacity(sent),
            netweir_fingerprint::qpack_capacity(recorded),
            "{variant}: the table's capacity"
        );
        assert_eq!(
            netweir_fingerprint::qpack_inserts(sent),
            netweir_fingerprint::qpack_inserts(recorded),
            "{variant}: entries inserted"
        );
    }
}

/// The capacities a QPACK encoder set the server's table to so far, in
/// order.
fn qpack_capacities(http3: &netweir_fingerprint::Http3) -> Vec<u64> {
    http3
        .encoder
        .iter()
        .filter_map(|i| match i {
            netweir_fingerprint::EncoderInstruction::SetCapacity { capacity } => Some(*capacity),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_resumed_connection_uses_the_qpack_table_in_0rtt_as_safaris_did() {
    // profiles/{PROFILE}.h3-0rtt-qpack.capture.json: Safari 27.2 on
    // /zero-rtt?close against a server that takes 0-RTT and offers a
    // 64 KiB table with 100 blocked streams. Safari sent nothing on its encoder stream in
    // 0-RTT, not even the table's capacity: it waits for the new SETTINGS.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../profiles/{PROFILE}.h3-0rtt-qpack.capture.json"
    ));
    let recording: Vec<Capture> =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let recorded = recording.iter().position(|c| c.path == "/again").unwrap();
    let want = recording[recorded].http3.as_ref().unwrap();
    assert!(want.early, "the recording's /again went in 0-RTT");
    let offer = netweir_fingerprint::QpackOffer {
        capacity: 65536,
        blocked_streams: 100,
    };
    let mut server = Server::start_h3_early_offering(0, None, None, Some(offer))
        .await
        .unwrap();
    let (fetcher, origin) = resuming(&mut server).await;
    fetcher.get(&format!("{origin}/again"), &[]).await.unwrap();
    let again = server.next().await.unwrap();
    assert!(resumed(&again));
    let got = again.http3.as_ref().unwrap();
    assert!(got.early, "the GET went in 0-RTT");
    let refers = |h: &netweir_fingerprint::Http3| {
        h.field_section.as_ref().unwrap().required_insert_count > 0
    };
    assert_eq!(refers(got), refers(want), "refers to the table in 0-RTT");
    assert_eq!(qpack_capacities(got), qpack_capacities(want), "in 0-RTT");
}

#[tokio::test]
async fn a_huffman_tie_goes_as_it_is_as_safaris_did() {
    // profiles/safari-27-macos.h3-qpack-ties.capture.json: Safari 27.2's
    // fetches on /qpack-ties sent headers whose Huffman code is as long as
    // the string: "?0", "&&&&", the name "&&" and the value "1". Safari
    // sent them as they are, in the field section and on the encoder
    // stream alike. Safari put those names among its own, where Netweir
    // adds a caller's after the profile's, so each field is compared by
    // its name.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../profiles/{PROFILE}.h3-qpack-ties.capture.json"
    ));
    let recording: Vec<Capture> =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let ties = [("x-tie", "?0"), ("x-tie-4", "&&&&"), ("&&", "1")];
    let offer = netweir_fingerprint::QpackOffer {
        capacity: 65536,
        blocked_streams: 100,
    };
    let mut server = Server::start_h3_qpack(0, None, offer, false).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();
    fetcher
        .use_http3(&origin, &format!("127.0.0.1:{port}"))
        .unwrap();
    let page = format!("{origin}/qpack-ties?close");
    fetcher.get(&page, &[]).await.unwrap();
    server.next().await.unwrap();
    let script = Outgoing::fetch("GET", None, None, Some(page.clone()));
    let extra: Vec<(String, String)> = ties
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    // Each tie field's representation and Huffman flags, by request.
    let lines = |c: &Capture| -> Vec<(String, String, Option<bool>, Option<bool>)> {
        let mut out: Vec<_> = c
            .http3
            .as_ref()
            .unwrap()
            .field_section
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .filter(|l| ties.iter().any(|(k, _)| *k == l.name))
            .map(|l| {
                (
                    l.name.clone(),
                    l.kind.clone(),
                    l.huffman_name,
                    l.huffman_value,
                )
            })
            .collect();
        out.sort();
        out
    };
    for path in ["/qpack/1", "/qpack/2", "/qpack/3", "/qpack/4"] {
        fetcher
            .send(&format!("{origin}{path}"), &script, &extra)
            .await
            .unwrap();
        let got = server.next().await.unwrap();
        let want = recording.iter().find(|c| c.path == path).unwrap();
        assert_eq!(lines(&got), lines(want), "{path}");
    }
    // The inserts of the tie fields, and their Huffman flags.
    let inserts = |c: &Capture| -> Vec<String> {
        let mut out: Vec<String> = c
            .http3
            .as_ref()
            .unwrap()
            .encoder
            .iter()
            .filter_map(|i| match i {
                netweir_fingerprint::EncoderInstruction::InsertWithLiteralName {
                    name,
                    huffman_name,
                    huffman_value,
                    ..
                } if ties.iter().any(|(k, _)| k == name) => {
                    Some(format!("{name} {huffman_name}/{huffman_value}"))
                }
                _ => None,
            })
            .collect();
        out.sort();
        out
    };
    fetcher
        .send(&format!("{origin}/qpack/end"), &script, &[])
        .await
        .unwrap();
    let last = server.next().await.unwrap();
    let recorded = recording
        .iter()
        .rev()
        .find(|c| c.protocol == "h3" && c.path.starts_with("/qpack/"))
        .unwrap();
    assert_eq!(inserts(&last), inserts(recorded));
}

/// Safari 27.2 against tables of 128 to 2048 bytes
/// (profiles/safari-27-macos.h3-qpack-{N}b.capture.json, the requests
/// over HTTP/3 of one of two runs at each; the two were the same at every
/// size), and how many of the page's requests the fork's Safari encoder
/// sends as Safari did. No one rule fits every run: this one, the best
/// fit, matches all of them at 128, 448, 512, 1536 and 2048 bytes and the
/// first few elsewhere (docs/design/http3.md).
const QPACK_SWEEP: [(u64, usize); 12] = [
    (128, 6),
    (192, 2),
    (256, 2),
    (320, 3),
    (384, 2),
    (448, 6),
    (512, 6),
    (640, 4),
    (768, 4),
    (1024, 5),
    (1536, 6),
    (2048, 6),
];

#[tokio::test]
async fn a_small_qpack_table_is_filled_as_safaris_was() {
    let paths = [
        "/qpack/1",
        "/qpack/2",
        "/qpack/3",
        "/qpack/4",
        "/picture.png?1",
        "/picture.png?2",
    ];
    let requests: Vec<(&str, bool)> = paths
        .iter()
        .map(|p| (*p, p.starts_with("/picture")))
        .collect();
    for (capacity, matching) in QPACK_SWEEP {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../profiles/{PROFILE}.h3-qpack-{capacity}b.capture.json"
        ));
        let recording: Vec<Capture> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let offer = netweir_fingerprint::QpackOffer {
            capacity,
            blocked_streams: 100,
        };
        let got = qpack_run(offer, "/qpack?close", &requests).await;
        let same = &paths[..matching];
        let differences =
            netweir_fingerprint::qpack_differences(&recording, &got, same, same.len());
        assert!(differences.is_empty(), "{capacity} bytes: {differences:#?}");
        // Where the rule stops fitting, it doesn't: a change there is news.
        if matching < paths.len() {
            let next = &paths[matching..=matching];
            let differences = netweir_fingerprint::qpack_differences(&recording, &got, next, 1);
            assert!(
                !differences.is_empty(),
                "{capacity} bytes: {} now matches",
                next[0]
            );
        }
    }
}
