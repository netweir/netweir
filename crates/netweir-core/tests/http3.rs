//! A request over HTTP/3 with the Firefox profile must look, on the wire,
//! like Firefox 160's: profiles/firefox-160-macos.h3.capture.json recorded
//! Firefox loading a page that set a cookie over HTTP/2, moving to HTTP/3
//! on the server's Alt-Svc, then typing /image into the address bar, which
//! loads /picture.png. Here netweir does the same against the same server,
//! and every request is compared with Firefox's.

#![cfg(feature = "http3")]

use std::path::PathBuf;

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

    // The cookie, over HTTP/2, as Firefox got it.
    let response = fetcher
        .get(&format!("{origin}/?set=nw"), &[])
        .await
        .unwrap();
    assert_eq!(response.version, "HTTP/2");
    assert_eq!(server.next().await.unwrap().protocol, "h2");

    fetcher
        .use_http3(&origin, &format!("localhost:{port}"))
        .unwrap();
    let page = format!("{origin}/image");
    let response = fetcher.get(&page, &[]).await.unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let navigation = server.next().await.unwrap();
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
