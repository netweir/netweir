//! The crawl engine against a scripted local site.

mod site;

use std::time::{Duration, Instant};

use netweir_core::{
    CrawlRequest, CrawlSettings, Crawler, DropReason, Event, FetchOptions, Profile, Submitted,
};
use site::{Page, Site};

fn settings() -> CrawlSettings {
    CrawlSettings {
        // Fast by default here; throttling has its own tests.
        throttle: false,
        start_delay: Duration::ZERO,
        backoff_base: Duration::from_millis(10),
        backoff_max: Duration::from_millis(20),
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
        steps: None,
    }
}

/// Every event until the crawl is done.
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

fn fetched(events: &[Event]) -> Vec<u64> {
    let mut ids: Vec<u64> = events
        .iter()
        .filter_map(|e| {
            if let Event::Fetched { id, .. } = e {
                Some(*id)
            } else {
                None
            }
        })
        .collect();
    ids.sort();
    ids
}

fn dropped(events: &[Event]) -> Vec<(u64, DropReason)> {
    let mut out: Vec<(u64, DropReason)> = events
        .iter()
        .filter_map(|e| {
            if let Event::Dropped { id, reason } = e {
                Some((*id, *reason))
            } else {
                None
            }
        })
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

#[tokio::test]
async fn crawls_until_there_is_nothing_left() {
    let site = Site::start(vec![
        ("/a", Page::html("a")),
        ("/b", Page::html("b")),
        ("/c", Page::html("c")),
    ])
    .await;
    let c = crawler(settings());
    for (i, p) in ["/a", "/b", "/c"].iter().enumerate() {
        assert_eq!(c.submit(request(i as u64, site.url(p))), Submitted::Queued);
    }
    let events = drain(&c).await;
    assert_eq!(fetched(&events), [0, 1, 2]);
    let body = events.iter().find_map(|e| match e {
        Event::Fetched {
            id: 1, response, ..
        } => Some(response.text()),
        _ => None,
    });
    assert_eq!(body.as_deref(), Some("b"));
    let stats = c.stats();
    assert_eq!(stats.fetched, 3);
    assert_eq!(stats.queued, 0);
}

#[tokio::test]
async fn the_same_page_is_fetched_once() {
    let site = Site::start(vec![("/p?a=1&b=2", Page::html("p"))]).await;
    let c = crawler(settings());
    assert_eq!(
        c.submit(request(1, site.url("/p?a=1&b=2"))),
        Submitted::Queued
    );
    assert_eq!(
        c.submit(request(2, site.url("/p?b=2&a=1#x"))),
        Submitted::Duplicate
    );
    let mut again = request(3, site.url("/p?a=1&b=2"));
    again.dont_filter = true;
    assert_eq!(
        c.submit(again),
        Submitted::Queued,
        "dont_filter skips the check"
    );
    assert_eq!(c.submit(request(4, "not a url".into())), Submitted::Invalid);
    assert_eq!(fetched(&drain(&c).await), [1, 3]);
    assert_eq!(c.stats().duplicates, 1);
}

#[tokio::test]
async fn robots_txt_is_read_first_and_obeyed() {
    let robots = "User-agent: *\nDisallow: /private\n\nUser-agent: netweir\nDisallow: /secret\n";
    let site = Site::start(vec![
        ("/robots.txt", Page::status(200, robots)),
        ("/open", Page::html("o")),
        ("/private/x", Page::html("p")),
        ("/secret", Page::html("s")),
    ])
    .await;
    let c = crawler(settings());
    for (i, p) in ["/open", "/private/x", "/secret"].iter().enumerate() {
        c.submit(request(i as u64, site.url(p)));
    }
    let events = drain(&c).await;
    // netweir's own group applies, not "*": /private is allowed, /secret isn't.
    assert_eq!(fetched(&events), [0, 1]);
    assert_eq!(dropped(&events), [(2, DropReason::Robots)]);
    let paths = site.paths();
    assert_eq!(paths.first().map(String::as_str), Some("/robots.txt"));
    assert_eq!(
        paths.iter().filter(|p| *p == "/robots.txt").count(),
        1,
        "read once per site"
    );
    assert!(!paths.contains(&"/secret".to_string()));
}

#[tokio::test]
async fn robots_txt_errors_follow_rfc_9309() {
    // 4xx: no rules, everything allowed.
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/a", Page::html("a")),
    ])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/a")));
    assert_eq!(fetched(&drain(&c).await), [1]);

    // 5xx: the site can't say, so nothing is allowed.
    let site = Site::start(vec![
        ("/robots.txt", Page::status(503, "")),
        ("/a", Page::html("a")),
    ])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/a")));
    assert_eq!(dropped(&drain(&c).await), [(1, DropReason::Robots)]);
}

#[tokio::test]
async fn obey_robots_can_be_turned_off() {
    let site = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nDisallow: /\n"),
        ),
        ("/a", Page::html("a")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: false,
        ..settings()
    });
    c.submit(request(1, site.url("/a")));
    assert_eq!(fetched(&drain(&c).await), [1]);
    assert!(!site.paths().contains(&"/robots.txt".to_string()));
}

#[tokio::test]
async fn tdm_reservations_are_honoured() {
    let tdm = r#"[{"location": "/reserved/*", "tdm-reservation": 1}]"#;
    let site = Site::start(vec![
        ("/.well-known/tdmrep.json", Page::status(200, tdm)),
        ("/reserved/a", Page::html("r")),
        (
            "/header",
            Page::html("h").with_header("tdm-reservation", "1"),
        ),
        (
            "/meta",
            Page::html(r#"<head><meta name="tdm-reservation" content="1"></head>"#),
        ),
        ("/free", Page::html("f")),
    ])
    .await;
    let c = crawler(settings());
    for (i, p) in ["/reserved/a", "/header", "/meta", "/free"]
        .iter()
        .enumerate()
    {
        c.submit(request(i as u64, site.url(p)));
    }
    let events = drain(&c).await;
    assert_eq!(fetched(&events), [3]);
    assert_eq!(
        dropped(&events),
        [
            (0, DropReason::TdmReserved),
            (1, DropReason::TdmReserved),
            (2, DropReason::TdmReserved)
        ]
    );
    assert!(
        !site.paths().contains(&"/reserved/a".to_string()),
        "reserved by the file: never fetched"
    );

    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(9, site.url("/header")));
    assert_eq!(fetched(&drain(&c).await), [9]);
}

#[tokio::test]
async fn per_domain_concurrency_is_capped() {
    let slow = Duration::from_millis(200);
    let pages: Vec<(&str, Page)> = ["/1", "/2", "/3", "/4", "/5", "/6"]
        .iter()
        .map(|p| (*p, Page::html("x").slow(slow)))
        .collect();
    let site = Site::start(pages).await;
    let c = crawler(CrawlSettings {
        per_domain: 2,
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    let start = Instant::now();
    for i in 1..=6 {
        c.submit(request(i, site.url(&format!("/{i}"))));
    }
    assert_eq!(fetched(&drain(&c).await).len(), 6);
    let took = start.elapsed();
    assert!(
        took >= Duration::from_millis(580),
        "6 x 200 ms, 2 at a time, took {took:?}"
    );
    assert!(took < Duration::from_millis(1500), "took {took:?}");
}

#[tokio::test]
async fn throttling_spaces_requests_to_one_site() {
    let site = Site::start(vec![
        ("/1", Page::html("x")),
        ("/2", Page::html("x")),
        ("/3", Page::html("x")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        throttle: true,
        start_delay: Duration::from_millis(300),
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    for i in 1..=3 {
        c.submit(request(i, site.url(&format!("/{i}"))));
    }
    drain(&c).await;
    let t = site.times("/");
    assert_eq!(t.len(), 3);
    // The delay starts at 300 ms and halves towards the (tiny) latency.
    // These are arrival times, and the first request also opened the
    // connection, so it can arrive late and shorten the first gap by that
    // much on a slow machine.
    assert!(
        t[1] - t[0] >= Duration::from_millis(250),
        "{:?}",
        t[1] - t[0]
    );
    assert!(
        t[2] - t[1] >= Duration::from_millis(120),
        "{:?}",
        t[2] - t[1]
    );
}

#[tokio::test]
async fn crawl_delay_in_robots_txt_is_a_floor() {
    let site = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nCrawl-delay: 0.3\n"),
        ),
        ("/1", Page::html("x")),
        ("/2", Page::html("x")),
    ])
    .await;
    let c = crawler(settings());
    c.submit(request(1, site.url("/1")));
    c.submit(request(2, site.url("/2")));
    drain(&c).await;
    let (one, two) = (site.times("/1")[0], site.times("/2")[0]);
    assert!(two - one >= Duration::from_millis(280), "{:?}", two - one);
}

#[tokio::test]
async fn higher_priority_goes_first() {
    let site = Site::start(vec![
        ("/low", Page::html("l")),
        ("/high", Page::html("h")),
        ("/first", Page::html("f")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        per_domain: 1,
        throttle: true,
        start_delay: Duration::from_millis(100),
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(1, site.url("/first")));
    // Once /first is under way, the host's delay holds the next two back
    // long enough to be queued together.
    assert_eq!(fetched(&c.next(1).await), [1]);
    c.submit(request(2, site.url("/low")));
    let mut high = request(3, site.url("/high"));
    high.priority = 10;
    c.submit(high);
    drain(&c).await;
    assert_eq!(site.paths(), ["/first", "/high", "/low"]);
}

#[tokio::test]
async fn failures_are_events() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(7, format!("http://127.0.0.1:{port}/gone")));
    let events = drain(&c).await;
    assert!(
        matches!(&events[..], [Event::Failed { id: 7, error }] if error.kind == netweir_core::FetchErrorKind::Connect)
    );
    assert_eq!(c.stats().failed, 1);
}

#[tokio::test]
async fn requests_added_while_crawling_are_picked_up() {
    let site = Site::start(vec![
        ("/1", Page::html("x").slow(Duration::from_millis(100))),
        ("/2", Page::html("y")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(1, site.url("/1")));
    let first = c.next(10).await;
    assert_eq!(fetched(&first), [1]);
    c.submit(request(2, site.url("/2")));
    assert_eq!(fetched(&drain(&c).await), [2]);
}

#[tokio::test]
async fn a_robots_txt_that_breaks_the_parser_does_not_hang_the_crawl() {
    let site = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nDisallow: /%aé\n"),
        ),
        ("/a", Page::html("a")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(1, site.url("/a")));
    assert_eq!(fetched(&drain(&c).await), [1]);
}

fn redirect(to: &str) -> Page {
    Page::status(302, "").with_header("location", to)
}

#[tokio::test]
async fn a_redirect_to_another_site_obeys_that_sites_robots_txt() {
    let other = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nDisallow: /secret\n"),
        ),
        ("/secret", Page::html("secret")),
        ("/open", Page::html("open")),
    ])
    .await;
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/go", redirect(&other.url("/secret"))),
        ("/go2", redirect(&other.url("/open"))),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(1, site.url("/go")));
    c.submit(request(2, site.url("/go2")));
    let events = drain(&c).await;
    assert_eq!(dropped(&events), [(1, DropReason::Robots)]);
    assert_eq!(fetched(&events), [2]);
    let final_url = events.iter().find_map(|e| match e {
        Event::Fetched { response, .. } => Some(response.url.clone()),
        _ => None,
    });
    assert_eq!(final_url, Some(other.url("/open")));
    assert!(!other.paths().contains(&"/secret".to_string()));
    assert_eq!(other.paths()[0], "/robots.txt", "read before anything else");
}

#[tokio::test]
async fn a_redirect_target_counts_as_seen() {
    let site = Site::start(vec![
        ("/go", redirect("/target")),
        ("/target", Page::html("t")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        per_domain: 1,
        ..settings()
    });
    c.submit(request(1, site.url("/go")));
    let first = drain(&c).await;
    assert_eq!(fetched(&first), [1]);
    assert_eq!(
        c.submit(request(2, site.url("/target"))),
        Submitted::Duplicate
    );
}

#[tokio::test]
async fn redirect_loops_end_as_failures() {
    let site = Site::start(vec![("/loop", redirect("/loop"))]).await;
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings()
    });
    c.submit(request(1, site.url("/loop")));
    let events = drain(&c).await;
    assert!(
        matches!(&events[..], [Event::Failed { id: 1, error }] if error.kind == netweir_core::FetchErrorKind::TooManyRedirects),
    );
    assert_eq!(site.paths().len(), 21, "the first request and 20 redirects");
}

#[tokio::test]
async fn the_engine_does_not_run_far_ahead_of_its_reader() {
    let pages: Vec<(String, Page)> = (0..300)
        .map(|i| (format!("/p{i}"), Page::html("x")))
        .collect();
    let site = Site::start(
        pages
            .iter()
            .map(|(p, page)| (p.as_str(), page.clone()))
            .collect(),
    )
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        concurrency: 4,
        per_domain: 4,
        ..settings()
    });
    for i in 0..300 {
        c.submit(request(i, site.url(&format!("/p{i}"))));
    }
    let first = c.next(1).await;
    assert_eq!(first.len(), 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let served = site.paths().len();
    assert!(
        served <= 4 * 3 + 1,
        "served {served} pages nobody asked for yet"
    );
    assert_eq!(fetched(&drain(&c).await).len(), 299);
}

#[tokio::test]
async fn robots_txt_fetches_count_against_concurrency() {
    // Two host names for one machine, so the per-host rule can't hide it.
    let mut sites = Vec::new();
    for _ in 0..2 {
        sites.push(
            Site::start(vec![
                (
                    "/robots.txt",
                    Page::status(404, "").slow(Duration::from_millis(150)),
                ),
                ("/a", Page::html("a")),
            ])
            .await,
        );
    }
    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        concurrency: 1,
        ..settings()
    });
    c.submit(request(0, sites[0].url("/a")));
    c.submit(request(
        1,
        sites[1].url("/a").replace("127.0.0.1", "localhost"),
    ));
    assert_eq!(fetched(&drain(&c).await), [0, 1]);
    let mut starts: Vec<Instant> = sites.iter().flat_map(|s| s.times("/robots.txt")).collect();
    starts.sort();
    for pair in starts.windows(2) {
        assert!(
            pair[1] - pair[0] >= Duration::from_millis(140),
            "two robots.txt fetches overlapped with concurrency=1"
        );
    }
}

#[tokio::test]
async fn a_huge_crawl_delay_is_capped_at_max_delay() {
    let site = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nCrawl-delay: 1e300\n"),
        ),
        ("/a", Page::html("a")),
        ("/b", Page::html("b")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        max_delay: Duration::from_millis(300),
        ..settings()
    });
    c.submit(request(1, site.url("/a")));
    c.submit(request(2, site.url("/b")));
    assert_eq!(fetched(&drain(&c).await).len(), 2);
    let t = site.times("/");
    let gap = t[t.len() - 1] - t[t.len() - 2];
    assert!(
        gap >= Duration::from_millis(280),
        "gap {gap:?}: the delay was ignored"
    );
    assert!(gap < Duration::from_secs(2), "gap {gap:?}");
}

#[tokio::test]
async fn traps_are_refused_at_the_door() {
    let site = Site::start(vec![("/a", Page::html("a"))]).await;
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        max_depth: Some(2),
        max_pages_per_domain: Some(3),
        ..settings()
    });
    let deep = CrawlRequest {
        depth: 3,
        ..request(1, site.url("/deep"))
    };
    assert_eq!(c.submit(deep), Submitted::TooDeep);
    let ok_depth = CrawlRequest {
        depth: 2,
        ..request(2, site.url("/a"))
    };
    assert_eq!(c.submit(ok_depth), Submitted::Queued);
    assert_eq!(
        c.submit(request(3, site.url("/x/y/x/y/x/y/page"))),
        Submitted::Trap
    );
    assert_eq!(c.submit(request(4, site.url("/b"))), Submitted::Queued);
    assert_eq!(c.submit(request(5, site.url("/c"))), Submitted::Queued);
    assert_eq!(c.submit(request(6, site.url("/d"))), Submitted::DomainFull);
    let other = request(7, site.url("/e").replace("127.0.0.1", "localhost"));
    assert_eq!(c.submit(other), Submitted::Queued, "the limit is per host");
    let stats = c.stats();
    assert_eq!(
        (
            stats.skipped_depth,
            stats.skipped_traps,
            stats.skipped_domain_full
        ),
        (1, 1, 1)
    );
    drain(&c).await;
}

#[tokio::test]
async fn max_pages_sends_that_many_requests_and_ends_the_crawl() {
    let site = Site::start(vec![("/", Page::html("p"))]).await;
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        concurrency: 3,
        max_pages: Some(4),
        ..settings()
    });
    for i in 0..10 {
        c.submit(request(i, site.url(&format!("/p{i}"))));
    }
    // Not read until every fetch could have finished: nothing waiting to
    // be read makes the scheduler start more than the limit.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fetched(&drain(&c).await).len(), 4);
    let stats = c.stats();
    assert_eq!((stats.fetched, stats.queued, stats.in_flight), (4, 6, 0));
}

#[tokio::test]
async fn a_download_starts_past_max_pages_and_a_full_backlog_and_comes_back_on_its_own() {
    let robots = "User-agent: *\nDisallow: /private\n";
    let site = Site::start(vec![
        ("/robots.txt", Page::status(200, robots)),
        ("/", Page::html("p")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        concurrency: 1,
        max_pages: Some(2),
        ..settings()
    });
    for i in 0..4 {
        // The highest priority a page can ask for still comes after a
        // download.
        let mut page = request(i, site.url(&format!("/p{i}")));
        page.priority = i32::MAX;
        c.submit(page);
    }
    // Unread, the two pages fill the backlog (twice the concurrency) and
    // max_pages: no page may start, but a download still does.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut file = request(100, site.url("/file.pdf"));
    file.outgoing = netweir_core::Outgoing::image(Some(site.url("/p0")));
    let done = tokio::time::timeout(Duration::from_secs(10), c.download(file))
        .await
        .expect("the download waited behind the pages")
        .expect("the crawl is still running");
    assert!(!done.cached);
    assert!(matches!(done.event, Event::Fetched { id: 100, .. }));
    let private = c
        .download(request(101, site.url("/private/file.pdf")))
        .await;
    assert!(matches!(
        private.map(|d| d.event),
        Some(Event::Dropped {
            id: 101,
            reason: DropReason::Robots
        })
    ));
    // Neither counted as a page, nor came back as an event.
    assert_eq!(fetched(&drain(&c).await), [0, 1]);
    assert_eq!(c.stats().queued, 2);
}

#[tokio::test]
async fn dropping_the_crawler_closes_its_checkpoint() {
    let dir = std::env::temp_dir().join(format!("netweir-crawl-close-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("crawl.sqlite3");
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        checkpoint: Some(path.clone()),
        ..settings()
    });
    for i in 0..2000 {
        c.submit(request(i, format!("http://127.0.0.1:9/{i}")));
    }
    // No flush: dropping the crawler has to commit everything and let go
    // of the file before it returns, or the directory can't be removed on
    // Windows.
    drop(c);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2000);
    drop(conn);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_checkpoint_remembers_what_was_left() {
    let site = Site::start(vec![
        ("/a", Page::html("a")),
        ("/b", Page::html("b")),
        ("/c", Page::html("c")),
    ])
    .await;
    let dir = std::env::temp_dir().join(format!("netweir-crawl-cp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let with_checkpoint = || CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        checkpoint: Some(dir.join("crawl.sqlite3")),
        ..settings()
    };
    {
        let c = crawler(with_checkpoint());
        assert!(c.saved().unwrap().pending.is_empty());
        for (i, p) in ["/a", "/b", "/c"].iter().enumerate() {
            let payload = format!(r#"{{"n":{i}}}"#);
            assert_eq!(
                c.submit_with(request(i as u64, site.url(p)), &payload),
                Submitted::Queued
            );
        }
        assert_eq!(fetched(&drain(&c).await), [0, 1, 2]);
        // Only /a's page was dealt with before the "crash".
        c.done(&[0]);
        c.flush().unwrap();
    }
    let c = crawler(with_checkpoint());
    let saved = c.saved().unwrap();
    let left: Vec<(&str, &str)> = saved
        .pending
        .iter()
        .map(|p| (p.url.rsplit('/').next().unwrap(), p.payload.as_str()))
        .collect();
    assert_eq!(left, [("b", r#"{"n":1}"#), ("c", r#"{"n":2}"#)]);
    assert_eq!(
        c.submit(request(9, site.url("/a"))),
        Submitted::Duplicate,
        "what was seen stays seen"
    );
    for (i, p) in saved.pending.iter().enumerate() {
        assert_eq!(
            c.resubmit(request(10 + i as u64, p.url.clone()), p.row),
            Submitted::Queued
        );
    }
    assert_eq!(fetched(&drain(&c).await), [10, 11]);
    c.done(&[10, 11]);
    c.flush().unwrap();
    drop(c);
    let c = crawler(with_checkpoint());
    assert!(c.saved().unwrap().pending.is_empty());
    drop(c);
    std::fs::remove_dir_all(&dir).unwrap();
}
