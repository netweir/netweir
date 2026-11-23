//! A request over HTTP/3 with the Firefox profile must look, on the wire,
//! like Firefox 160's: profiles/firefox-160-macos.h3.capture.json recorded
//! Firefox loading a page that set a cookie over HTTP/2, moving to HTTP/3
//! on the server's Alt-Svc, then typing /image into the address bar, which
//! loads /picture.png. Here netweir does the same against the same server,
//! and every request is compared with Firefox's. Then how the fetcher
//! moves to HTTP/3 and back off it, as Firefox does.

#![cfg(feature = "http3")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use netweir_core::{FetchOptions, Fetcher, Outgoing, Profile};
use netweir_fingerprint::{Capture, Quic, Server, differences, transport_parameter_order_differs};

const PROFILE: &str = "firefox-160-macos";
const PRE_SHARED_KEY: u16 = 0x0029;

fn recording() -> Vec<Capture> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../profiles/{PROFILE}.h3.capture.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn fetcher() -> Fetcher {
    let mut options = FetchOptions::new(Profile::named(PROFILE).unwrap());
    options.verify_certificates = false;
    Fetcher::new(options).unwrap()
}

fn resumed(c: &Capture) -> bool {
    c.client_hello.extensions.contains(&PRE_SHARED_KEY)
}

/// The first datagram's size: 1252 bytes over IPv4 and 1232 over IPv6.
fn first_datagram(q: &Quic) -> usize {
    q.datagrams[0].size
}

/// What Firefox draws at random for each connection, checked against
/// the range it draws from: the Destination Connection ID is 8 to 20
/// bytes, the first packet number below 1024 (neqo's `generate_initial`
/// and `randomize_first_pn`).
fn random_in_range(q: &Quic, what: &str) {
    assert!(
        (8..=20).contains(&q.dcid_len),
        "{what}: a {}-byte Destination Connection ID",
        q.dcid_len
    );
    let first = q
        .datagrams
        .iter()
        .flat_map(|d| &d.packets)
        .find_map(|p| p.packet_number)
        .unwrap();
    assert!(first < 1024, "{what}: first packet number {first}");
}

/// Firefox shuffles its ClientHello's extensions on every connection but
/// keeps quic_transport_parameters and encrypted_client_hello last, with
/// pre_shared_key after them when it resumes.
fn tail(c: &Capture) -> Vec<u16> {
    let e: Vec<u16> = (c.client_hello.extensions.iter().copied())
        .filter(|&x| x != PRE_SHARED_KEY)
        .collect();
    e[e.len() - 2..].to_vec()
}

/// `actual` against `expected`, field for field, then what's random
/// against its range.
fn check(expected: &Capture, actual: &Capture, what: &str) {
    let diff = differences(expected, actual);
    assert!(
        diff.is_empty(),
        "{what} differs from Firefox's:\n{}",
        diff.join("\n")
    );
    let (eq, aq) = (
        expected.quic.as_ref().unwrap(),
        actual.quic.as_ref().unwrap(),
    );
    if let Some(diff) = transport_parameter_order_differs(eq, aq) {
        panic!("{what}: {diff}");
    }
    assert_eq!(tail(actual), [0x39, 0xfe0d], "{what}: last extensions");
    random_in_range(aq, what);
}

/// Firefox's first flight on a fresh (or resumed) connection over the same
/// IP version as `actual`'s.
fn flight_like<'a>(recording: &'a [Capture], actual: &Capture) -> &'a Capture {
    let size = first_datagram(actual.quic.as_ref().unwrap());
    recording
        .iter()
        .find(|c| {
            c.quic.as_ref().is_some_and(|q| first_datagram(q) == size)
                && resumed(c) == resumed(actual)
        })
        .unwrap_or_else(|| panic!("no recorded flight of {size}-byte datagrams"))
}

#[tokio::test]
async fn requests_over_http3_are_firefoxs() {
    let recording = recording();
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();

    // The cookie, over HTTP/2, as Firefox got it, with the server's
    // Alt-Svc, which moves the next requests to HTTP/3 once a connection
    // has proved it.
    let response = fetcher
        .get(&format!("{origin}/?set=nw"), &[])
        .await
        .unwrap();
    assert_eq!(response.version, "HTTP/2");
    assert_eq!(server.next().await.unwrap().protocol, "h2");

    let page = format!("{origin}/image");
    let navigation = loop {
        let response = fetcher.get(&page, &[]).await.unwrap();
        let capture = server.next().await.unwrap();
        if response.version == "HTTP/3" {
            assert_eq!(response.status, 200);
            break capture;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let response = fetcher
        .send(
            &format!("{origin}/picture.png"),
            &Outgoing::image(Some(page)),
            &[],
        )
        .await
        .unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let image = server.next().await.unwrap();
    assert_eq!(
        (navigation.connection, image.connection),
        (navigation.connection, navigation.connection),
        "both requests go on one connection, as Firefox's did"
    );
    assert!(!resumed(&navigation));

    // Firefox's image priority depends on the page's layout: the recording
    // has u=4 and u=5. The profile sends u=4, i, as Firefox did over TCP;
    // compare with a connection that sent that.
    let recorded = recording
        .iter()
        .find(|c| {
            c.path == "/picture.png"
                && c.headers
                    .iter()
                    .any(|(k, v)| k == "priority" && v == "u=4, i")
        })
        .unwrap();
    let pair = |path: &str| {
        recording
            .iter()
            .find(|c| c.connection == recorded.connection && c.path == path)
            .unwrap()
            .clone()
    };
    for (mut expected, actual, what) in [
        (pair("/image"), &navigation, "a typed navigation"),
        (pair("/picture.png"), &image, "an image"),
    ] {
        // Its request, on a first flight like ours.
        expected.quic = flight_like(&recording, actual).quic.clone();
        check(&expected, actual, what);
    }
}

#[tokio::test]
async fn a_second_connection_resumes_like_firefox_and_shuffles_anew() {
    let recording = recording();
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let first = fetcher();
    // Firefox only resumed over IPv6 in the recording, so this test goes
    // to ::1 rather than wherever `localhost` resolves first.
    first.use_http3(&origin, &format!("[::1]:{port}")).unwrap();
    first.get(&format!("{origin}/"), &[]).await.unwrap();
    let fresh = server.next().await.unwrap();
    // The same Fetcher's next connection resumes with the ticket the
    // first one got, once that one has gone.
    first.close_http3().await;
    first.get(&format!("{origin}/"), &[]).await.unwrap();
    let again = server.next().await.unwrap();
    assert_ne!(fresh.connection, again.connection);
    assert!(resumed(&again), "the second connection resumed");
    for (actual, what) in [(&fresh, "a fresh connection"), (&again, "a resumed one")] {
        let mut expected = flight_like(&recording, actual).clone();
        // The recording's flights carry no request: the HTTP/3 connection
        // is compared with the one Firefox's typed address went on, and
        // the request itself is the other test's.
        let typed = recording.iter().find(|c| c.path == "/image").unwrap();
        expected.http3 = typed.http3.clone();
        (expected.method, expected.path) = (actual.method.clone(), actual.path.clone());
        expected.headers = actual.headers.clone();
        check(&expected, actual, what);
    }
    assert_ne!(
        fresh.client_hello.extensions, again.client_hello.extensions,
        "the extensions go in a new order on each connection"
    );
}

#[tokio::test]
async fn a_form_posts_its_body_and_redirects_are_followed_over_http3() {
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();
    fetcher
        .use_http3(&origin, &format!("localhost:{port}"))
        .unwrap();
    let form = Outgoing::form(
        b"user=ada".to_vec(),
        Some("application/x-www-form-urlencoded".into()),
        Some(format!("{origin}/form")),
    );
    let response = fetcher
        .send(&format!("{origin}/redirect?to=/after"), &form, &[])
        .await
        .unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let (post, after) = (server.next().await.unwrap(), server.next().await.unwrap());
    assert_eq!(
        (post.protocol.as_str(), post.method.as_str()),
        ("h3", "POST")
    );
    assert_eq!(post.body, "user=ada");
    let header = |c: &Capture, name: &str| {
        c.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(header(&post, "content-length").as_deref(), Some("8"));
    // The 302 turns the POST into a GET, on the same connection.
    assert_eq!(
        (after.method.as_str(), after.path.as_str()),
        ("GET", "/after")
    );
    assert_eq!(after.connection, post.connection);
    // The order is the TCP profile's for each, less Host and Connection,
    // with Alt-Used where Connection was.
    let profile = Profile::named(PROFILE).unwrap();
    for (capture, shape) in [
        (&post, profile.form.unwrap()),
        (&after, profile.after_form_redirect.unwrap()),
    ] {
        let sent: Vec<&str> = capture
            .headers
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !k.starts_with(':'))
            .collect();
        let shaped: Vec<String> = shape
            .headers
            .iter()
            .map(|(k, _)| k.to_ascii_lowercase())
            .filter(|k| k != "host")
            .map(|k| {
                if k == "connection" {
                    "alt-used".into()
                } else {
                    k
                }
            })
            .filter(|k| sent.contains(&k.as_str()))
            .collect();
        assert_eq!(sent, shaped, "{}", capture.path);
    }
}

#[tokio::test]
async fn certificates_are_checked_over_http3() {
    let dir = std::env::temp_dir().join(format!("netweir-h3-cert-{}", std::process::id()));
    let mut server = Server::start_h3(0, Some(&dir)).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let alternative = format!("localhost:{port}");
    // Nothing vouches for the test server's certificate...
    let strict = Fetcher::new(FetchOptions::new(Profile::named(PROFILE).unwrap())).unwrap();
    strict.use_http3(&origin, &alternative).unwrap();
    let error = strict.get(&format!("{origin}/"), &[]).await.unwrap_err();
    assert_eq!(error.kind, netweir_core::FetchErrorKind::Tls, "{error}");
    // ...until it's trusted as a CA.
    let mut options = FetchOptions::new(Profile::named(PROFILE).unwrap());
    options.ca_certificates = Some(std::fs::read(dir.join("cert.pem")).unwrap());
    let trusting = Fetcher::new(options).unwrap();
    trusting.use_http3(&origin, &alternative).unwrap();
    let response = trusting.get(&format!("{origin}/"), &[]).await.unwrap();
    assert_eq!(response.version, "HTTP/3");
    while let Ok(Some(c)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), server.next()).await
    {
        assert!(c.protocol == "h3");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn http3_only_for_a_profile_that_has_it_and_never_through_a_proxy() {
    let chrome = Fetcher::new(FetchOptions::new(Profile::named("chrome").unwrap())).unwrap();
    assert!(
        chrome
            .use_http3("https://example.com", "example.com:443")
            .is_err()
    );
    let mut options = FetchOptions::new(Profile::named(PROFILE).unwrap());
    options.proxy = Some("http://127.0.0.1:9".into());
    let proxied = Fetcher::new(options).unwrap();
    assert!(
        proxied
            .use_http3("https://example.com", "example.com:443")
            .is_err()
    );
    assert!(
        fetcher()
            .use_http3("http://example.com", "example.com:443")
            .is_err()
    );
}

/// `value` percent-encoded, for the test server's `?alt-svc=`.
fn encoded(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A URL on `origin` whose answer carries `Alt-Svc: value`.
fn offering(origin: &str, value: &str) -> String {
    format!("{origin}/?alt-svc={}", encoded(value))
}

/// Requests `url` until an answer comes over HTTP/3, as it does once the
/// connection the fetcher made to prove the alternative is up.
async fn until_http3(fetcher: &Fetcher, url: &str) {
    let started = Instant::now();
    while fetcher.get(url, &[]).await.unwrap().version != "HTTP/3" {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{url} never moved to HTTP/3"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A UDP port on 127.0.0.1 that passes datagrams on to `to` and back
/// while `drop` is false, and drops them all while it's true, as a network
/// that blocks QUIC; `seen` counts what reached it from the client.
struct Relay {
    port: u16,
    drop: Arc<AtomicBool>,
    seen: Arc<AtomicUsize>,
}

async fn relay(to: Option<u16>) -> Relay {
    let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let port = socket.local_addr().unwrap().port();
    let (drop, seen) = (
        Arc::new(AtomicBool::new(to.is_none())),
        Arc::new(AtomicUsize::new(0)),
    );
    let upstream = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    if let Some(to) = to {
        upstream.connect(("127.0.0.1", to)).await.unwrap();
    }
    let client = Arc::new(std::sync::Mutex::new(None));
    {
        let (socket, upstream, client) = (socket.clone(), upstream.clone(), client.clone());
        let (drop, seen) = (drop.clone(), seen.clone());
        tokio::spawn(async move {
            let mut buf = vec![0; 65536];
            loop {
                let (n, from) = match socket.recv_from(&mut buf).await {
                    Ok(got) => got,
                    // Windows reports an earlier datagram's ICMP error here.
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                    Err(_) => return,
                };
                seen.fetch_add(1, Ordering::Relaxed);
                if !drop.load(Ordering::Relaxed) {
                    *client.lock().unwrap() = Some(from);
                    let _ = upstream.send(&buf[..n]).await;
                }
            }
        });
    }
    {
        let drop = drop.clone();
        tokio::spawn(async move {
            let mut buf = vec![0; 65536];
            loop {
                let n = match upstream.recv(&mut buf).await {
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                    Err(_) => return,
                };
                let to = *client.lock().unwrap();
                if let Some(to) = to
                    && !drop.load(Ordering::Relaxed)
                {
                    let _ = socket.send_to(&buf[..n], to).await;
                }
            }
        });
    }
    Relay { port, drop, seen }
}

#[tokio::test]
async fn a_site_that_offers_http3_gets_its_next_requests_over_it() {
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();
    let first = fetcher.get(&format!("{origin}/"), &[]).await.unwrap();
    assert_eq!(first.version, "HTTP/2");
    until_http3(&fetcher, &format!("{origin}/")).await;
    let response = fetcher.get(&format!("{origin}/again"), &[]).await.unwrap();
    assert_eq!(response.version, "HTTP/3");
    let last = loop {
        let capture = server.next().await.unwrap();
        if capture.path == "/again" {
            break capture;
        }
    };
    assert_eq!(last.protocol, "h3");
    let alt_used = last.headers.iter().find(|(k, _)| k == "alt-used");
    assert_eq!(
        alt_used.map(|(_, v)| v.as_str()),
        Some(format!("localhost:{port}").as_str())
    );
}

#[tokio::test]
async fn alt_svc_clear_and_max_age_are_honoured() {
    // The alternative is another server's UDP port, so the TCP origin's
    // own answers carry only the Alt-Svc each request asks for.
    let h3 = Server::start_h3(0, None).await.unwrap();
    let tcp = Server::start().await.unwrap();
    let origin = format!("https://localhost:{}", tcp.port);
    let to = format!("localhost:{}", h3.port);
    let plain = format!("{origin}/");
    let fetcher = fetcher();

    // ma=3: HTTP/3 for three seconds, then TCP again.
    let learnt = Instant::now();
    let offered = fetcher
        .get(&offering(&origin, &format!("h3=\"{to}\"; ma=3")), &[])
        .await
        .unwrap();
    assert_eq!(offered.version, "HTTP/2");
    until_http3(&fetcher, &plain).await;
    assert!(
        learnt.elapsed() < Duration::from_secs(3),
        "too slow to tell"
    );
    tokio::time::sleep_until((learnt + Duration::from_millis(3200)).into()).await;
    assert_eq!(fetcher.get(&plain, &[]).await.unwrap().version, "HTTP/2");

    // A day by default, until the origin says `clear`, over HTTP/3 here.
    fetcher
        .get(&offering(&origin, &format!("h3=\"{to}\"")), &[])
        .await
        .unwrap();
    until_http3(&fetcher, &plain).await;
    let cleared = fetcher.get(&offering(&origin, "clear"), &[]).await.unwrap();
    assert_eq!(cleared.version, "HTTP/3");
    assert_eq!(fetcher.get(&plain, &[]).await.unwrap().version, "HTTP/2");
}

#[tokio::test]
async fn a_network_that_drops_udp_falls_back_to_tcp_and_stops_trying_http3() {
    let h3 = Server::start_h3(0, None).await.unwrap();
    let tcp = Server::start().await.unwrap();
    let origin = format!("https://localhost:{}", tcp.port);
    let plain = format!("{origin}/");
    let udp = relay(Some(h3.port)).await;
    let offer = offering(&origin, &format!("h3=\"127.0.0.1:{}\"", udp.port));

    // An alternative proved while UDP got through...
    let proved = fetcher();
    proved.get(&offer, &[]).await.unwrap();
    until_http3(&proved, &plain).await;
    proved.close_http3().await;
    // ...and another learnt as UDP stops getting through.
    udp.drop.store(true, Ordering::Relaxed);
    let learning = fetcher();
    let before = udp.seen.load(Ordering::Relaxed);
    assert_eq!(learning.get(&offer, &[]).await.unwrap().version, "HTTP/2");

    // A request to the proved one waits Firefox's 100 ms backup timer for
    // the new connection, then goes over TCP without fail.
    let started = Instant::now();
    let response = proved.get(&plain, &[]).await.unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/2"));
    assert!(started.elapsed() < Duration::from_secs(5));
    // The unproved one stays on TCP while its connection is tried.
    assert_eq!(learning.get(&offer, &[]).await.unwrap().version, "HTTP/2");
    assert!(udp.seen.load(Ordering::Relaxed) > before, "QUIC was tried");

    // Neqo gives up after Firefox's 30 s idle timeout. The host is then
    // excluded, as in Firefox: no more QUIC to it, and no new attempt
    // when UDP comes back, until the network changes (see the unit test
    // hosts_http3_failed_to_reach_stay_excluded_until_the_network_changes).
    tokio::time::sleep_until((started + Duration::from_secs(33)).into()).await;
    udp.drop.store(false, Ordering::Relaxed);
    let tried = udp.seen.load(Ordering::Relaxed);
    for fetcher in [&proved, &learning] {
        for _ in 0..2 {
            let response = fetcher.get(&offer, &[]).await.unwrap();
            assert_eq!(response.version, "HTTP/2");
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        udp.seen.load(Ordering::Relaxed),
        tried,
        "QUIC was tried again"
    );
}

/// A proxy that tunnels each CONNECT to where it asks.
async fn tunnel() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                let Some(to) = head.split_whitespace().nth(1) else {
                    return;
                };
                let Ok(mut site) = tokio::net::TcpStream::connect(to).await else {
                    return;
                };
                let _ = client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut site).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Asks `fetcher` three times for an origin offering HTTP/3 on a UDP
/// port, and gives how many datagrams reached the port.
async fn datagrams_sent(fetcher: &Fetcher) -> usize {
    let tcp = Server::start().await.unwrap();
    let origin = format!("https://localhost:{}", tcp.port);
    let udp = relay(None).await;
    let offer = offering(&origin, &format!("h3=\"127.0.0.1:{}\"", udp.port));
    for _ in 0..3 {
        let response = fetcher.get(&offer, &[]).await.unwrap();
        assert_eq!(response.version, "HTTP/2");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    udp.seen.load(Ordering::Relaxed)
}

#[tokio::test]
async fn a_proxied_fetch_never_sends_a_udp_packet() {
    let mut options = FetchOptions::new(Profile::named(PROFILE).unwrap());
    options.verify_certificates = false;
    options.proxy = Some(tunnel().await);
    assert_eq!(datagrams_sent(&Fetcher::new(options).unwrap()).await, 0);
    // The same Fetcher without one would have.
    assert!(datagrams_sent(&fetcher()).await > 0);
}

#[tokio::test]
async fn chrome_safari_and_http3_off_never_send_quic() {
    for profile in ["chrome", "safari", PROFILE] {
        let mut options = FetchOptions::new(Profile::named(profile).unwrap());
        options.verify_certificates = false;
        options.http3 = false;
        if profile != PROFILE {
            // Without an [http3] section, it makes no difference.
            options.http3 = true;
        }
        let fetcher = Fetcher::new(options).unwrap();
        assert_eq!(datagrams_sent(&fetcher).await, 0, "{profile}");
    }
}

#[tokio::test]
async fn the_archive_records_http3() {
    let dir = std::env::temp_dir().join(format!("netweir-h3-archive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (warc, har) = (dir.join("h3.warc"), dir.join("h3.har"));
    let archive = netweir_core::Archive::new(Some(&warc), Some(&har), false).unwrap();
    archive.start().unwrap();
    let server = Server::start_h3(0, None).await.unwrap();
    let origin = format!("https://localhost:{}", server.port);
    let mut options = FetchOptions::new(Profile::named(PROFILE).unwrap());
    options.verify_certificates = false;
    options.archive = Some(archive.clone());
    let fetcher = Fetcher::new(options).unwrap();
    fetcher
        .use_http3(&origin, &format!("localhost:{}", server.port))
        .unwrap();
    let response = fetcher.get(&format!("{origin}/"), &[]).await.unwrap();
    assert_eq!(response.version, "HTTP/3");
    archive.close().unwrap();
    let records = String::from_utf8_lossy(&std::fs::read(&warc).unwrap()).into_owned();
    assert_eq!(
        records.matches("WARC-Protocol: h3\r\n").count(),
        2,
        "{records}"
    );
    let har: serde_json::Value = serde_json::from_slice(&std::fs::read(&har).unwrap()).unwrap();
    let entry = &har["log"]["entries"][0];
    assert_eq!(entry["request"]["httpVersion"], "HTTP/3");
    assert_eq!(entry["response"]["httpVersion"], "HTTP/3");
    let _ = std::fs::remove_dir_all(&dir);
}
