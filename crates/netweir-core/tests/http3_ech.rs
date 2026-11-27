//! What the Firefox profile does when an HTTP/3 server rejects the ECH
//! configuration an HTTPS record gave, against a neqo server that can
//! hold ECH keys of its own. NSS serves the server's certificate from
//! nss-rs's test database, so every test here starts NSS with it, in a
//! test binary of its own.

#![cfg(feature = "http3")]

use std::cell::RefCell;
use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use neqo_common::{Datagram, Tos};
use neqo_http3::{Header, Http3Parameters, Http3Server, Http3ServerEvent, Output};
use neqo_transport::RandomConnectionIdGenerator;
use netweir_core::{FetchOptions, Fetcher, Profile};

/// An ECHConfigList from BoringSSL's `bssl generate-ech`, public name
/// ech.com, that no test server holds the key to.
const ECH_CONFIG_LIST: &str = "003efe0d003a0000200020bb2f29e3e3057e0419d52fc5f44118776f8db61cea4fdf76079b93606c5a62480008000100010001000300076563682e636f6d0000";

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// What the server saw: the client ports QUIC came from, one per
/// connection, and for each request it answered whether ECH was
/// accepted on its connection.
#[derive(Default)]
struct Seen {
    ports: HashSet<u16>,
    accepted: Vec<bool>,
}

/// An HTTP/3 server on 127.0.0.1, with ECH keys of its own if `ech`,
/// which answers every request with 200, each datagram `slow` after it
/// came.
fn server(ech: bool, slow: Duration) -> (u16, Arc<Mutex<Seen>>) {
    // nss-rs's test-fixture/db (a key and certificate for "key"), copied:
    // nss-rs no longer exports its path, and the crate that does isn't
    // published.
    nss_rs::init_db(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/nss-db")).unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let local = socket.local_addr().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(5)))
        .unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let seeing = seen.clone();
    std::thread::spawn(move || {
        let now = Instant::now();
        let replay = nss_rs::AntiReplay::new(now, Duration::from_secs(10), 7, 14).unwrap();
        let mut server = Http3Server::new(
            now,
            &["key"],
            &["h3"],
            replay,
            Rc::new(RefCell::new(RandomConnectionIdGenerator::new(8))),
            Http3Parameters::default(),
            None,
        )
        .unwrap();
        if ech {
            let (sk, pk) = nss_rs::generate_ech_keys().unwrap();
            server.enable_ech(7, "public.example", &sk, &pk).unwrap();
        }
        let mut buf = vec![0; 65536];
        loop {
            let input = match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    seeing.lock().unwrap().ports.insert(from.port());
                    std::thread::sleep(slow);
                    Some(Datagram::new(from, local, Tos::default(), &buf[..n]))
                }
                Err(_) => None,
            };
            let mut out = server.process(input, Instant::now());
            loop {
                while let Some(event) = server.next_event() {
                    if let Http3ServerEvent::Headers { stream, .. } = event {
                        let accepted = stream
                            .conn
                            .borrow()
                            .tls_info()
                            .is_some_and(nss_rs::SecretAgentInfo::ech_accepted);
                        seeing.lock().unwrap().accepted.push(accepted);
                        let _ = stream.send_headers(&[Header::new(":status", "200")]);
                        let _ = stream.stream_close_send(Instant::now());
                    }
                }
                match out {
                    Output::Datagram(d) => {
                        let to: SocketAddr = d.destination();
                        let _ = socket.send_to(&d, to);
                        out = server.process_output(Instant::now());
                    }
                    _ => break,
                }
            }
        }
    });
    (local.port(), seen)
}

/// A Fetcher with the Firefox profile whose origin.test has one HTTPS
/// record: h3 at `port`, with the ECH configuration no server holds.
fn fetcher(port: u16) -> Fetcher {
    let record = netweir_core::https_answer(
        "origin.test",
        &[(
            1,
            ".",
            &[
                (1, b"\x02h3".to_vec()),
                (3, port.to_be_bytes().to_vec()),
                (5, hex(ECH_CONFIG_LIST)),
            ],
        )],
        300,
    );
    let mut options = FetchOptions::new(Profile::named("firefox-160-macos").unwrap());
    options.verify_certificates = false;
    options.resolve = vec![("origin.test".into(), ([127, 0, 0, 1], port).into())];
    options.https_records.insert("origin.test".into(), record);
    Fetcher::new(options).unwrap()
}

#[tokio::test]
async fn a_rejected_ech_is_tried_again_with_the_servers_configurations() {
    // The server holds other keys, so it rejects the record's ECH and
    // sends its own configurations in retry_configs. neqo fails the
    // handshake with them (Error::EchRetry), and Firefox connects to the
    // same record again with them (SSL_ERROR_ECH_RETRY_WITH_ECH in
    // nsHttpTransaction::PrepareConnInfoForRetry). The server then accepts
    // ECH, and answers over HTTP/3.
    retries_with_the_servers_configurations(Duration::ZERO).await;
}

#[tokio::test]
async fn a_rejection_after_the_fast_fallback_timer_is_tried_again_once() {
    // The same, with the handshake failing after Firefox's 50 ms
    // fast-fallback timer: still one connection again, with the
    // configurations, and none beside it.
    retries_with_the_servers_configurations(Duration::from_millis(40)).await;
}

/// A request to a server that rejects ECH and sends configurations to
/// retry with, each of whose datagrams is answered `slow` after it came:
/// two connections, the second accepted.
async fn retries_with_the_servers_configurations(slow: Duration) {
    let (port, seen) = server(true, slow);
    let fetcher = fetcher(port);
    let started = Instant::now();
    // What each fetch before the HTTP/3 answer came back with, so a
    // failure says why it took more than one.
    let mut before = Vec::new();
    loop {
        let answer = tokio::time::timeout(
            Duration::from_secs(5),
            fetcher.get("https://origin.test/", &[]),
        )
        .await
        .expect("an answer in time");
        match answer {
            Ok(answer) if answer.version == "HTTP/3" => break,
            Ok(answer) => before.push(format!("answered over {}", answer.version)),
            Err(e) => before.push(format!("failed: {e:?}")),
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "never answered over HTTP/3; fetches: {before:?}"
        );
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.accepted, [true], "fetches before HTTP/3: {before:?}");
    assert_eq!(
        seen.ports.len(),
        2,
        "one connection rejected, one accepted; fetches before HTTP/3: {before:?}"
    );
}

#[tokio::test]
async fn a_rejected_ech_with_nothing_to_retry_with_rules_http3_out() {
    // This server has no ECH keys, so it sends no retry_configs, and
    // neqo fails the handshake with ech_required. Firefox's Http3Session
    // reads that as NS_ERROR_NET_HTTP3_PROTOCOL_ERROR: HTTP/3 is excluded
    // from the host and taken from the request (DisableHttp3), so the
    // request doesn't go on to the second h3 record. Neither record has
    // a TCP entry (no-default-alpn), so the request fails.
    rules_http3_out(Duration::ZERO).await;
}

#[tokio::test]
async fn a_rejection_after_the_fast_fallback_timer_still_rules_http3_out() {
    // The same, with a server slow enough that the handshake fails after
    // Firefox's 50 ms fast-fallback timer, while the request waits for it
    // with no backup to race (ECH, and no record without h3). Its failed
    // connection decides what comes next, as Firefox's transaction is in
    // that session: the request isn't made again on a new one.
    rules_http3_out(Duration::from_millis(40)).await;
}

/// A request to a server that rejects ECH with nothing to retry with,
/// each of whose datagrams is answered `slow` after it came, makes one
/// QUIC connection.
async fn rules_http3_out(slow: Duration) {
    let (port, seen) = server(false, slow);
    let h3 = |priority| {
        (
            priority,
            vec![
                (1, b"\x02h3".to_vec()),
                (2, Vec::new()),
                (3, port.to_be_bytes().to_vec()),
                (5, hex(ECH_CONFIG_LIST)),
            ],
        )
    };
    let ((p1, first), (p2, second)) = (h3(1), h3(2));
    let record = netweir_core::https_answer(
        "origin.test",
        &[(p1, "a.test", &first), (p2, "b.test", &second)],
        300,
    );
    let mut options = FetchOptions::new(Profile::named("firefox-160-macos").unwrap());
    options.verify_certificates = false;
    options.resolve = ["origin.test", "a.test", "b.test"]
        .iter()
        .map(|name| (name.to_string(), ([127, 0, 0, 1], port).into()))
        .collect();
    options.https_records.insert("origin.test".into(), record);
    let fetcher = Fetcher::new(options).unwrap();
    let started = Instant::now();
    while seen.lock().unwrap().ports.is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the records never took"
        );
        let request = fetcher.get("https://origin.test/", &[]);
        let _ = tokio::time::timeout(Duration::from_secs(5), request).await;
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.ports.len(), 1, "the second h3 record was tried");
    assert!(seen.accepted.is_empty());
}
