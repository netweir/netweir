//! What the fetcher does when a TLS handshake fails, as each browser does
//! it. Firefox 160 makes a failed transaction again
//! (nsHttpTransaction::Restart) after a connection that was reset or
//! closed during the handshake, up to network.http.request.max-attempts
//! (10) connections, or, with an HTTPS record, after any handshake error
//! (PrepareConnInfoForRetry). Every connection after the first carries
//! IS_RETRY, and nsNSSIOLayer offers no certificate compression then. A
//! handshake_failure alert from a server with no HTTPS record isn't
//! tried again: security.tls.version.fallback-limit is TLS 1.3, so there
//! is no version to fall back to. Recorded from Firefox 160.0.1 for #436.
//! Chrome makes one connection either way.
//!
//! Firefox 161.0 does the same without Happy Eyeballs. With it, as
//! Mozilla's happy-eyeballs-release-rollout has it in every release
//! Firefox, the HTTPS records are part of the connection attempt, and
//! PrepareConnInfoForRetry never runs: a failed connection from a record
//! is made again only when it was reset or closed, where it went, with
//! IS_RETRY. A failed fast fallback is the request's failure the same way.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use netweir_core::{FetchOptions, Fetcher, Profile};
use netweir_fingerprint::Server;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const COMPRESS_CERTIFICATE: u16 = 27;

/// What the front does with a connection once it has its ClientHello.
#[derive(Clone, Copy)]
enum Then {
    /// Resets it.
    Reset,
    /// Closes it.
    Close,
    /// Answers with a fatal handshake_failure alert.
    Alert,
    /// Passes it on to this port, the ClientHello included.
    Relay(u16),
    /// The same, then closes it after this long, so it can't wait in the
    /// client's pool.
    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
    RelayFor(u16, Duration),
}

type Hellos = Arc<Mutex<Vec<Vec<u8>>>>;

/// A TCP port on 127.0.0.1 that reads each connection's ClientHello,
/// writes it down, and does with the connection what `then` says for its
/// number (from 0).
async fn front(then: impl Fn(usize) -> Then + Send + 'static) -> (u16, Hellos) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hellos: Hellos = Arc::default();
    let seen = hellos.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let mut record = vec![0; 5];
            if client.read_exact(&mut record).await.is_err() {
                continue;
            }
            let len = u16::from_be_bytes([record[3], record[4]]) as usize;
            let mut hello = vec![0; len];
            if client.read_exact(&mut hello).await.is_err() {
                continue;
            }
            let n = {
                let mut seen = seen.lock().unwrap();
                seen.push(hello.clone());
                seen.len() - 1
            };
            match then(n) {
                Then::Reset => {
                    let _ = client.set_zero_linger();
                }
                Then::Close => {
                    let _ = client.shutdown().await;
                }
                Then::Alert => {
                    let _ = client.write_all(&[21, 3, 3, 0, 2, 2, 40]).await;
                    let _ = client.shutdown().await;
                }
                Then::Relay(to) | Then::RelayFor(to, _) => {
                    let lasting = match then(n) {
                        Then::RelayFor(_, lasting) => lasting,
                        _ => Duration::MAX,
                    };
                    tokio::spawn(async move {
                        let mut server = tokio::net::TcpStream::connect(("127.0.0.1", to))
                            .await
                            .unwrap();
                        server.write_all(&record).await.unwrap();
                        server.write_all(&hello).await.unwrap();
                        let piped = tokio::io::copy_bidirectional(&mut client, &mut server);
                        let _ = tokio::time::timeout(lasting, piped).await;
                    });
                }
            }
        }
    });
    (port, hellos)
}

/// A ClientHello's extension types, in order.
fn extensions(hello: &[u8]) -> Vec<u16> {
    let mut at = 4 + 2 + 32;
    at += 1 + hello[at] as usize;
    at += 2 + u16::from_be_bytes([hello[at], hello[at + 1]]) as usize;
    at += 1 + hello[at] as usize;
    at += 2;
    let mut out = Vec::new();
    while at + 4 <= hello.len() {
        out.push(u16::from_be_bytes([hello[at], hello[at + 1]]));
        at += 4 + u16::from_be_bytes([hello[at + 2], hello[at + 3]]) as usize;
    }
    out
}

/// firefox-161-macos with Happy Eyeballs on or off.
#[cfg(feature = "http3")]
fn firefox(happy_eyeballs: bool) -> Profile {
    let mut profile = Profile::named("firefox-161-macos").unwrap();
    profile.http3.as_mut().unwrap().happy_eyeballs = happy_eyeballs;
    profile
}

fn fetcher(profile: &str) -> Fetcher {
    let mut options = FetchOptions::new(Profile::named(profile).unwrap());
    options.verify_certificates = false;
    Fetcher::new(options).unwrap()
}

/// Each hello after the first is the first's without
/// compress_certificate, as Firefox's retries were.
fn retried_as_firefox(hellos: &[Vec<u8>]) {
    let first = extensions(&hellos[0]);
    assert!(first.contains(&COMPRESS_CERTIFICATE), "{first:?}");
    let without: Vec<u16> = first
        .iter()
        .copied()
        .filter(|&e| e != COMPRESS_CERTIFICATE)
        .collect();
    for (i, hello) in hellos.iter().enumerate().skip(1) {
        assert_eq!(extensions(hello), without, "connection {i}");
        assert_eq!(hello.len(), hellos[0].len() - 11, "connection {i}");
    }
}

#[tokio::test]
async fn a_reset_handshake_is_made_again_without_certificate_compression() {
    let mut server = Server::start().await.unwrap();
    let to = server.port;
    let (port, hellos) = front(move |n| if n < 2 { Then::Reset } else { Then::Relay(to) }).await;
    let fetcher = fetcher("firefox-160-macos");
    let response = fetcher
        .get(&format!("https://127.0.0.1:{port}/"), &[])
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    let hellos = hellos.lock().unwrap().clone();
    assert_eq!(hellos.len(), 3);
    retried_as_firefox(&hellos);
    let capture = server.next().await.unwrap();
    assert!(
        !capture
            .client_hello
            .extensions
            .contains(&COMPRESS_CERTIFICATE)
    );
}

#[tokio::test]
async fn a_closed_handshake_is_made_ten_times_at_most() {
    let (port, hellos) = front(|_| Then::Close).await;
    let fetcher = fetcher("firefox-160-macos");
    let error = fetcher
        .get(&format!("https://127.0.0.1:{port}/"), &[])
        .await
        .unwrap_err();
    let hellos = hellos.lock().unwrap().clone();
    assert_eq!(hellos.len(), 10, "{error}");
    retried_as_firefox(&hellos);
}

#[tokio::test]
async fn a_handshake_failure_alert_is_not_made_again() {
    let (port, hellos) = front(|_| Then::Alert).await;
    let fetcher = fetcher("firefox-160-macos");
    fetcher
        .get(&format!("https://127.0.0.1:{port}/"), &[])
        .await
        .unwrap_err();
    assert_eq!(hellos.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn chrome_makes_a_reset_handshake_once() {
    let (port, hellos) = front(|_| Then::Reset).await;
    let fetcher = fetcher("chrome");
    fetcher
        .get(&format!("https://127.0.0.1:{port}/"), &[])
        .await
        .unwrap_err();
    assert_eq!(hellos.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_connection_a_retry_made_serves_the_next_request() {
    let mut server = Server::start().await.unwrap();
    let to = server.port;
    let (port, hellos) = front(move |n| if n < 1 { Then::Reset } else { Then::Relay(to) }).await;
    let fetcher = fetcher("firefox-160-macos");
    let url = format!("https://127.0.0.1:{port}/");
    fetcher.get(&url, &[]).await.unwrap();
    fetcher.get(&url, &[]).await.unwrap();
    server.next().await.unwrap();
    assert_eq!(hellos.lock().unwrap().len(), 2);
}

/// Where a test's HTTPS record sends origin.test's connections.
#[cfg(feature = "http3")]
#[derive(Clone, Copy, PartialEq)]
enum Place {
    /// The URL has no port, and the record none: the origin's own.
    Origin,
    /// The record names the front's port, so it has a routed host, and its
    /// route goes to the record's target (`connect_to`).
    OwnPort,
}

/// The hellos of the first request to origin.test, whose one HTTPS record
/// has `params` and whose every connection the front fails as `then`
/// says, that went by the record: the ones before it was in made one
/// connection. A record with ECH makes hellos that name ech.com; with
/// Happy Eyeballs, only those tell that the record was in.
#[cfg(feature = "http3")]
async fn once_the_record_is_in(
    params: &[(u16, Vec<u8>)],
    place: Place,
    profile: Profile,
    then: Then,
) -> Vec<Vec<u8>> {
    let happy_eyeballs = profile.http3.as_ref().is_some_and(|h| h.happy_eyeballs);
    let (port, hellos) = front(move |_| then).await;
    let mut params = params.to_vec();
    if place == Place::OwnPort {
        params.push((3, port.to_be_bytes().to_vec()));
        // A record's SvcParams go in key order.
        params.sort_by_key(|p| p.0);
    }
    let ech = params.iter().any(|p| p.0 == 5);
    let record = netweir_core::https_answer("origin.test", &[(1, ".", &params)], 300);
    let mut options = FetchOptions::new(profile);
    options.verify_certificates = false;
    options.resolve = vec![("origin.test".into(), ([127, 0, 0, 1], port).into())];
    options.https_records.insert("origin.test".into(), record);
    let fetcher = Fetcher::new(options).unwrap();
    let started = std::time::Instant::now();
    loop {
        let before = hellos.lock().unwrap().len();
        fetcher.get("https://origin.test/", &[]).await.unwrap_err();
        let made = hellos.lock().unwrap()[before..].to_vec();
        // With ECH, a request whose own connection was under way when the
        // record came in made that one without it first.
        let with_ech = |h: &Vec<u8>| h.windows(7).any(|w| w == b"ech.com");
        if (made.len() > 1 || happy_eyeballs && ech) && (!ech || made.iter().all(with_ech)) {
            return made;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the record never took"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// With HTTPS records, Firefox makes the transaction again after any
/// handshake error (PrepareConnInfoForRetry). A record without ECH sends
/// the retry to the origin, once.
#[cfg(feature = "http3")]
#[tokio::test]
async fn a_failed_handshake_from_a_record_without_ech_goes_to_the_origin_once() {
    let params = [(1, b"\x02h2\x08http/1.1".to_vec())];
    let made = once_the_record_is_in(&params, Place::Origin, firefox(false), Then::Alert).await;
    assert_eq!(made.len(), 2);
    retried_as_firefox(&made);
}

/// An ECHConfigList from BoringSSL's bssl generate-ech, public name
/// ech.com.
#[cfg(feature = "http3")]
fn ech_config_list() -> Vec<u8> {
    const LIST: &str = "003efe0d003a0000200020bb2f29e3e3057e0419d52fc5f44118776f8db61cea4fdf76079b93606c5a62480008000100010001000300076563682e636f6d0000";
    (0..LIST.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&LIST[i..i + 2], 16).unwrap())
        .collect()
}

/// With ECH, a failed record goes on to the record's other ALPN entries
/// (PrepareSVCBRecordsForRetry). `h2,http/1.1` flattens to http/1.1 and
/// the default ALPN; with a port of its own the record has a routed host,
/// so the failed one is left out and one connection follows it. The ECH
/// stays, and the retry leaves compress_certificate out.
#[cfg(feature = "http3")]
#[tokio::test]
async fn a_failed_handshake_with_ech_goes_through_the_records_alpns() {
    let params = [(1, b"\x02h2\x08http/1.1".to_vec()), (5, ech_config_list())];
    let made = once_the_record_is_in(&params, Place::OwnPort, firefox(false), Then::Alert).await;
    assert_eq!(made.len(), 2);
    retried_as_firefox(&made);
}

/// Firefox 160.0.1's three connections for `. alpn=h2,http/1.1 ech=...`
/// with the origin's port: the record keeps the origin's host and port,
/// so nsHttpConnectionInfo gives it no routed host, and
/// PrepareSVCBRecordsForRetry leaves nothing out after it failed. Its
/// first entry is tried again after the other.
#[cfg(feature = "http3")]
#[tokio::test]
async fn a_record_with_the_origins_port_is_tried_again_after_it_failed() {
    let params = [(1, b"\x02h2\x08http/1.1".to_vec()), (5, ech_config_list())];
    let made = once_the_record_is_in(&params, Place::Origin, firefox(false), Then::Alert).await;
    assert_eq!(made.len(), 3);
    retried_as_firefox(&made);
}

/// Firefox 161.0 with Happy Eyeballs, a record with ECH and the origin's
/// port, and a server that fails the handshake with an alert: one
/// connection, with ECH, and the error.
#[cfg(feature = "http3")]
#[tokio::test]
async fn with_happy_eyeballs_a_record_that_failed_is_not_tried_again() {
    let params = [(1, b"\x02h2\x08http/1.1".to_vec()), (5, ech_config_list())];
    let made = once_the_record_is_in(&params, Place::Origin, firefox(true), Then::Alert).await;
    assert_eq!(made.len(), 1);
}

/// The same with a server that resets the handshake: Firefox 161.0 made
/// ten connections to the record's address, the first with
/// compress_certificate, the rest without it, all with ECH
/// (nsHttpTransaction::Restart).
#[cfg(feature = "http3")]
#[tokio::test]
async fn with_happy_eyeballs_a_reset_record_is_made_again_where_it_went() {
    let params = [(1, b"\x02h2\x08http/1.1".to_vec()), (5, ech_config_list())];
    let made = once_the_record_is_in(&params, Place::Origin, firefox(true), Then::Reset).await;
    assert_eq!(made.len(), 10);
    retried_as_firefox(&made);
}

/// An h3 record whose QUIC goes unanswered, as Firefox 160.0.1 went on
/// from it (#436): after 50 ms the fast fallback's TCP connection, with
/// the profile's hello (FallbackTransaction, no IS_RETRY). Once its
/// handshake is over, even failed, the request moves to its route
/// (OnBackupConnectionReady, HandleFallback: a restart), leaving HTTP/3:
/// the request's own connection to the fallback's record and the
/// origin's after it are both made again, without compress_certificate.
/// Against a server whose handshake works, the request rides the
/// backup's connection and nothing more is made.
#[cfg(feature = "http3")]
#[tokio::test]
async fn a_failed_fast_fallback_goes_on_over_tcp_as_firefox_does() {
    let made = after_fast_fallback(firefox(false), Then::Alert, 3).await;
    retried_as_firefox(&made);
}

/// With Happy Eyeballs, Firefox 161.0's fast fallback came 49 ms after
/// QUIC as before, and its failure was the request's: one connection
/// after an alert. After a reset, ten, the first with
/// compress_certificate and the rest without.
#[cfg(feature = "http3")]
#[tokio::test]
async fn with_happy_eyeballs_a_failed_fast_fallback_is_the_requests_failure() {
    after_fast_fallback(firefox(true), Then::Alert, 1).await;
    let made = after_fast_fallback(firefox(true), Then::Reset, 10).await;
    retried_as_firefox(&made);
}

/// The TCP hellos of the first request to origin.test, whose h3 record
/// with a port of its own goes unanswered over QUIC and whose TCP
/// connections the front fails as `then` says, once it went over QUIC
/// and made `connections` of them.
#[cfg(feature = "http3")]
async fn after_fast_fallback(profile: Profile, then: Then, connections: usize) -> Vec<Vec<u8>> {
    let (port, hellos) = front(move |_| then).await;
    let quiet = std::net::UdpSocket::bind(("127.0.0.1", port)).unwrap();
    quiet.set_nonblocking(true).unwrap();
    let params = [
        (1, b"\x02h3\x02h2".to_vec()),
        (3, port.to_be_bytes().to_vec()),
    ];
    let record = netweir_core::https_answer("origin.test", &[(1, ".", &params)], 300);
    let mut options = FetchOptions::new(profile);
    options.verify_certificates = false;
    options.resolve = vec![("origin.test".into(), ([127, 0, 0, 1], port).into())];
    options.https_records.insert("origin.test".into(), record);
    let fetcher = Fetcher::new(options).unwrap();
    let started = std::time::Instant::now();
    loop {
        while quiet.recv(&mut [0; 2048]).is_ok() {}
        let before = hellos.lock().unwrap().len();
        let sent = std::time::Instant::now();
        fetcher.get("https://origin.test/", &[]).await.unwrap_err();
        let made = hellos.lock().unwrap()[before..].to_vec();
        // The first request's own connection may be under way before the
        // record is in, and then it sends no QUIC.
        let over_quic = quiet.recv(&mut [0; 2048]).is_ok();
        if over_quic && made.len() == connections {
            // Not held for HTTP/3's handshake timeout.
            assert!(sent.elapsed() < Duration::from_secs(5));
            return made;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{} connections",
            made.len()
        );
    }
}

/// An `h2` alternative whose handshake is reset: Firefox 160.0.1 made the
/// request again on the direct route, the origin's, without Alt-Used and
/// with IS_RETRY (nsHttpTransaction::Restart), so its connection to the
/// origin left compress_certificate out. The alternative's connection had
/// it.
#[cfg(feature = "http3")]
#[tokio::test]
async fn a_reset_h2_alternative_goes_on_at_the_origin_as_a_retry() {
    let origin_server = Server::start().await.unwrap();
    let alternative = Server::start().await.unwrap();
    let (to_origin, to_alternative) = (origin_server.port, alternative.port);
    // Neither first connection stays: the origin's carries the answer that
    // offers the alternative, the alternative's proves it.
    let short = Duration::from_millis(300);
    let (origin, origin_hellos) = front(move |n| match n {
        0 => Then::RelayFor(to_origin, short),
        _ => Then::Relay(to_origin),
    })
    .await;
    let (alt, alt_hellos) = front(move |n| match n {
        0 => Then::RelayFor(to_alternative, short),
        _ => Then::Reset,
    })
    .await;
    let mut options = FetchOptions::new(Profile::named("firefox-160-macos").unwrap());
    options.verify_certificates = false;
    options.resolve = vec![
        ("origin.test".into(), ([127, 0, 0, 1], origin).into()),
        ("alt.test".into(), ([127, 0, 0, 1], alt).into()),
    ];
    let fetcher = Fetcher::new(options).unwrap();
    let offer = format!("h2%3D%22alt.test%3A{alt}%22");
    fetcher
        .get(&format!("https://origin.test/?alt-svc={offer}"), &[])
        .await
        .unwrap();
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let (alt_before, origin_before) = (
            alt_hellos.lock().unwrap().len(),
            origin_hellos.lock().unwrap().len(),
        );
        let response = fetcher.get("https://origin.test/", &[]).await.unwrap();
        assert_eq!(response.status, 200);
        let alt_made = alt_hellos.lock().unwrap()[alt_before..].to_vec();
        if alt_made.is_empty() {
            assert!(started.elapsed() < Duration::from_secs(10), "never moved");
            continue;
        }
        // One connection to the alternative, which was reset, then the
        // origin's, made again.
        assert_eq!(alt_made.len(), 1);
        let origin_made = origin_hellos.lock().unwrap()[origin_before..].to_vec();
        assert_eq!(origin_made.len(), 1);
        let mut made = alt_made;
        made.extend(origin_made);
        retried_as_firefox(&made);
        let capture: netweir_fingerprint::Capture = serde_json::from_slice(&response.body).unwrap();
        assert!(!capture.headers.iter().any(|(k, _)| k == "alt-used"));
        return;
    }
}
