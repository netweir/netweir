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

use std::sync::{Arc, Mutex};

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
                Then::Relay(to) => {
                    tokio::spawn(async move {
                        let mut server = tokio::net::TcpStream::connect(("127.0.0.1", to))
                            .await
                            .unwrap();
                        server.write_all(&record).await.unwrap();
                        server.write_all(&hello).await.unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
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

/// The hellos of the first request to origin.test, whose one HTTPS record
/// has `params` and whose every connection the front fails, that went by
/// the record: the ones before it was in made one connection. `with_port`
/// names the front's port in the record, for a record with ECH, whose
/// route goes to the record's target (`connect_to`); its hellos name
/// ech.com.
#[cfg(feature = "http3")]
async fn once_the_record_is_in(params: &[(u16, Vec<u8>)], with_port: bool) -> Vec<Vec<u8>> {
    let (port, hellos) = front(|_| Then::Alert).await;
    let mut params = params.to_vec();
    if with_port {
        params.push((3, port.to_be_bytes().to_vec()));
        // A record's SvcParams go in key order.
        params.sort_by_key(|p| p.0);
    }
    let params = params.as_slice();
    let record = netweir_core::https_answer("origin.test", &[(1, ".", params)], 300);
    let mut options = FetchOptions::new(Profile::named("firefox-160-macos").unwrap());
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
        let ech = |h: &Vec<u8>| h.windows(7).any(|w| w == b"ech.com");
        if made.len() > 1 && (!with_port || made.iter().all(ech)) {
            return made;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the record never took"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// With HTTPS records, Firefox makes the transaction again after any
/// handshake error (PrepareConnInfoForRetry). A record without ECH sends
/// the retry to the origin, once.
#[cfg(feature = "http3")]
#[tokio::test]
async fn a_failed_handshake_from_a_record_without_ech_goes_to_the_origin_once() {
    let made = once_the_record_is_in(&[(1, b"\x02h2\x08http/1.1".to_vec())], false).await;
    assert_eq!(made.len(), 2);
    retried_as_firefox(&made);
}

/// With ECH, a failed record goes on to the record's other ALPN entries
/// (PrepareSVCBRecordsForRetry). `h2,http/1.1` flattens to http/1.1 and
/// the default ALPN; with a port of its own the record has a routed host,
/// so the failed one is left out and one connection follows it. The ECH
/// stays, and the retry leaves compress_certificate out. (Firefox's three
/// connections were to a record with the origin's port, whose failed
/// entry is tried again: `https_rr::skipped_after`.)
#[cfg(feature = "http3")]
#[tokio::test]
async fn a_failed_handshake_with_ech_goes_through_the_records_alpns() {
    // From BoringSSL's bssl generate-ech, public name ech.com.
    const LIST: &str = "003efe0d003a0000200020bb2f29e3e3057e0419d52fc5f44118776f8db61cea4fdf76079b93606c5a62480008000100010001000300076563682e636f6d0000";
    let ech = (0..LIST.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&LIST[i..i + 2], 16).unwrap())
        .collect();
    let made = once_the_record_is_in(&[(1, b"\x02h2\x08http/1.1".to_vec()), (5, ech)], true).await;
    assert_eq!(made.len(), 2);
    retried_as_firefox(&made);
}
