//! Proxy pools: rotation, per-host proxies, benching and per-request
//! proxies. Each proxy is a scripted local site: an HTTP proxy is sent the
//! whole URL (`GET http://shop.test/a`), so the site answers for any host
//! and logs which requests it carried.

mod site;

use std::collections::HashSet;
use std::time::Duration;

use netweir_core::{
    CrawlRequest, CrawlSettings, Crawler, Event, FetchOptions, Profile, ProxyRotation,
};
use site::{Page, Site};

fn settings(rotation: ProxyRotation, proxies: &[&Site]) -> CrawlSettings {
    CrawlSettings {
        throttle: false,
        start_delay: Duration::ZERO,
        obey_robots: false,
        obey_tdmrep: false,
        backoff_base: Duration::from_millis(5),
        backoff_max: Duration::from_millis(20),
        // One request at a time, so the rotation's order is the requests'.
        concurrency: 1,
        per_domain: 1,
        proxies: proxies.iter().map(|p| proxy(p)).collect(),
        proxy_rotation: rotation,
        ..CrawlSettings::default()
    }
}

fn proxy(site: &Site) -> String {
    format!("http://127.0.0.1:{}", site.port)
}

/// A proxy nothing answers at: every connection is refused.
async fn dead() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

fn crawler(settings: CrawlSettings) -> Crawler {
    Crawler::new(
        FetchOptions::new(Profile::named("chrome").unwrap()),
        settings,
    )
    .unwrap()
}

fn request(id: u64, url: &str) -> CrawlRequest {
    CrawlRequest {
        id,
        url: url.to_string(),
        priority: 0,
        headers: Vec::new(),
        dont_filter: true,
        depth: 0,
        browser: false,
        outgoing: netweir_core::Outgoing::navigate(),
        retry_post: false,
        proxy: None,
        delta_key: Default::default(),
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

fn statuses(events: &[Event]) -> Vec<(u64, u16)> {
    let mut out: Vec<(u64, u16)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Fetched { id, response, .. } => Some((*id, response.status)),
            _ => None,
        })
        .collect();
    out.sort();
    out
}

fn warnings(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn hosts(site: &Site) -> HashSet<String> {
    site.paths()
        .iter()
        .filter_map(|p| url::Url::parse(p).ok()?.host_str().map(String::from))
        .collect()
}

fn stats_of(c: &Crawler, proxy: &str) -> netweir_core::ProxyStats {
    c.proxy_stats()
        .into_iter()
        .find(|(url, _)| url == proxy)
        .map(|(_, s)| s)
        .unwrap_or_default()
}

fn cloudflare_block() -> Page {
    Page::status(
        403,
        "<title>Attention Required! | Cloudflare</title><h1>Sorry, you have been blocked</h1>",
    )
    .with_header("server", "cloudflare")
}

#[tokio::test]
async fn per_request_sends_each_request_through_the_next_proxy() {
    let ok = || Page::html("ok");
    let pages: Vec<(String, Page)> = (0..6)
        .map(|i| (format!("http://shop.test/{i}"), ok()))
        .collect();
    let pages: Vec<(&str, Page)> = pages
        .iter()
        .map(|(p, page)| (p.as_str(), page.clone()))
        .collect();
    let (a, b, c) = (
        Site::start(pages.clone()).await,
        Site::start(pages.clone()).await,
        Site::start(pages).await,
    );
    let crawl = crawler(settings(ProxyRotation::PerRequest, &[&a, &b, &c]));
    for i in 0..6 {
        crawl.submit(request(i, &format!("http://shop.test/{i}")));
    }
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events).len(), 6);
    assert_eq!(a.paths(), ["http://shop.test/0", "http://shop.test/3"]);
    assert_eq!(b.paths(), ["http://shop.test/1", "http://shop.test/4"]);
    assert_eq!(c.paths(), ["http://shop.test/2", "http://shop.test/5"]);
    let stats = crawl.proxy_stats();
    assert_eq!(stats.len(), 3);
    assert!(
        stats
            .iter()
            .all(|(_, s)| s.requests == 2 && s.failures == 0)
    );
}

#[tokio::test]
async fn per_request_shares_the_crawls_cookies_and_a_block_drops_the_sites() {
    let login = Page::html("in").with_header("set-cookie", "sid=1; Path=/");
    let a = Site::start(vec![
        ("http://shop.test/login", login.clone()),
        ("http://shop.test/x", Page::html("x")),
    ])
    .await;
    let b = Site::start(vec![("http://shop.test/x", cloudflare_block())]).await;
    let crawl = crawler(settings(ProxyRotation::PerRequest, &[&a, &b]));
    crawl.submit(request(1, "http://shop.test/login"));
    drain(&crawl).await;
    crawl.submit(request(2, "http://shop.test/x"));
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events), [(2, 200)]);
    let cookie = |site: &Site| {
        let requests = site.requests.lock().unwrap();
        requests
            .iter()
            .filter(|(p, _)| p == "http://shop.test/x")
            .map(|(_, h)| {
                h.iter()
                    .find(|(k, _)| k == "cookie")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(cookie(&b), ["sid=1"], "one jar, whichever proxy");
    assert_eq!(cookie(&a), [""], "the retry, after the block, had none");
    assert_eq!(stats_of(&crawl, &proxy(&b)).failures, 1, "a block counts");
}

#[tokio::test]
async fn per_host_keeps_each_site_on_its_own_proxy() {
    let mut pages = Vec::new();
    for host in ["a.test", "b.test"] {
        for i in 0..3 {
            pages.push((format!("http://{host}/{i}"), Page::html("ok")));
        }
    }
    let pages: Vec<(&str, Page)> = pages
        .iter()
        .map(|(p, page)| (p.as_str(), page.clone()))
        .collect();
    let (one, two) = (Site::start(pages.clone()).await, Site::start(pages).await);
    let crawl = crawler(CrawlSettings {
        concurrency: 4,
        ..settings(ProxyRotation::PerHost, &[&one, &two])
    });
    let mut id = 0;
    for i in 0..3 {
        for host in ["a.test", "b.test"] {
            id += 1;
            crawl.submit(request(id, &format!("http://{host}/{i}")));
        }
    }
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events).len(), 6);
    let (h1, h2) = (hosts(&one), hosts(&two));
    assert_eq!((h1.len(), h2.len()), (1, 1), "{h1:?} {h2:?}");
    assert_ne!(h1, h2);
    assert_eq!((one.paths().len(), two.paths().len()), (3, 3));
    assert_eq!(
        crawl.stats().sessions_replaced,
        0,
        "a first proxy replaces nothing"
    );
}

#[tokio::test]
async fn per_host_moves_a_blocked_site_to_the_next_proxy() {
    let one = Site::start(vec![("http://a.test/x", cloudflare_block())]).await;
    let two = Site::start(vec![("http://a.test/x", Page::html("x"))]).await;
    let crawl = crawler(settings(ProxyRotation::PerHost, &[&one, &two]));
    crawl.submit(request(1, "http://a.test/x"));
    crawl.submit(request(2, "http://a.test/x"));
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events), [(1, 200), (2, 200)]);
    assert_eq!(one.paths().len(), 1);
    assert_eq!(
        two.paths().len(),
        2,
        "the retry and the next request stay there"
    );
    let stats = crawl.stats();
    assert_eq!((stats.blocked, stats.sessions_replaced), (1, 1));
}

#[tokio::test]
async fn a_proxy_that_keeps_failing_is_benched_and_the_rest_carry_the_crawl() {
    let dead = dead().await;
    let pages: Vec<(String, Page)> = (0..6)
        .map(|i| (format!("http://shop.test/{i}"), Page::html("ok")))
        .collect();
    let good = Site::start(
        pages
            .iter()
            .map(|(p, page)| (p.as_str(), page.clone()))
            .collect(),
    )
    .await;
    let mut s = settings(ProxyRotation::PerRequest, &[&good]);
    s.proxies.insert(0, dead.clone());
    s.proxy_failures = 2;
    let crawl = crawler(s);
    for i in 0..6 {
        crawl.submit(request(i, &format!("http://shop.test/{i}")));
    }
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events).len(), 6, "every request got through");
    let d = stats_of(&crawl, &dead);
    assert_eq!((d.requests, d.failures, d.benched), (2, 2, 1));
    assert_eq!(stats_of(&crawl, &proxy(&good)).requests, 6);
    assert_eq!(crawl.stats().proxies_benched, 1);
    let benched: Vec<String> = warnings(&events)
        .into_iter()
        .filter(|w| w.contains("benched for"))
        .collect();
    assert_eq!(benched.len(), 1, "{benched:?}");
    assert!(
        benched[0].contains(&dead) && benched[0].contains("60s"),
        "{}",
        benched[0]
    );
}

#[tokio::test]
async fn a_benched_proxy_comes_back_and_is_benched_twice_as_long_next_time() {
    let dead = dead().await;
    let good = Site::start(vec![
        ("http://shop.test/1", Page::html("ok")),
        ("http://shop.test/2", Page::html("ok")),
    ])
    .await;
    let mut s = settings(ProxyRotation::PerRequest, &[&good]);
    s.proxies.insert(0, dead.clone());
    s.proxy_failures = 1;
    s.proxy_cooldown = Duration::from_millis(200);
    let crawl = crawler(s);
    crawl.submit(request(1, "http://shop.test/1"));
    let first = drain(&crawl).await;
    assert_eq!(stats_of(&crawl, &dead).benched, 1);
    // Back after its 200ms: the rotation reaches it again, it fails again,
    // and goes back for twice as long.
    tokio::time::sleep(Duration::from_millis(300)).await;
    crawl.submit(request(2, "http://shop.test/2"));
    let second = drain(&crawl).await;
    assert_eq!(statuses(&second), [(2, 200)]);
    let d = stats_of(&crawl, &dead);
    assert_eq!((d.requests, d.benched), (2, 2));
    assert!(
        warnings(&first)
            .iter()
            .any(|w| w.contains("benched for 200ms"))
    );
    assert!(
        warnings(&second)
            .iter()
            .any(|w| w.contains("benched for 400ms"))
    );
}

#[tokio::test]
async fn with_every_proxy_benched_the_soonest_back_carries_on_and_a_success_clears_it() {
    let only = Site::scripted(vec![
        (
            "http://shop.test/p",
            vec![
                Page::status(407, "login"),
                Page::status(407, "login"),
                Page::html("p"),
            ],
        ),
        (
            "http://shop.test/q",
            vec![Page::status(407, "login"), Page::html("q")],
        ),
    ])
    .await;
    let mut s = settings(ProxyRotation::PerRequest, &[&only]);
    s.proxy_failures = 2;
    s.retries = 5;
    let crawl = crawler(s);
    crawl.submit(request(1, "http://shop.test/p"));
    let first = drain(&crawl).await;
    assert_eq!(statuses(&first), [(1, 200)], "a 407 is tried again");
    crawl.submit(request(2, "http://shop.test/q"));
    let second = drain(&crawl).await;
    assert_eq!(statuses(&second), [(2, 200)]);
    let o = stats_of(&crawl, &proxy(&only));
    // Benched after the second 407; the success cleared it, so the next
    // 407 is only the first of a new run.
    assert_eq!((o.requests, o.failures, o.benched), (5, 3, 1));
    let exhausted: Vec<String> = warnings(&first)
        .into_iter()
        .chain(warnings(&second))
        .filter(|w| w.contains("every proxy is benched"))
        .collect();
    assert_eq!(exhausted.len(), 1, "{exhausted:?}");
}

#[tokio::test]
async fn a_request_names_its_own_proxy_whatever_the_rotation() {
    let pool = Site::start(vec![("http://shop.test/a", Page::html("a"))]).await;
    let own = Site::start(vec![("http://shop.test/b", Page::html("b"))]).await;
    let crawl = crawler(settings(ProxyRotation::PerRequest, &[&pool]));
    crawl.submit(request(1, "http://shop.test/a"));
    crawl.submit(CrawlRequest {
        proxy: Some(proxy(&own)),
        ..request(2, "http://shop.test/b")
    });
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events), [(1, 200), (2, 200)]);
    assert_eq!(own.paths(), ["http://shop.test/b"]);
    assert_eq!(pool.paths(), ["http://shop.test/a"]);
    let names: Vec<String> = crawl.proxy_stats().into_iter().map(|(u, _)| u).collect();
    assert_eq!(names, [proxy(&pool), proxy(&own)], "the pool first");
    assert_eq!(stats_of(&crawl, &proxy(&own)).requests, 1);
}

#[tokio::test]
async fn a_request_with_a_proxy_the_client_cant_use_is_refused() {
    let crawl = crawler(settings(ProxyRotation::OnBlock, &[]));
    let outcome = crawl.submit(CrawlRequest {
        proxy: Some("ftp://127.0.0.1:21".into()),
        ..request(1, "http://shop.test/a")
    });
    assert_eq!(outcome, netweir_core::Submitted::BadProxy);
}

#[tokio::test]
async fn robots_txt_a_dead_proxy_couldnt_fetch_is_fetched_through_another() {
    let dead = dead().await;
    let good = Site::start(vec![("http://shop.test/a", Page::html("a"))]).await;
    let mut s = settings(ProxyRotation::PerRequest, &[&good]);
    s.proxies.insert(0, dead.clone());
    s.obey_robots = true;
    let crawl = crawler(s);
    crawl.submit(request(1, "http://shop.test/a"));
    let events = drain(&crawl).await;
    assert_eq!(
        statuses(&events),
        [(1, 200)],
        "not shut out by the dead proxy"
    );
    assert_eq!(
        good.paths(),
        ["http://shop.test/robots.txt", "http://shop.test/a"]
    );
    assert_eq!(
        stats_of(&crawl, &dead).failures,
        1,
        "robots.txt only: the page went to the next in turn"
    );
}

#[tokio::test]
async fn rotation_needs_proxies_and_wont_take_proxy_as_well() {
    let options = || FetchOptions::new(Profile::named("chrome").unwrap());
    for rotation in [ProxyRotation::PerRequest, ProxyRotation::PerHost] {
        let none = CrawlSettings {
            proxy_rotation: rotation,
            ..CrawlSettings::default()
        };
        let err = Crawler::new(options(), none).err().unwrap();
        assert!(err.message.contains("none were given"), "{}", err.message);
        let both = CrawlSettings {
            proxy_rotation: rotation,
            proxies: vec!["http://127.0.0.1:9".into()],
            ..CrawlSettings::default()
        };
        let mut with_proxy = options();
        with_proxy.proxy = Some("http://127.0.0.1:8".into());
        let err = Crawler::new(with_proxy, both).err().unwrap();
        assert!(err.message.contains("which would win"), "{}", err.message);
    }
    let bad = CrawlSettings {
        proxies: vec!["ftp://ann:secret@127.0.0.1:9".into()],
        ..CrawlSettings::default()
    };
    let err = Crawler::new(options(), bad).err().unwrap();
    assert!(!err.message.contains("secret"), "{}", err.message);
}

#[tokio::test]
async fn on_block_moves_a_session_off_a_proxy_once_its_benched() {
    // The crawl's own proxy, until the block.
    let front = Site::start(vec![("http://a.test/x", cloudflare_block())]).await;
    let dead = dead().await;
    let good = Site::start(vec![("http://a.test/x", Page::html("x"))]).await;
    let mut s = settings(ProxyRotation::OnBlock, &[&good]);
    s.proxies.insert(0, dead.clone());
    s.proxy_failures = 1;
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.proxy = Some(proxy(&front));
    let crawl = Crawler::new(options, s).unwrap();
    crawl.submit(request(1, "http://a.test/x"));
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events), [(1, 200)]);
    assert_eq!(front.paths().len(), 1);
    assert_eq!(stats_of(&crawl, &dead).benched, 1);
    assert_eq!(good.paths().len(), 1);
    assert_eq!(
        crawl.stats().sessions_replaced,
        2,
        "the block, then the bench"
    );
}

#[tokio::test]
async fn per_host_with_every_proxy_benched_moves_to_the_one_back_soonest() {
    // A fails, so the host moves to B; B fails too, and now both are
    // benched. A was benched first, so it's back first: the host goes back
    // to it rather than sitting on B.
    let a = Site::scripted(vec![(
        "http://shop.test/p",
        vec![Page::status(407, "login"), Page::html("from a")],
    )])
    .await;
    let b = Site::scripted(vec![(
        "http://shop.test/p",
        vec![Page::status(407, "login"), Page::status(407, "login")],
    )])
    .await;
    let mut s = settings(ProxyRotation::PerHost, &[&a, &b]);
    s.proxy_failures = 1;
    s.retries = 3;
    let crawl = crawler(s);
    crawl.submit(request(1, "http://shop.test/p"));
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events), [(1, 200)]);
    assert_eq!(a.paths().len(), 2, "back to A, the first back");
    assert_eq!(b.paths().len(), 1);
}

fn dropped(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Dropped { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn per_host_robots_txt_a_dead_proxy_couldnt_fetch_moves_the_site_even_without_retries() {
    let dead = dead().await;
    let good = Site::start(vec![("http://shop.test/a", Page::html("a"))]).await;
    let mut s = settings(ProxyRotation::PerHost, &[&good]);
    s.proxies.insert(0, dead.clone());
    s.obey_robots = true;
    s.retries = 0;
    let crawl = crawler(s);
    crawl.submit(request(1, "http://shop.test/a"));
    let events = drain(&crawl).await;
    assert_eq!(
        statuses(&events),
        [(1, 200)],
        "not shut out by the dead proxy"
    );
    assert_eq!(
        good.paths(),
        ["http://shop.test/robots.txt", "http://shop.test/a"],
        "the site moved to the proxy that answered"
    );
}

#[tokio::test]
async fn robots_txt_tries_each_other_proxy_until_one_answers() {
    let (dead1, dead2) = (dead().await, dead().await);
    let good = Site::start(vec![("http://shop.test/a", Page::html("a"))]).await;
    let mut s = settings(ProxyRotation::PerRequest, &[&good]);
    s.proxies.insert(0, dead2.clone());
    s.proxies.insert(0, dead1.clone());
    s.obey_robots = true;
    s.retries = 0;
    s.proxy_failures = 1;
    let crawl = crawler(s);
    crawl.submit(request(1, "http://shop.test/a"));
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events), [(1, 200)]);
    assert_eq!(good.paths()[0], "http://shop.test/robots.txt");
    assert_eq!(stats_of(&crawl, &dead1).failures, 1);
    assert_eq!(stats_of(&crawl, &dead2).failures, 1);
}

#[tokio::test]
async fn a_proxy_that_wants_a_login_for_robots_txt_is_no_answer_from_the_site() {
    // Behind a 407 the site's robots.txt is unknown: off limits for now, as
    // when it can't be reached, not "no rules" as a 4xx from the site is.
    let login = Site::start(vec![
        ("http://shop.test/robots.txt", Page::status(407, "login")),
        ("http://shop.test/a", Page::html("a")),
    ])
    .await;
    let mut s = settings(ProxyRotation::PerRequest, &[&login]);
    s.obey_robots = true;
    let crawl = crawler(s);
    crawl.submit(request(1, "http://shop.test/a"));
    assert_eq!(dropped(&drain(&crawl).await), [1]);
    // The same through the crawl's own proxy, which isn't in a pool.
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.proxy = Some(proxy(&login));
    let mut s = settings(ProxyRotation::OnBlock, &[]);
    s.obey_robots = true;
    let crawl = Crawler::new(options, s).unwrap();
    crawl.submit(request(2, "http://shop.test/a"));
    assert_eq!(dropped(&drain(&crawl).await), [2]);
}

#[tokio::test]
async fn a_proxy_that_wants_a_login_for_tdmrep_json_is_no_answer_either() {
    // As when tdmrep.json can't be reached: no file, so no reservations.
    let reserving = r#"[{"location": "/*", "tdm-reservation": 1}]"#;
    let login = Site::start(vec![
        (
            "http://shop.test/.well-known/tdmrep.json",
            Page::status(407, reserving).with_header("content-type", "application/json"),
        ),
        ("http://shop.test/a", Page::html("a")),
    ])
    .await;
    let mut s = settings(ProxyRotation::PerRequest, &[&login]);
    s.obey_tdmrep = true;
    let crawl = crawler(s);
    crawl.submit(request(1, "http://shop.test/a"));
    assert_eq!(statuses(&drain(&crawl).await), [(1, 200)]);
}

#[tokio::test]
async fn an_empty_proxy_on_a_request_sends_it_direct_whatever_the_rotation() {
    let site = Site::start(vec![("/a", Page::html("a"))]).await;
    let pool = Site::start(vec![]).await;
    let crawl = crawler(settings(ProxyRotation::PerRequest, &[&pool]));
    crawl.submit(CrawlRequest {
        proxy: Some(String::new()),
        ..request(1, &site.url("/a"))
    });
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events), [(1, 200)]);
    assert!(pool.paths().is_empty());
    assert_eq!(site.paths(), ["/a"]);
    assert!(
        crawl
            .proxy_stats()
            .iter()
            .all(|(u, s)| !u.is_empty() && s.requests == 0)
    );
}

/// A proxy for https:// URLs, answering every CONNECT with `answer`, then,
/// if `then` is given, passing that back as the "site" through the tunnel.
async fn tunnel(answer: &'static str, then: Option<&'static str>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(answer.as_bytes()).await;
                if let Some(then) = then {
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(then.as_bytes()).await;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Three https sites that can't be had, through `proxy`, robots.txt on,
/// no retries: what each proxy was charged.
async fn three_dead_https_sites(proxy: &str) -> (netweir_core::ProxyStats, u64) {
    let mut s = settings(ProxyRotation::PerRequest, &[]);
    s.proxies = vec![proxy.to_string()];
    s.obey_robots = true;
    s.retries = 0;
    s.proxy_failures = 1;
    let crawl = crawler(s);
    for (i, host) in ["a.test", "b.test", "c.test"].iter().enumerate() {
        crawl.submit(request(i as u64, &format!("https://{host}/")));
    }
    drain(&crawl).await;
    (stats_of(&crawl, proxy), crawl.stats().proxies_benched)
}

#[tokio::test]
async fn a_proxy_that_cant_reach_a_site_isnt_charged_for_it() {
    // CONNECT answered 502: the proxy is up, the site isn't.
    let gateway = tunnel(
        "HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n",
        None,
    )
    .await;
    let (stats, benched) = three_dead_https_sites(&gateway).await;
    assert!(stats.requests > 0);
    assert_eq!((stats.failures, stats.benched, benched), (0, 0, 0));
}

#[tokio::test]
async fn a_sites_own_tls_failure_isnt_the_proxys() {
    // The tunnel opens, and what's at the other end doesn't speak TLS.
    let open = tunnel(
        "HTTP/1.1 200 Connection established\r\n\r\n",
        Some("HTTP/1.1 400 Not TLS\r\n\r\n"),
    )
    .await;
    let (stats, benched) = three_dead_https_sites(&open).await;
    assert!(stats.requests > 0);
    assert_eq!((stats.failures, stats.benched, benched), (0, 0, 0));
}

#[tokio::test]
async fn a_tunnel_that_wants_a_login_or_a_proxy_that_isnt_there_is_charged() {
    let login = tunnel("HTTP/1.1 407 Login\r\ncontent-length: 0\r\n\r\n", None).await;
    let (stats, _) = three_dead_https_sites(&login).await;
    assert!(stats.failures > 0 && stats.benched > 0, "{stats:?}");
    let dead = dead().await;
    let (stats, _) = three_dead_https_sites(&dead).await;
    assert!(stats.failures > 0 && stats.benched > 0, "{stats:?}");
}

#[tokio::test]
async fn per_host_leaves_a_site_whose_requests_go_direct_out_of_the_pool() {
    // Every pool proxy is dead, but the requests go direct: robots.txt goes
    // the way they do, and no pool session is made for the site.
    let site = Site::start(vec![("/a", Page::html("a"))]).await;
    let dead = dead().await;
    let mut s = settings(ProxyRotation::PerHost, &[]);
    s.proxies = vec![dead.clone()];
    s.obey_robots = true;
    let crawl = crawler(s);
    crawl.submit(CrawlRequest {
        proxy: Some(String::new()),
        ..request(1, &site.url("/a"))
    });
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events), [(1, 200)]);
    assert_eq!(site.paths(), ["/robots.txt", "/a"]);
    assert_eq!(stats_of(&crawl, &dead).requests, 0);
}

#[tokio::test]
async fn per_host_fetches_robots_txt_through_the_proxy_a_sites_requests_name() {
    let own = Site::start(vec![("http://shop.test/a", Page::html("a"))]).await;
    let dead = dead().await;
    let mut s = settings(ProxyRotation::PerHost, &[]);
    s.proxies = vec![dead.clone()];
    s.obey_robots = true;
    let crawl = crawler(s);
    crawl.submit(CrawlRequest {
        proxy: Some(proxy(&own)),
        ..request(1, "http://shop.test/a")
    });
    let events = drain(&crawl).await;
    assert_eq!(statuses(&events), [(1, 200)]);
    assert_eq!(
        own.paths(),
        ["http://shop.test/robots.txt", "http://shop.test/a"]
    );
    assert_eq!(stats_of(&crawl, &dead).requests, 0);
}
