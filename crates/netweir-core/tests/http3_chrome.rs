//! A request over HTTP/3 with the Chrome profile must look, on the wire,
//! like Chrome 157's: profiles/chrome-157-macos.h3.capture.json recorded
//! Chrome moving to HTTP/3 on the fingerprint server's Alt-Svc, typing
//! /image into the address bar, which loads /picture.png and
//! /favicon.ico, on fresh and resumed connections. Here netweir does the
//! same against the same server and every request is compared with
//! Chrome's: exactly, field for field, for what Chrome sends the same on
//! every connection; against the range Chrome draws from for what it
//! draws at random (the transport parameters' order, GREASE, the CRYPTO
//! frame layout). Then how the fetcher moves a Chrome profile to HTTP/3
//! and off it, as Chrome does.

#![cfg(feature = "http3")]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use netweir_core::{FetchOptions, Fetcher, Outgoing, Profile};
use netweir_fingerprint::{Capture, Frame, Quic, Server, differences, is_h3_grease};

const PROFILE: &str = "chrome-157-macos";
const PRE_SHARED_KEY: u16 = 0x0029;
const INITIAL_RTT: u64 = 0x3127;
const VERSION_INFORMATION: u64 = 0x11;
const PRIORITY_UPDATE: u64 = 0xf0700;

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

/// A recorded request to `path` on a connection that resumed, or didn't.
fn recorded<'a>(recording: &'a [Capture], path: &str, resumed_: bool) -> &'a Capture {
    recording
        .iter()
        .find(|c| c.path == path && resumed(c) == resumed_)
        .unwrap_or_else(|| panic!("no recorded {path}, resumed {resumed_}"))
}

/// What Chrome draws per connection, made the same in both so that
/// `differences` compares the rest: where the GREASE version sits in
/// version_information, and the RTT initial_rtt carries (whether it's
/// there is still compared). Checked against their ranges in
/// [`chaos_in_range`] and [`shuffled_like_chrome`].
fn settle(q: &mut Quic) {
    for p in &mut q.transport_parameters {
        match p.id {
            INITIAL_RTT => p.value.clear(),
            VERSION_INFORMATION => {
                let words: Vec<String> = (0..p.value.len() / 8)
                    .map(|i| p.value[i * 8..i * 8 + 8].to_string())
                    .collect();
                let (grease, rest): (Vec<_>, Vec<_>) = words.into_iter().partition(|w| {
                    u32::from_str_radix(w, 16).unwrap() & 0x0f0f_0f0f == 0x0a0a_0a0a
                });
                p.value = rest.concat() + &grease.concat();
            }
            _ => {}
        }
    }
}

/// The layout of an Initial flight, which Chrome's chaos protection makes
/// anew each time: how many CRYPTO and PING frames, the first piece of
/// the ClientHello.
#[derive(Debug)]
struct Layout {
    crypto: usize,
    pings: usize,
    first_piece: u64,
}

/// `q`'s first flight against Chrome's rules: two Initial packets, each
/// filling its datagram (1250 bytes over IPv4, 1230 over IPv6) with
/// PADDING frames and no zero bytes after it, numbered 1 and 2 in one and
/// two bytes, no ACK and no token; the first carrying the ClientHello's
/// first 55 to 86 bytes and its end, the second the middle; 3 to 23
/// CRYPTO frames and 4 to 20 PINGs in all (QuicChaosProtector's ranges).
fn chaos_in_range(q: &Quic, what: &str) -> Layout {
    assert_eq!(q.version, 1, "{what}: QUIC v1");
    assert_eq!(q.dcid_len, 8, "{what}: an 8-byte Destination Connection ID");
    assert_eq!(q.scid_len, 0, "{what}: no Source Connection ID");
    assert!(q.token.is_empty(), "{what}: no token");
    assert_eq!(q.datagrams.len(), 2, "{what}: two datagrams");
    let size = q.datagrams[0].size;
    assert!(
        size == 1250 || size == 1230,
        "{what}: {size}-byte datagrams"
    );
    let (mut crypto, mut pings) = (Vec::new(), 0);
    for (i, d) in q.datagrams.iter().enumerate() {
        assert_eq!(d.size, size, "{what}: datagram {i}");
        assert_eq!(d.packets.len(), 1, "{what}: one packet in datagram {i}");
        let p = &d.packets[0];
        assert_eq!(
            (p.kind.as_str(), p.size),
            ("initial", size),
            "{what}: packet {i}"
        );
        assert_eq!(p.packet_number, Some(i as u64 + 1), "{what}: packet {i}");
        assert_eq!(p.packet_number_len, Some(i + 1), "{what}: packet {i}");
        for f in &p.frames {
            match f {
                Frame::Crypto { offset, length } => crypto.push((i, *offset, *length as u64)),
                Frame::Ping => pings += 1,
                Frame::Padding { .. } => {}
                other => panic!("{what}: a {other:?} frame"),
            }
        }
    }
    crypto.sort_by_key(|c| c.1);
    let mut at = 0;
    for &(_, offset, length) in &crypto {
        assert_eq!(
            offset, at,
            "{what}: the CRYPTO frames cover the ClientHello"
        );
        at += length;
    }
    let first_piece: u64 = crypto.iter().take_while(|c| c.0 == 0).map(|c| c.2).sum();
    assert!(
        (55..=86).contains(&first_piece),
        "{what}: first piece {first_piece}"
    );
    assert_eq!(
        crypto.last().unwrap().0,
        0,
        "{what}: the end in the first packet"
    );
    assert!(
        crypto.iter().any(|c| c.0 == 1),
        "{what}: the middle in the second"
    );
    assert!(
        (3..=23).contains(&crypto.len()),
        "{what}: {} CRYPTO frames",
        crypto.len()
    );
    assert!((4..=20).contains(&pings), "{what}: {pings} PINGs");
    Layout {
        crypto: crypto.len(),
        pings,
        first_piece,
    }
}

/// `actual` against `expected` field for field, but for the CRYPTO frame
/// layout, checked against its range instead.
fn check(expected: &Capture, actual: &Capture, what: &str) -> Layout {
    let (mut expected, mut actual) = (expected.clone(), actual.clone());
    // The server snapshots the control stream when a request comes in,
    // and reads the two on separate tasks, so the request's own
    // PRIORITY_UPDATE may not be in yet: they're checked once all are in
    // (see `requests_over_http3_are_chromes`).
    for c in [&mut expected, &mut actual] {
        if let Some(h3) = c.http3.as_mut() {
            h3.control_frames
                .retain(|(kind, _)| *kind != PRIORITY_UPDATE);
        }
    }
    settle(expected.quic.as_mut().unwrap());
    settle(actual.quic.as_mut().unwrap());
    let diff: Vec<String> = differences(&expected, &actual)
        .into_iter()
        .filter(|d| !d.starts_with("quic datagrams:"))
        .collect();
    assert!(
        diff.is_empty(),
        "{what} differs from Chrome's:\n{}",
        diff.join("\n")
    );
    let layout = chaos_in_range(actual.quic.as_ref().unwrap(), what);
    // Chrome's GREASE transport parameter: one, 31 * N + 27, 0 to 15 bytes.
    let grease: Vec<_> = actual
        .quic
        .as_ref()
        .unwrap()
        .transport_parameters
        .iter()
        .filter(|p| p.grease)
        .collect();
    assert_eq!(grease.len(), 1, "{what}: one GREASE transport parameter");
    assert_eq!(grease[0].id % 31, 27, "{what}");
    assert!(grease[0].value.len() / 2 < 16, "{what}");
    layout
}

/// The HTTP/3 connection of a capture, with its request's priority.
fn priority_updates(c: &Capture) -> Vec<String> {
    c.http3
        .as_ref()
        .unwrap()
        .control_frames
        .iter()
        .filter(|(kind, _)| !is_h3_grease(*kind))
        .map(|(kind, payload)| format!("{kind:#x} {payload}"))
        .collect()
}

#[tokio::test]
async fn requests_over_http3_are_chromes() {
    let recording = recording();
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();
    // Chrome's recording is over IPv4: 1250-byte datagrams.
    fetcher
        .use_http3(&origin, &format!("127.0.0.1:{port}"))
        .unwrap();

    let page = format!("{origin}/image");
    let response = fetcher.get(&page, &[]).await.unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let navigation = server.next().await.unwrap();
    let response = fetcher
        .send(
            &format!("{origin}/picture.png"),
            &Outgoing::image(Some(page.clone())),
            &[],
        )
        .await
        .unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let image = server.next().await.unwrap();
    let response = fetcher
        .send(
            &format!("{origin}/favicon.ico"),
            // Chrome's favicon request is an image's at u=1; netweir has
            // no favicon kind, and a script's fetch shares that priority.
            &Outgoing::fetch("GET", None, None, Some(page.clone())),
            &[],
        )
        .await
        .unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/3"));
    let favicon = server.next().await.unwrap();
    assert_eq!(
        [image.connection, favicon.connection],
        [navigation.connection; 2],
        "the requests go on one connection, as Chrome's did"
    );
    assert!(!resumed(&navigation));
    for (actual, path) in [(&navigation, "/image"), (&image, "/picture.png")] {
        check(recorded(&recording, path, false), actual, path);
    }
    // One more request, after the three PRIORITY_UPDATEs are surely in.
    fetcher.get(&format!("{origin}/"), &[]).await.unwrap();
    let last = server.next().await.unwrap();
    assert_eq!(last.connection, navigation.connection);
    assert_eq!(
        priority_updates(&last)[..3],
        [
            "0xf0700 00753d302c2069",
            "0xf0700 04753d322c2069",
            "0xf0700 08753d312c2069"
        ],
        "a PRIORITY_UPDATE repeating each request's priority"
    );
}

#[tokio::test]
async fn a_second_connection_resumes_like_chrome_and_sends_its_rtt() {
    let recording = recording();
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let fetcher = fetcher();
    fetcher
        .use_http3(&origin, &format!("127.0.0.1:{port}"))
        .unwrap();
    let page = format!("{origin}/image");
    fetcher.get(&page, &[]).await.unwrap();
    let fresh = server.next().await.unwrap();
    fetcher.close_http3().await;
    fetcher.get(&page, &[]).await.unwrap();
    let again = server.next().await.unwrap();
    assert_ne!(fresh.connection, again.connection);
    assert!(!resumed(&fresh));
    assert!(resumed(&again), "the second connection resumed");
    check(
        recorded(&recording, "/image", false),
        &fresh,
        "a fresh connection",
    );
    check(
        recorded(&recording, "/image", true),
        &again,
        "a resumed one",
    );
    // initial_rtt carries the first connection's smoothed RTT, in µs.
    let rtt = again
        .quic
        .as_ref()
        .unwrap()
        .transport_parameters
        .iter()
        .find(|p| p.id == INITIAL_RTT)
        .expect("initial_rtt on the resumed connection");
    assert!(!rtt.value.is_empty());
}

/// Chrome shuffles its transport parameters and its ClientHello's
/// extensions on every connection, picks a new GREASE parameter, a new
/// GREASE version place, new HTTP/3 GREASE, and a new CRYPTO layout.
#[tokio::test]
async fn many_connections_vary_as_chromes_do() {
    let recording = recording();
    let mut server = Server::start_h3(0, None).await.unwrap();
    let port = server.port;
    let origin = format!("https://localhost:{port}");
    let (mut orders, mut extensions, mut greases, mut versions) = (
        HashSet::new(),
        HashSet::new(),
        HashSet::new(),
        HashSet::new(),
    );
    let (mut settings, mut frames, mut layouts) = (HashSet::new(), HashSet::new(), Vec::new());
    const N: usize = 16;
    for i in 0..N {
        let fetcher = fetcher();
        fetcher
            .use_http3(&origin, &format!("127.0.0.1:{port}"))
            .unwrap();
        fetcher.get(&format!("{origin}/image"), &[]).await.unwrap();
        let c = server.next().await.unwrap();
        layouts.push(check(
            recorded(&recording, "/image", false),
            &c,
            &format!("connection {i}"),
        ));
        let q = c.quic.as_ref().unwrap();
        orders.insert(
            q.transport_parameters
                .iter()
                .map(|p| if p.grease { 0 } else { p.id })
                .collect::<Vec<_>>(),
        );
        extensions.insert(c.client_hello.extensions.clone());
        greases.extend(
            q.transport_parameters
                .iter()
                .filter(|p| p.grease)
                .map(|p| p.id),
        );
        versions.extend(
            q.transport_parameters
                .iter()
                .filter(|p| p.id == VERSION_INFORMATION)
                .map(|p| p.value[8..16].to_string() == "00000001"),
        );
        let h3 = c.http3.as_ref().unwrap();
        settings.extend(h3.settings.iter().filter(|s| s.grease).map(|s| s.id));
        frames.extend(
            h3.control_frames
                .iter()
                .filter(|(k, _)| is_h3_grease(*k))
                .map(|(k, _)| *k),
        );
    }
    assert!(
        orders.len() > N / 2,
        "transport parameter orders: {}",
        orders.len()
    );
    assert!(
        extensions.len() > N / 2,
        "extension orders: {}",
        extensions.len()
    );
    assert_eq!(
        greases.len(),
        N,
        "a new GREASE transport parameter each time"
    );
    assert_eq!(versions.len(), 2, "the GREASE version on either side of v1");
    assert_eq!(settings.len(), N, "a new GREASE setting each time");
    assert_eq!(frames.len(), N, "a new GREASE frame each time");
    let distinct = |f: fn(&Layout) -> u64| layouts.iter().map(f).collect::<HashSet<_>>().len();
    assert!(distinct(|l| l.crypto as u64) > 2, "{layouts:?}");
    assert!(distinct(|l| l.pings as u64) > 2, "{layouts:?}");
    assert!(distinct(|l| l.first_piece) > 2, "{layouts:?}");
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
/// [`slow_relay`] holds each datagram back a while, each way.
struct Relay {
    port: u16,
    // Every test here keeps the relay dropping from the start.
    #[allow(dead_code)]
    drop: Arc<AtomicBool>,
    seen: Arc<AtomicUsize>,
}

async fn relay(to: Option<u16>) -> Relay {
    slow_relay(to, Duration::ZERO).await
}

async fn slow_relay(to: Option<u16>, delay: Duration) -> Relay {
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
                    let (upstream, datagram) = (upstream.clone(), buf[..n].to_vec());
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = upstream.send(&datagram).await;
                    });
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
                    let (socket, datagram) = (socket.clone(), buf[..n].to_vec());
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = socket.send_to(&datagram, to).await;
                    });
                }
            }
        });
    }
    Relay { port, drop, seen }
}

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

#[tokio::test]
async fn a_site_that_offers_http3_moves_chrome_to_it() {
    let mut server = Server::start_h3(0, None).await.unwrap();
    let origin = format!("https://localhost:{}", server.port);
    let fetcher = fetcher();
    let first = fetcher.get(&format!("{origin}/"), &[]).await.unwrap();
    assert_eq!(first.version, "HTTP/2");
    until_http3(&fetcher, &format!("{origin}/")).await;
    fetcher.get(&format!("{origin}/again"), &[]).await.unwrap();
    let last = loop {
        let capture = server.next().await.unwrap();
        if capture.path == "/again" {
            break capture;
        }
    };
    assert_eq!(last.protocol, "h3");
    assert!(
        !last.headers.iter().any(|(k, _)| k == "alt-used"),
        "Chrome sends no Alt-Used"
    );
}

#[tokio::test]
async fn alt_svc_is_kept_dropped_and_cleared_as_chrome_does() {
    let h3 = Server::start_h3(0, None).await.unwrap();
    let tcp = Server::start().await.unwrap();
    let origin = format!("https://localhost:{}", tcp.port);
    let to = format!("localhost:{}", h3.port);
    let plain = format!("{origin}/");
    let fetcher = fetcher();
    fetcher
        .get(&offering(&origin, &format!("h3=\"{to}\"")), &[])
        .await
        .unwrap();
    until_http3(&fetcher, &plain).await;
    // A header Chrome can't parse whole, ma=0 among them, changes nothing.
    for bad in ["h3=\":1\"; ma=0".to_string(), format!("h3=\"{to}\"; ma=x")] {
        fetcher.get(&offering(&origin, &bad), &[]).await.unwrap();
        assert_eq!(
            fetcher.get(&plain, &[]).await.unwrap().version,
            "HTTP/3",
            "{bad}"
        );
    }
    // Only h3-29: a list left empty, which forgets the alternative.
    fetcher
        .get(&offering(&origin, &format!("h3-29=\"{to}\"")), &[])
        .await
        .unwrap();
    assert_eq!(fetcher.get(&plain, &[]).await.unwrap().version, "HTTP/2");
    fetcher
        .get(&offering(&origin, &format!("h3=\"{to}\"")), &[])
        .await
        .unwrap();
    until_http3(&fetcher, &plain).await;
    let cleared = fetcher.get(&offering(&origin, "clear"), &[]).await.unwrap();
    assert_eq!(cleared.version, "HTTP/3");
    assert_eq!(fetcher.get(&plain, &[]).await.unwrap().version, "HTTP/2");
}

/// A network that drops UDP: the request goes over TCP at once, as
/// Chrome's TCP job doesn't wait for QUIC that has never worked; the QUIC
/// job gives up after Chrome's 10 s handshake limit and the alternative is
/// broken for 10 s, then forgotten until the site offers it again.
#[tokio::test]
async fn a_network_that_drops_udp_falls_back_and_backs_off_as_chrome_does() {
    let tcp = Server::start().await.unwrap();
    let origin = format!("https://localhost:{}", tcp.port);
    let plain = format!("{origin}/");
    let udp = relay(None).await;
    let offer = offering(&origin, &format!("h3=\"127.0.0.1:{}\"", udp.port));
    let fetcher = fetcher();
    assert_eq!(fetcher.get(&offer, &[]).await.unwrap().version, "HTTP/2");
    let started = Instant::now();
    let response = fetcher.get(&plain, &[]).await.unwrap();
    assert_eq!((response.status, response.version), (200, "HTTP/2"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "TCP didn't wait"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(udp.seen.load(Ordering::Relaxed) > 0, "QUIC was tried");

    // Once the QUIC job has failed: broken, so no UDP at all.
    tokio::time::sleep_until((started + Duration::from_secs(12)).into()).await;
    let tried = udp.seen.load(Ordering::Relaxed);
    assert_eq!(fetcher.get(&plain, &[]).await.unwrap().version, "HTTP/2");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(udp.seen.load(Ordering::Relaxed), tried, "QUIC while broken");

    // When the mark expires the alternative goes with it.
    tokio::time::sleep_until((started + Duration::from_secs(23)).into()).await;
    assert_eq!(fetcher.get(&plain, &[]).await.unwrap().version, "HTTP/2");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(udp.seen.load(Ordering::Relaxed), tried, "QUIC after expiry");

    // Offered again, it's tried again.
    fetcher.get(&offer, &[]).await.unwrap();
    fetcher.get(&plain, &[]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        udp.seen.load(Ordering::Relaxed) > tried,
        "QUIC offered again"
    );
}

#[tokio::test]
async fn a_proxied_chrome_fetch_never_sends_a_udp_packet() {
    let tcp = Server::start().await.unwrap();
    let origin = format!("https://localhost:{}", tcp.port);
    let udp = relay(None).await;
    let offer = offering(&origin, &format!("h3=\"127.0.0.1:{}\"", udp.port));
    let mut options = FetchOptions::new(Profile::named(PROFILE).unwrap());
    options.verify_certificates = false;
    options.proxy = Some(tunnel().await);
    let proxied = Fetcher::new(options).unwrap();
    for _ in 0..3 {
        assert_eq!(proxied.get(&offer, &[]).await.unwrap().version, "HTTP/2");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(udp.seen.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn the_archive_records_chromes_http3() {
    let dir = std::env::temp_dir().join(format!("netweir-h3c-archive-{}", std::process::id()));
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
        .use_http3(&origin, &format!("127.0.0.1:{}", server.port))
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
