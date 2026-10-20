//! The development cache: a rerun is served from disk.

mod site;

use std::path::PathBuf;
use std::time::Duration;

use netweir_core::{
    CrawlRequest, CrawlSettings, Crawler, Event, FetchOptions, Profile, Response, Submitted,
};
use site::{Page, Site};

/// A fresh cache file, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("netweir-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Dir(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("cache.sqlite3")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn settings(dir: &Dir) -> CrawlSettings {
    CrawlSettings {
        throttle: false,
        start_delay: Duration::ZERO,
        backoff_base: Duration::from_millis(10),
        backoff_max: Duration::from_millis(20),
        cache: Some(dir.file()),
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

/// Each fetched page's id and response, by id.
fn pages(events: &[Event]) -> Vec<(u64, Response)> {
    let mut out: Vec<(u64, Response)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Fetched { id, response, .. } => Some((*id, response.clone())),
            _ => None,
        })
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

/// Runs a crawl of `paths` on `site` and returns its pages and stats.
async fn run(
    settings: CrawlSettings,
    site: &Site,
    paths: &[&str],
) -> (Vec<(u64, Response)>, netweir_core::Stats) {
    let c = crawler(settings);
    for (i, p) in paths.iter().enumerate() {
        c.submit(request(i as u64, site.url(p)));
    }
    let events = drain(&c).await;
    (pages(&events), c.stats())
}

#[tokio::test]
async fn a_rerun_is_served_from_the_cache_without_asking_the_site() {
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/a", Page::html("a").with_header("x-made", "here")),
        ("/b", Page::html("b")),
        ("/gone", Page::status(404, "gone")),
    ])
    .await;
    let dir = Dir::new("rerun");
    let (first, stats) = run(settings(&dir), &site, &["/a", "/b", "/gone"]).await;
    assert_eq!(first.len(), 3);
    assert_eq!((stats.cache_hits, stats.cache_stores), (0, 3));
    let asked = site.paths().len();

    let (second, stats) = run(settings(&dir), &site, &["/a", "/b", "/gone"]).await;
    assert_eq!(site.paths().len(), asked, "{:?}", site.paths());
    assert_eq!((stats.cache_hits, stats.cache_stores), (3, 0));
    assert_eq!(stats.fetched, 3, "hits are pages the caller got");
    for ((id1, r1), (id2, r2)) in first.iter().zip(&second) {
        assert_eq!(id1, id2);
        assert_eq!(
            (&r1.url, r1.status, r1.version, &r1.headers, &r1.body),
            (&r2.url, r2.status, r2.version, &r2.headers, &r2.body)
        );
    }
}

#[tokio::test]
async fn a_hit_waits_for_no_delay() {
    let site = Site::start(vec![("/a", Page::html("a")), ("/b", Page::html("b"))]).await;
    let dir = Dir::new("delay");
    let slow = || CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        min_delay: Duration::from_secs(6),
        max_delay: Duration::from_secs(6),
        ..settings(&dir)
    };
    run(slow(), &site, &["/a", "/b"]).await;
    let started = std::time::Instant::now();
    let (second, _) = run(slow(), &site, &["/a", "/b"]).await;
    assert_eq!(second.len(), 2);
    // A crawl with no wait took 2.2 s on a loaded Windows runner: the
    // bound leaves room for that, and a hit that waited would take 6 s.
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_redirect_is_kept_as_where_it_ended() {
    let site = Site::start(vec![
        (
            "/go",
            Page::status(302, "").with_header("location", "/there"),
        ),
        ("/there", Page::html("t")),
    ])
    .await;
    let dir = Dir::new("redirect");
    let s = || CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings(&dir)
    };
    run(s(), &site, &["/go"]).await;
    assert_eq!(site.paths(), ["/go", "/there"]);
    let c = crawler(s());
    c.submit(request(1, site.url("/go")));
    let second = pages(&drain(&c).await);
    assert_eq!(site.paths().len(), 2, "no hops on a hit");
    assert_eq!(second[0].1.url, site.url("/there"));
    assert_eq!(c.stats().cache_hits, 1);
    assert_eq!(
        c.submit(request(2, site.url("/there"))),
        Submitted::Duplicate,
        "where it led counts as seen, as when it was followed"
    );
}

#[tokio::test]
async fn only_answers_that_would_come_again_are_kept() {
    let block = Page::status(
        403,
        "<title>Attention Required! | Cloudflare</title><h1>Sorry, you have been blocked</h1>",
    )
    .with_header("server", "cloudflare");
    let bad = [
        ("/blocked", block),
        ("/busy", Page::status(503, "busy")),
        ("/slow-down", Page::status(429, "wait")),
        ("/pay", Page::status(402, "pay")),
        ("/who", Page::status(401, "who are you")),
        // A WAF netweir doesn't know yet looks like this.
        ("/forbidden", Page::status(403, "no")),
        ("/proxy", Page::status(407, "your proxy wants a login")),
        ("/legal", Page::status(451, "not here")),
        ("/same", Page::status(304, "")),
        (
            "/reserved",
            Page::html("r").with_header("tdm-reservation", "1"),
        ),
    ];
    let paths: Vec<&str> = bad.iter().map(|(p, _)| *p).collect();
    let mut pages_ = bad.to_vec();
    pages_.push(("/ok", Page::html("ok")));
    let site = Site::start(pages_).await;
    let dir = Dir::new("bad");
    let s = || CrawlSettings {
        obey_robots: false,
        retries: 0,
        ..settings(&dir)
    };
    for round in 0..2 {
        let c = crawler(s());
        for (i, p) in paths.iter().chain(&["/ok"]).enumerate() {
            c.submit(request(i as u64, site.url(p)));
        }
        // A network failure: nothing listens there.
        c.submit(request(99, "http://127.0.0.1:9/down".into()));
        drain(&c).await;
        let stats = c.stats();
        assert_eq!(
            (stats.cache_stores, stats.cache_hits),
            if round == 0 { (1, 0) } else { (0, 1) }
        );
    }
    let asked = |p: &str| site.paths().iter().filter(|x| *x == p).count();
    for p in paths {
        assert_eq!(asked(p), 2, "{p} was kept");
    }
    assert_eq!(asked("/ok"), 1);
}

#[tokio::test]
async fn a_stale_entry_is_fetched_again_and_replaced() {
    let site = Site::scripted(vec![("/a", vec![Page::html("old"), Page::html("new")])]).await;
    let dir = Dir::new("expiry");
    let s = || CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        cache_expiry: Some(Duration::from_secs(600)),
        ..settings(&dir)
    };
    run(s(), &site, &["/a"]).await;
    let (fresh, _) = run(s(), &site, &["/a"]).await;
    assert_eq!(fresh[0].1.text(), "old");
    age(&dir, 601.0);
    let (stale, stats) = run(s(), &site, &["/a"]).await;
    assert_eq!(stale[0].1.text(), "new");
    assert_eq!((stats.cache_hits, stats.cache_stores), (0, 1));
    let (again, stats) = run(s(), &site, &["/a"]).await;
    assert_eq!(again[0].1.text(), "new", "replaced");
    assert_eq!(stats.cache_hits, 1);
    assert_eq!(site.paths().len(), 2);
}

#[tokio::test]
async fn browser_requests_skip_the_cache() {
    let site = Site::start(vec![("/a", Page::html("a"))]).await;
    let dir = Dir::new("browser");
    let s = || CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings(&dir)
    };
    run(s(), &site, &["/a"]).await;
    let c = crawler(CrawlSettings {
        // Wherever Chrome is or isn't installed, it isn't asked to start.
        browser_launch: netweir_browser_missing(),
        ..s()
    });
    let mut r = request(1, site.url("/a"));
    r.browser = true;
    c.submit(r);
    let events = drain(&c).await;
    assert!(
        matches!(&events[..], [Event::Failed { id: 1, .. }]),
        "{events:?}"
    );
    let stats = c.stats();
    assert_eq!((stats.cache_hits, stats.cache_stores), (0, 0));
}

/// Launch options that name a Chrome that doesn't exist.
fn netweir_browser_missing() -> netweir_browser::LaunchOptions {
    netweir_browser::LaunchOptions {
        executable: Some("/nonexistent/chrome".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn hits_count_toward_max_pages() {
    let site = Site::start(vec![]).await;
    let dir = Dir::new("max-pages");
    let s = |max| CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        max_pages: max,
        ..settings(&dir)
    };
    let paths: Vec<String> = (0..10).map(|i| format!("/p{i}")).collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    run(s(None), &site, &paths).await;
    let (second, stats) = run(s(Some(4)), &site, &paths).await;
    assert_eq!(second.len(), 4);
    assert_eq!((stats.cache_hits, stats.queued, stats.in_flight), (4, 6, 0));
    assert_eq!(site.paths().len(), 10);
}

#[tokio::test]
async fn a_checkpoint_works_the_same_with_the_cache() {
    let site = Site::start(vec![("/a", Page::html("a")), ("/b", Page::html("b"))]).await;
    let dir = Dir::new("checkpoint");
    let s = || CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        checkpoint: Some(dir.0.join("crawl.sqlite3")),
        ..settings(&dir)
    };
    {
        let c = crawler(s());
        c.submit(request(0, site.url("/a")));
        c.submit(request(1, site.url("/b")));
        assert_eq!(pages(&drain(&c).await).len(), 2);
        // Only /a was dealt with before the "crash".
        c.done(&[0]);
        c.flush().unwrap();
    }
    let c = crawler(s());
    let saved = c.saved().unwrap();
    assert_eq!(saved.pending.len(), 1);
    c.resubmit(
        request(5, saved.pending[0].url.clone()),
        saved.pending[0].row,
    );
    let events = drain(&c).await;
    assert_eq!(pages(&events)[0].0, 5);
    assert_eq!(
        c.stats().cache_hits,
        1,
        "the page left over comes from the cache"
    );
    c.done(&[5]);
    c.flush().unwrap();
    drop(c);
    let c = crawler(s());
    assert!(c.saved().unwrap().pending.is_empty(), "the hit was acked");
    assert_eq!(site.paths().len(), 2);
}

#[tokio::test]
async fn a_cache_from_a_newer_netweir_is_refused_untouched() {
    let dir = Dir::new("newer");
    std::fs::create_dir_all(&dir.0).unwrap();
    rusqlite::Connection::open(dir.file())
        .unwrap()
        .execute_batch("CREATE TABLE future (x); PRAGMA user_version = 99;")
        .unwrap();
    let err = Crawler::new(
        FetchOptions::new(Profile::named("chrome").unwrap()),
        settings(&dir),
    )
    .err()
    .unwrap();
    assert!(err.message.contains("newer netweir"), "{}", err.message);
    let conn = rusqlite::Connection::open(dir.file()).unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'responses'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!((tables, journal.as_str()), (0, "delete"));
}

#[tokio::test]
async fn admitting_a_request_never_waits_for_the_cache_file() {
    let site = Site::start(vec![("/a", Page::html("a")), ("/b", Page::html("b"))]).await;
    let dir = Dir::new("held");
    let s = || CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings(&dir)
    };
    run(s(), &site, &["/a"]).await;
    let c = crawler(s());
    // Another process is writing to the file for a while, so storing /b
    // has to wait for it.
    let holder = rusqlite::Connection::open(dir.file()).unwrap();
    holder
        .execute_batch("BEGIN IMMEDIATE; UPDATE responses SET stored_at = stored_at;")
        .unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        holder.execute_batch("COMMIT").unwrap();
    });
    c.submit(request(2, site.url("/b")));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    c.submit(request(1, site.url("/a")));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
    release.join().unwrap();
    let events = drain(&c).await;
    assert_eq!(pages(&events).len(), 2);
    assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
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

/// Breaks the cache under a crawl that has it open, as another process
/// or a failing disk might: every read and write fails from then on.
fn break_cache(dir: &Dir) {
    rusqlite::Connection::open(dir.file())
        .unwrap()
        .execute("DROP TABLE responses", [])
        .unwrap();
}

#[tokio::test]
async fn a_cache_that_cant_be_read_is_warned_about_and_the_page_fetched() {
    let site = Site::start(vec![("/a", Page::html("a"))]).await;
    let dir = Dir::new("unreadable");
    let s = || CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings(&dir)
    };
    run(s(), &site, &["/a"]).await;
    let c = crawler(s());
    break_cache(&dir);
    c.submit(request(1, site.url("/a")));
    let events = drain(&c).await;
    assert_eq!(pages(&events)[0].1.text(), "a");
    assert_eq!(site.paths().len(), 2, "fetched after all");
    let warned = warnings(&events);
    assert!(
        warned.len() == 1 && warned[0].contains("couldn't be read"),
        "{warned:?}"
    );
    assert!(
        warned[0].contains(&dir.file().display().to_string()),
        "{}",
        warned[0]
    );
    assert_eq!(c.stats().cache_hits, 0);
}

#[tokio::test]
async fn a_cache_that_cant_be_written_is_warned_about_once() {
    let site = Site::start(vec![("/a", Page::html("a")), ("/b", Page::html("b"))]).await;
    let dir = Dir::new("unwritable");
    let c = crawler(CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings(&dir)
    });
    break_cache(&dir);
    c.submit(request(1, site.url("/a")));
    c.submit(request(2, site.url("/b")));
    let events = drain(&c).await;
    assert_eq!(pages(&events).len(), 2, "the crawl goes on");
    let warned = warnings(&events);
    assert!(
        warned.len() == 1 && warned[0].contains("couldn't be written"),
        "once per crawl: {warned:?}"
    );
    assert!(
        warned[0].contains(&dir.file().display().to_string()),
        "{}",
        warned[0]
    );
    assert_eq!(c.stats().cache_stores, 0);
}

/// Makes every entry `seconds` older, rather than waiting that long.
fn age(dir: &Dir, seconds: f64) {
    rusqlite::Connection::open(dir.file())
        .unwrap()
        .execute("UPDATE responses SET stored_at = stored_at - ?1", [seconds])
        .unwrap();
}

fn quiet(dir: &Dir) -> CrawlSettings {
    CrawlSettings {
        obey_robots: false,
        obey_tdmrep: false,
        ..settings(dir)
    }
}

fn texts(events: &[Event]) -> Vec<String> {
    pages(events).iter().map(|(_, r)| r.text()).collect()
}

#[tokio::test]
async fn cookies_a_hit_set_reach_the_session() {
    let site = Site::start(vec![
        (
            "/login",
            Page::html("in").with_header("set-cookie", "sid=ok; Path=/"),
        ),
        (
            "/secret",
            Page::html("PLEASE LOG IN").unless_cookie("sid=ok", Page::html("SECRET")),
        ),
    ])
    .await;
    let dir = Dir::new("cookie");
    run(quiet(&dir), &site, &["/login"]).await;
    let c = crawler(quiet(&dir));
    c.submit(request(1, site.url("/login")));
    drain(&c).await;
    assert_eq!(c.stats().cache_hits, 1);
    c.submit(request(2, site.url("/secret")));
    assert_eq!(texts(&drain(&c).await), ["SECRET"]);
}

#[tokio::test]
async fn cookies_set_on_the_way_through_a_redirect_reach_the_session() {
    let site = Site::start(vec![
        (
            "/go",
            Page::status(302, "")
                .with_header("location", "/home")
                .with_header("set-cookie", "sid=ok; Path=/"),
        ),
        ("/home", Page::html("home")),
        (
            "/secret",
            Page::html("PLEASE LOG IN").unless_cookie("sid=ok", Page::html("SECRET")),
        ),
    ])
    .await;
    let dir = Dir::new("hop-cookie");
    run(quiet(&dir), &site, &["/go"]).await;
    let c = crawler(quiet(&dir));
    c.submit(request(1, site.url("/go")));
    drain(&c).await;
    assert_eq!(c.stats().cache_hits, 1);
    c.submit(request(2, site.url("/secret")));
    assert_eq!(texts(&drain(&c).await), ["SECRET"]);
}

#[tokio::test]
async fn every_hop_of_a_cached_redirect_counts_as_seen_and_is_saved() {
    let site = Site::start(vec![
        ("/a", Page::status(301, "").with_header("location", "/b")),
        ("/b", Page::status(301, "").with_header("location", "/c")),
        ("/c", Page::html("c")),
    ])
    .await;
    let dir = Dir::new("hops");
    run(quiet(&dir), &site, &["/a"]).await;
    let s = || CrawlSettings {
        checkpoint: Some(dir.0.join("crawl.sqlite3")),
        ..quiet(&dir)
    };
    {
        let c = crawler(s());
        c.submit(request(1, site.url("/a")));
        drain(&c).await;
        assert_eq!(c.stats().cache_hits, 1);
        for (id, p) in [(2, "/b"), (3, "/c")] {
            assert_eq!(
                c.submit(request(id, site.url(p))),
                Submitted::Duplicate,
                "{p}"
            );
        }
        c.flush().unwrap();
    }
    let c = crawler(s());
    for (id, p) in [(2, "/b"), (3, "/c")] {
        assert_eq!(
            c.submit(request(id, site.url(p))),
            Submitted::Duplicate,
            "{p} after a resume"
        );
    }
    assert_eq!(site.paths().len(), 3);
}

#[tokio::test]
async fn a_page_kept_by_a_laxer_run_is_not_served_to_a_stricter_one() {
    let site = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nDisallow: /private\n"),
        ),
        ("/private", Page::html("p")),
        ("/open", Page::html("o")),
    ])
    .await;
    let dir = Dir::new("strict");
    run(quiet(&dir), &site, &["/private"]).await;
    let robots = CrawlSettings {
        obey_robots: true,
        ..quiet(&dir)
    };
    let (served, _) = run(robots.clone(), &site, &["/private"]).await;
    assert!(served.is_empty(), "robots.txt disallows it");

    // TDMRep obeyed now, and not then.
    let tdm = CrawlSettings {
        obey_tdmrep: true,
        ..quiet(&dir)
    };
    run(quiet(&dir), &site, &["/open"]).await;
    let (_, stats) = run(tdm, &site, &["/open"]).await;
    assert_eq!(stats.cache_hits, 0);

    // Another robots agent.
    run(robots.clone(), &site, &["/open"]).await;
    let (_, stats) = run(robots.clone(), &site, &["/open"]).await;
    assert_eq!(stats.cache_hits, 1, "the same rules");
    let other = CrawlSettings {
        robots_agent: "other".into(),
        ..robots
    };
    let (_, stats) = run(other, &site, &["/open"]).await;
    assert_eq!(stats.cache_hits, 0);

    // A stricter run's page is good for a laxer one.
    let (_, stats) = run(quiet(&dir), &site, &["/open"]).await;
    assert_eq!(stats.cache_hits, 1);
}

#[tokio::test]
async fn a_hit_comes_back_with_the_url_it_was_asked_for() {
    let site = Site::start(vec![
        ("/a?utm_source=x&z=1", Page::html("a")),
        ("/a?z=1", Page::html("a")),
    ])
    .await;
    let dir = Dir::new("url");
    run(quiet(&dir), &site, &["/a?utm_source=x&z=1"]).await;
    let (served, stats) = run(quiet(&dir), &site, &["/a?z=1"]).await;
    assert_eq!(stats.cache_hits, 1);
    assert_eq!(served[0].1.url, site.url("/a?z=1"));
}

#[tokio::test]
async fn closing_the_crawler_lets_go_of_the_cache() {
    let site = Site::start(vec![("/a", Page::html("a"))]).await;
    let dir = Dir::new("close");
    let c = crawler(quiet(&dir));
    c.submit(request(1, site.url("/a")));
    drain(&c).await;
    assert!(dir.0.join("cache.sqlite3-wal").exists());
    c.close().unwrap();
    // The last connection to close removes the write-ahead log; one still
    // open would keep it, and on Windows keep the directory from going.
    assert!(!dir.0.join("cache.sqlite3-wal").exists());
    std::fs::remove_dir_all(&dir.0).unwrap();
}

#[tokio::test]
async fn dropping_a_crawler_does_not_wait_for_a_stuck_write() {
    let site = Site::start(vec![("/b", Page::html("b"))]).await;
    let dir = Dir::new("stuck");
    drop(crawler(quiet(&dir)));
    let c = crawler(quiet(&dir));
    let holder = rusqlite::Connection::open(dir.file()).unwrap();
    holder
        .execute_batch("BEGIN IMMEDIATE; DELETE FROM responses;")
        .unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        holder.execute_batch("COMMIT").unwrap();
    });
    c.submit(request(1, site.url("/b")));
    // Long enough for the fetch to be waiting to store /b.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = std::time::Instant::now();
    drop(c);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    release.join().unwrap();
}

#[tokio::test]
async fn a_file_that_isnt_a_cache_is_refused_by_name() {
    let dir = Dir::new("garbage");
    std::fs::create_dir_all(&dir.0).unwrap();
    std::fs::write(dir.file(), b"this is not a database, not even close to one").unwrap();
    let err = Crawler::new(
        FetchOptions::new(Profile::named("chrome").unwrap()),
        settings(&dir),
    )
    .err()
    .unwrap();
    assert!(
        err.message.contains(&dir.file().display().to_string()) && err.message.contains("delete"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn stored_data_that_cant_be_read_is_a_miss_with_a_warning() {
    let site = Site::start(vec![("/a", Page::html("a"))]).await;
    let dir = Dir::new("undecodable");
    run(quiet(&dir), &site, &["/a"]).await;
    rusqlite::Connection::open(dir.file())
        .unwrap()
        .execute("UPDATE responses SET headers = 'not json'", [])
        .unwrap();
    let c = crawler(quiet(&dir));
    c.submit(request(1, site.url("/a")));
    let events = drain(&c).await;
    assert_eq!(texts(&events), ["a"]);
    let warned = warnings(&events);
    assert!(
        warned.len() == 1 && warned[0].contains("couldn't be read"),
        "{warned:?}"
    );
    assert_eq!(c.stats().cache_hits, 0);
}

#[tokio::test]
async fn hits_are_not_counted_as_bytes_downloaded() {
    let site = Site::start(vec![("/a", Page::html("aaaa"))]).await;
    let dir = Dir::new("bytes");
    let (_, stats) = run(quiet(&dir), &site, &["/a"]).await;
    assert_eq!(stats.bytes, 4);
    let (_, stats) = run(quiet(&dir), &site, &["/a"]).await;
    assert_eq!((stats.cache_hits, stats.bytes), (1, 0));
}

#[tokio::test]
async fn a_hit_gone_or_stale_by_the_time_it_is_served_is_fetched() {
    let site = Site::start(vec![]).await;
    let dir = Dir::new("vanish");
    let s = || CrawlSettings {
        concurrency: 1,
        cache_expiry: Some(Duration::from_secs(600)),
        // Every one of the four is still sent: a hit that turns out to be
        // a fetch counts once.
        max_pages: Some(4),
        ..quiet(&dir)
    };
    let paths = ["/p0", "/p1", "/p2", "/p3"];
    run(s(), &site, &paths).await;
    let c = crawler(s());
    for (i, p) in paths.iter().enumerate() {
        c.submit(request(i as u64, site.url(p)));
    }
    // The backlog holds two; the other two wait to be served.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let db = rusqlite::Connection::open(dir.file()).unwrap();
    db.execute("DELETE FROM responses WHERE url LIKE '%/p2'", [])
        .unwrap();
    db.execute(
        "UPDATE responses SET stored_at = stored_at - 601 WHERE url LIKE '%/p3'",
        [],
    )
    .unwrap();
    drop(db);
    let events = drain(&c).await;
    assert_eq!(pages(&events).len(), 4);
    assert_eq!(c.stats().cache_hits, 2);
    let asked = site.paths();
    assert_eq!(asked.len(), 6, "{asked:?}");
}

#[tokio::test]
async fn max_pages_is_exact_with_hits_and_fetches_mixed() {
    let site = Site::start(vec![]).await;
    let dir = Dir::new("mixed");
    let paths: Vec<String> = (0..10).map(|i| format!("/p{i}")).collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    run(quiet(&dir), &site, &paths[..5]).await;
    let (served, stats) = run(
        CrawlSettings {
            max_pages: Some(7),
            ..quiet(&dir)
        },
        &site,
        &paths,
    )
    .await;
    assert_eq!(served.len(), 7);
    assert_eq!((stats.queued, stats.in_flight), (3, 0));
    assert_eq!(site.paths().len() as u64, 5 + 7 - stats.cache_hits);
}
