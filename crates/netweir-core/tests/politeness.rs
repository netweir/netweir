//! A site's crawl delay, as the site sees it: between the requests that
//! reach it, however long one took to connect.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use netweir_core::{CrawlRequest, CrawlSettings, Crawler, Event, FetchOptions, Profile};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// How the test site treats its connections after the first.
#[derive(Clone, Copy)]
enum Later {
    /// As the first, without its wait.
    Answered,
    /// Taken, and never said hello to: a TLS handshake that hangs.
    Hung,
}

/// An HTTPS site whose first connection takes `slow` to say hello back, as
/// a far-off or busy server's TLS handshake does, and which answers /a
/// after `hold_a`. Records when the first byte of each request's head
/// arrives, by path.
async fn site(
    slow: Duration,
    later: Later,
    hold_a: Duration,
) -> (String, Arc<Mutex<Vec<(String, Instant)>>>) {
    let made = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let cert = rustls::pki_types::CertificateDer::from(made.cert.der().to_vec());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(made.signing_key.serialize_der().into());
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "https://127.0.0.1:{}",
        listener.local_addr().unwrap().port()
    );
    let hits = Arc::new(Mutex::new(Vec::new()));
    let log = hits.clone();
    tokio::spawn(async move {
        let mut first = true;
        let mut hung = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            let (acceptor, log) = (acceptor.clone(), log.clone());
            let wait = if std::mem::take(&mut first) {
                slow
            } else if let Later::Hung = later {
                hung.push(stream);
                continue;
            } else {
                Duration::ZERO
            };
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                loop {
                    let mut head = Vec::new();
                    let mut arrived = None;
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                arrived.get_or_insert_with(Instant::now);
                                head.extend_from_slice(&buf[..n]);
                            }
                        }
                    }
                    let text = String::from_utf8_lossy(&head).to_string();
                    let path = text.split(' ').nth(1).unwrap_or("/").to_string();
                    if path == "/a" {
                        tokio::time::sleep(hold_a).await;
                    }
                    log.lock().unwrap().push((path, arrived.unwrap()));
                    let reply =
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 1\r\n\r\nx";
                    if tls.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, hits)
}

fn request(id: u64, url: String) -> CrawlRequest {
    CrawlRequest {
        id,
        url,
        priority: 0,
        headers: Vec::new(),
        dont_filter: false,
        depth: 0,
        browser: false,
        outgoing: netweir_core::Outgoing::navigate(),
        retry_post: false,
        proxy: None,
        delta_key: Default::default(),
        cookies: Default::default(),
        steps: None,
    }
}

/// A 300 ms delay, and a first connection 400 ms in the making: the second
/// request, handed out 300 ms after the first, would reach the site before
/// it on a quicker connection of its own. It waits for the first to leave,
/// and leaves 300 ms after it.
#[tokio::test(flavor = "multi_thread")]
async fn the_delay_runs_from_when_a_slow_connection_lets_the_request_go() {
    let (url, hits) = site(Duration::from_millis(400), Later::Answered, Duration::ZERO).await;
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.verify_certificates = false;
    let delay = Duration::from_millis(300);
    let c = Crawler::new(
        options,
        CrawlSettings {
            obey_robots: false,
            obey_tdmrep: false,
            throttle: false,
            min_delay: delay,
            ..CrawlSettings::default()
        },
    )
    .unwrap();
    for (id, path) in [(1, "/a"), (2, "/b"), (3, "/c")] {
        c.submit(request(id, format!("{url}{path}")));
    }
    let mut fetched = 0;
    while fetched < 3 {
        let batch = tokio::time::timeout(Duration::from_secs(20), c.next(64))
            .await
            .expect("crawl stalled");
        assert!(!batch.is_empty(), "the crawl ended early");
        fetched += batch
            .iter()
            .filter(|e| matches!(e, Event::Fetched { .. }))
            .count();
    }
    let mut hits = hits.lock().unwrap().clone();
    hits.sort_by_key(|(_, at)| *at);
    let order: Vec<&str> = hits.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(order, ["/a", "/b", "/c"], "{hits:?}");
    for pair in hits.windows(2) {
        let gap = pair[1].1 - pair[0].1;
        assert!(
            gap >= delay - Duration::from_millis(5),
            "{} reached the site {gap:?} after {}",
            pair[1].0,
            pair[0].0
        );
    }
}

/// Two requests at once to a site, the first held 800 ms: the second
/// starts a connection of its own, and while it's in the making the pool
/// hands it the first's, now free. That connection goes on in the
/// background, and its handshake never answers. Counted as being made
/// for good, it held the site's other requests back for good: the crawl
/// stalled after two. Found in review.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_left_hanging_in_the_background_holds_nothing_back() {
    let (url, _) = site(Duration::ZERO, Later::Hung, Duration::from_millis(800)).await;
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.verify_certificates = false;
    options.timeout = Duration::from_secs(5);
    let c = Crawler::new(
        options,
        CrawlSettings {
            obey_robots: false,
            obey_tdmrep: false,
            throttle: false,
            min_delay: Duration::from_millis(200),
            per_domain: 2,
            ..CrawlSettings::default()
        },
    )
    .unwrap();
    for (id, path) in [(1, "/a"), (2, "/b"), (3, "/c")] {
        c.submit(request(id, format!("{url}{path}")));
    }
    let began = Instant::now();
    let mut fetched = 0;
    while fetched < 3 {
        let batch = tokio::time::timeout(Duration::from_secs(15), c.next(64))
            .await
            .unwrap_or_else(|_| panic!("stalled after {fetched} fetched, {:?}", began.elapsed()));
        assert!(!batch.is_empty(), "the crawl ended early");
        fetched += batch
            .iter()
            .filter(|e| matches!(e, Event::Fetched { .. }))
            .count();
    }
}
