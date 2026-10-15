//! The recovery ladder: retries, sessions, throttling, the circuit breaker
//! and giving up, against a scripted local site.

mod site;

use std::time::Duration;

use netweir_core::classify::BlockKind;
use netweir_core::{CrawlRequest, CrawlSettings, Crawler, Event, FetchOptions, Profile};
use site::{Page, Site};

fn settings() -> CrawlSettings {
    CrawlSettings {
        throttle: false,
        start_delay: Duration::ZERO,
        obey_robots: false,
        obey_tdmrep: false,
        backoff_base: Duration::from_millis(10),
        backoff_max: Duration::from_millis(50),
        ..CrawlSettings::default()
    }
}

fn crawler(settings: CrawlSettings) -> Crawler {
    Crawler::new(
        FetchOptions::new(Profile::named("chrome").unwrap()),
        settings,
    )
    .unwrap()
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
    }
}

async fn drain(c: &Crawler) -> Vec<Event> {
    let mut all = Vec::new();
    loop {
        let batch = tokio::time::timeout(Duration::from_secs(20), c.next(64))
            .await
            .expect("crawl stalled");
        if batch.is_empty() {
            return all;
        }
        all.extend(batch);
    }
}

fn status_of(events: &[Event], want: u64) -> Option<u16> {
    events.iter().find_map(|e| match e {
        Event::Fetched { id, response, .. } if *id == want => Some(response.status),
        _ => None,
    })
}

fn cloudflare_block() -> Page {
    Page::status(
        403,
        "<title>Attention Required! | Cloudflare</title><h1>Sorry, you have been blocked</h1>",
    )
    .with_header("server", "cloudflare")
}

#[tokio::test]
async fn server_errors_are_retried_until_they_pass() {
    let site = Site::scripted(vec![(
        "/flaky",
        vec![
            Page::status(503, "busy"),
            Page::status(502, "busy"),
            Page::html("ok"),
        ],
    )])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/flaky")));
    let events = drain(&c).await;
    assert_eq!(status_of(&events, 1), Some(200));
    assert_eq!(c.stats().retries, 2);
    assert_eq!(site.paths().len(), 3);
}

#[tokio::test]
async fn retries_run_out() {
    let site = Site::start(vec![("/down", Page::status(500, "down"))]).await;
    let c = crawler(CrawlSettings {
        retries: 2,
        ..settings()
    });
    c.submit(request(1, site.url("/down")));
    let events = drain(&c).await;
    assert_eq!(
        status_of(&events, 1),
        Some(500),
        "the last answer is the page"
    );
    assert_eq!(site.paths().len(), 3, "one try and two retries");
}

#[tokio::test]
async fn client_errors_and_payment_are_not_retried() {
    let site = Site::start(vec![
        ("/gone", Page::status(404, "gone")),
        (
            "/paid",
            Page::status(402, "pay").with_header("crawler-price", "USD 0.01"),
        ),
    ])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/gone")));
    c.submit(request(2, site.url("/paid")));
    let events = drain(&c).await;
    assert_eq!(
        (status_of(&events, 1), status_of(&events, 2)),
        (Some(404), Some(402))
    );
    assert_eq!(site.paths().len(), 2);
    assert_eq!(c.stats().retries, 0);
}

#[tokio::test]
async fn network_errors_are_retried_then_fail() {
    let c = crawler(CrawlSettings {
        retries: 2,
        ..settings()
    });
    c.submit(request(1, "http://127.0.0.1:9/nothing".into()));
    let events = drain(&c).await;
    assert!(matches!(&events[..], [Event::Failed { id: 1, .. }]));
    assert_eq!(c.stats().retries, 2);
}

#[tokio::test]
async fn retry_after_is_honoured() {
    let site = Site::scripted(vec![(
        "/later",
        vec![
            Page::status(429, "wait").with_header("retry-after", "1"),
            Page::html("ok"),
        ],
    )])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/later")));
    let events = drain(&c).await;
    assert_eq!(status_of(&events, 1), Some(200));
    let t = site.times("/later");
    assert!(
        t[1] - t[0] >= Duration::from_millis(950),
        "{:?}",
        t[1] - t[0]
    );
    assert_eq!(c.stats().throttled, 1);
}

#[tokio::test]
async fn a_block_gets_a_new_session_without_the_old_cookies() {
    let site = Site::scripted(vec![(
        "/guarded",
        vec![
            cloudflare_block().with_header("set-cookie", "tracker=1; Path=/"),
            Page::html("welcome"),
        ],
    )])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/guarded")));
    let events = drain(&c).await;
    assert_eq!(status_of(&events, 1), Some(200));
    let requests = site.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(
        !requests[1].1.iter().any(|(k, _)| k == "cookie"),
        "the second try still carried {:?}",
        requests[1].1
    );
    let stats = c.stats();
    assert_eq!((stats.blocked, stats.sessions_replaced), (1, 1));
}

#[tokio::test]
async fn a_block_that_persists_is_reported() {
    let site = Site::start(vec![("/never", cloudflare_block())]).await;
    let c = crawler(CrawlSettings {
        retries: 1,
        ..settings()
    });
    c.submit(request(1, site.url("/never")));
    let events = drain(&c).await;
    match &events[..] {
        [
            Event::Blocked {
                id: 1,
                vendor,
                kind,
                response,
            },
        ] => {
            assert_eq!((vendor.as_str(), *kind), ("cloudflare", BlockKind::Block));
            assert_eq!(response.status, 403);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(site.paths().len(), 2);
}

#[tokio::test]
async fn the_breaker_pauses_a_host_that_keeps_blocking() {
    let pages: Vec<(String, Page)> = (0..30)
        .map(|i| (format!("/p{i}"), cloudflare_block()))
        .collect();
    let site = Site::start(
        pages
            .iter()
            .map(|(p, page)| (p.as_str(), page.clone()))
            .collect(),
    )
    .await;
    let c = crawler(CrawlSettings {
        retries: 0,
        per_domain: 1,
        breaker_window: 10,
        breaker_ratio: 0.3,
        breaker_pause: Duration::from_millis(400),
        // Each block doubles the host's delay; keep that short here.
        max_delay: Duration::from_millis(20),
        ..settings()
    });
    for i in 0..30 {
        c.submit(request(i, site.url(&format!("/p{i}"))));
    }
    let events = drain(&c).await;
    let paused: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::Paused { .. }))
        .collect();
    assert!(!paused.is_empty(), "the breaker never tripped");
    let blocked = events
        .iter()
        .filter(|e| matches!(e, Event::Blocked { .. }))
        .count();
    assert_eq!(blocked, 30, "every request still ends");
    // While paused, nothing was sent: some gap is at least the pause.
    let t = site.times("/p");
    assert!(
        t.windows(2)
            .any(|w| w[1] - w[0] >= Duration::from_millis(380)),
        "no pause between requests"
    );
}

#[tokio::test]
async fn without_the_throttle_good_responses_undo_a_blocks_delay() {
    let mut pages = vec![("/a", vec![cloudflare_block(), Page::html("a")])];
    let names: Vec<String> = (0..6).map(|i| format!("/p{i}")).collect();
    for n in &names {
        pages.push((n.as_str(), vec![Page::html("p")]));
    }
    let site = Site::scripted(pages).await;
    let c = crawler(CrawlSettings {
        per_domain: 1,
        max_delay: Duration::from_millis(400),
        ..settings()
    });
    c.submit(request(0, site.url("/a")));
    for (i, n) in names.iter().enumerate() {
        c.submit(request(i as u64 + 1, site.url(n)));
    }
    drain(&c).await;
    let t = site.times("/");
    let first = t[1] - t[0];
    let last = t[t.len() - 1] - t[t.len() - 2];
    assert!(
        first >= Duration::from_millis(350),
        "a block slows the host: {first:?}"
    );
    assert!(
        last < Duration::from_millis(100),
        "and it speeds up again: {last:?}"
    );
}

#[tokio::test]
async fn a_429_doubles_the_delay_once_with_the_throttle_on() {
    let site = Site::scripted(vec![(
        "/a",
        vec![Page::status(429, "slow down"), Page::html("ok")],
    )])
    .await;
    let c = crawler(CrawlSettings {
        throttle: true,
        start_delay: Duration::from_secs(1),
        ..settings()
    });
    c.submit(request(1, site.url("/a")));
    drain(&c).await;
    let t = site.times("/a");
    let gap = t[1] - t[0];
    // Doubled once: 2 s, not 4 s. The gap between the two leaves room for
    // a slow machine; the half-second floor is checked in adjust_delay's
    // own test.
    assert!(
        gap >= Duration::from_millis(1980) && gap < Duration::from_secs(3),
        "{gap:?}"
    );
}

#[tokio::test]
async fn the_breaker_judges_after_a_few_responses_not_a_full_window() {
    let pages: Vec<(String, Page)> = (0..12)
        .map(|i| (format!("/p{i}"), cloudflare_block()))
        .collect();
    let site = Site::start(
        pages
            .iter()
            .map(|(p, page)| (p.as_str(), page.clone()))
            .collect(),
    )
    .await;
    let c = crawler(CrawlSettings {
        retries: 0,
        per_domain: 1,
        breaker_window: 50,
        breaker_pause: Duration::from_millis(50),
        max_delay: Duration::from_millis(5),
        ..settings()
    });
    for i in 0..12 {
        c.submit(request(i, site.url(&format!("/p{i}"))));
    }
    let events = drain(&c).await;
    assert!(events.iter().any(|e| matches!(e, Event::Paused { .. })));
    assert_eq!(
        c.stats().sessions_replaced,
        0,
        "no retries, so no new sessions were needed"
    );
}
