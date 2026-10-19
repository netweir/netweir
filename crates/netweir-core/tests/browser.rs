//! Requests fetched in Chrome: asked for, always, or when a site keeps
//! blocking. Skipped without Chrome unless NETWEIR_REQUIRE_CHROME is set.

mod site;

use std::time::Duration;

use netweir_browser::LaunchOptions;
use netweir_core::{
    BrowserMode, CrawlRequest, CrawlSettings, Crawler, Event, FetchOptions, Profile,
};
use site::{Page, Site};

/// A turn with Chrome, or None (and a note) when there isn't one. Only
/// two tests have a Chrome at once: a small CI runner with one each, all
/// at once, takes longer over a page than a test waits.
async fn chrome() -> Option<tokio::sync::SemaphorePermit<'static>> {
    static TURNS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    match netweir_browser::find_chrome() {
        Ok(_) => Some(TURNS.acquire().await.unwrap()),
        Err(e) if std::env::var_os("NETWEIR_REQUIRE_CHROME").is_some() => panic!("{e}"),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn settings(browser: BrowserMode) -> CrawlSettings {
    CrawlSettings {
        throttle: false,
        start_delay: Duration::ZERO,
        obey_robots: false,
        obey_tdmrep: false,
        backoff_base: Duration::from_millis(10),
        backoff_max: Duration::from_millis(50),
        retries: 1,
        browser,
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

fn request(id: u64, url: String, browser: bool) -> CrawlRequest {
    CrawlRequest {
        id,
        url,
        priority: 0,
        headers: Vec::new(),
        dont_filter: false,
        depth: 0,
        browser,

        outgoing: netweir_core::Outgoing::navigate(),
        retry_post: false,
        proxy: None,
        delta_key: Default::default(),
        cookies: Default::default(),
    }
}

/// Longer than the crawl gives a Chrome fetch (twice the timeout, plus
/// the wait for a challenge), so a slow page fails with the crawl's own
/// error, which says what went wrong, rather than here.
const STALLED: Duration = Duration::from_secs(90);

async fn drain(c: &Crawler) -> Vec<Event> {
    let mut all = Vec::new();
    loop {
        let Ok(batch) = tokio::time::timeout(STALLED, c.next(64)).await else {
            // Where the request had got to says which half to look at.
            panic!("crawl stalled after {} events: {:?}", all.len(), c.stats());
        };
        if batch.is_empty() {
            return all;
        }
        all.extend(batch);
    }
}

/// The first event about a request; warnings come and go with the
/// machine (a runner's Chrome may have no matching profile).
fn main(events: &[Event]) -> Option<&Event> {
    events.iter().find(|e| !matches!(e, Event::Warning { .. }))
}

/// Like `drain`, but closes each Chrome page as it arrives, as a spider's
/// callback returning would.
async fn drain_closing(c: &Crawler) -> Vec<(Event, std::time::Instant)> {
    let mut all = Vec::new();
    loop {
        let Ok(batch) = tokio::time::timeout(STALLED, c.next(64)).await else {
            // Where the request had got to says which half to look at.
            panic!("crawl stalled after {} events: {:?}", all.len(), c.stats());
        };
        if batch.is_empty() {
            return all;
        }
        for event in batch {
            if let Event::Fetched {
                page: Some(page), ..
            } = &event
            {
                page.close().await;
            }
            all.push((event, std::time::Instant::now()));
        }
    }
}

const BUILT_BY_SCRIPT: &str = "<p id=static>static</p><script>document.body.insertAdjacentHTML('beforeend', '<p id=made>made by script</p>')</script>";

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_request_comes_back_rendered_with_its_page_open() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![("/", Page::html(BUILT_BY_SCRIPT))]).await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/"), true));
    let events = drain(&c).await;
    let Some(Event::Fetched { response, page, .. }) = main(&events) else {
        panic!("{events:?}");
    };
    assert_eq!(response.status, 200);
    assert!(
        response.text().contains(r#"<p id="made">"#),
        "{}",
        response.text()
    );
    let page = page.as_ref().expect("the live page comes with it");
    assert_eq!(
        page.page()
            .evaluate("document.querySelector('#made').textContent")
            .await
            .unwrap(),
        "made by script"
    );
    page.close().await;
    assert!(page.page().is_closed());
    let stats = c.stats();
    assert_eq!(stats.browser_fetches, 1);
    c.close().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn other_requests_still_use_the_http_client() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![("/", Page::html(BUILT_BY_SCRIPT))]).await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/"), false));
    let events = drain(&c).await;
    let Some(Event::Fetched { response, page, .. }) = main(&events) else {
        panic!("{events:?}");
    };
    assert!(page.is_none());
    assert!(!response.text().contains(r#"<p id="made">"#));
    assert_eq!(c.stats().browser_fetches, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_browser_always_every_request_goes_through_chrome() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![
        ("/a", Page::html(BUILT_BY_SCRIPT)),
        ("/b", Page::html(BUILT_BY_SCRIPT)),
    ])
    .await;
    let c = crawler(settings(BrowserMode::Always));
    c.submit(request(1, site.url("/a"), false));
    c.submit(request(2, site.url("/b"), false));
    let events = drain(&c).await;
    let rendered = events
        .iter()
        .filter(|e| matches!(e, Event::Fetched { response, .. } if response.text().contains(r#"<p id="made">"#)))
        .count();
    assert_eq!(rendered, 2, "{events:?}");
    assert_eq!(c.stats().browser_fetches, 2);
}

/// A challenge that a browser passes: its script sets a cookie and
/// reloads, and with the cookie the site serves the page.
fn challenge() -> Page {
    challenge_moving_on(
        "pass=1",
        "setTimeout(() => { document.cookie = 'pass=1; path=/'; location.reload() }, 200)",
    )
}

/// A challenge whose script `moves_on` once it has set the cookie
/// `pass` (`name=value`), which gets the page instead.
fn challenge_moving_on(pass: &str, moves_on: &str) -> Page {
    Page::status(
        403,
        &format!("<title>Just a moment...</title><script>{moves_on}</script>"),
    )
    .with_header("content-type", "text/html")
    .with_header("cf-mitigated", "challenge")
    .unless_cookie(pass, Page::html("<p id=prize>the page</p>"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_request_gets_through_in_chrome_and_hands_its_cookies_back() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![("/guarded", challenge()), ("/also", challenge())]).await;
    let c = crawler(settings(BrowserMode::OnBlock));
    c.submit(request(1, site.url("/guarded"), false));
    let events = drain(&c).await;
    let Some(Event::Fetched { response, page, .. }) = main(&events) else {
        panic!("{events:?}");
    };
    assert_eq!(response.status, 200);
    assert!(response.text().contains("the page"));
    if let Some(page) = page {
        page.close().await;
    }
    let stats = c.stats();
    assert_eq!((stats.browser_fetches, stats.browser_unblocked), (1, 1));

    // The HTTP client now carries Chrome's cookie and isn't challenged.
    c.submit(request(2, site.url("/also"), false));
    let events = drain(&c).await;
    let Some(Event::Fetched { response, page, .. }) = main(&events) else {
        panic!("{events:?}");
    };
    assert!(page.is_none(), "fetched over HTTP");
    assert_eq!(response.status, 200);
    assert_eq!(c.stats().browser_fetches, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_named_jar_gets_back_only_what_changed_in_chrome() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![("/", Page::html("<p>in</p>"))]).await;
    let c = crawler(settings(BrowserMode::Off));
    let jar = c.jar(Some("ada"));
    let cookie = |name: &str, value: &str| netweir_core::cookies::Cookie {
        name: name.into(),
        value: value.into(),
        domain: "127.0.0.1".into(),
        path: "/".into(),
        expires: -1.0,
        http_only: false,
        secure: false,
        same_site: None,
        partition_key: None,
    };
    for name in ["sid", "kept", "logged_in"] {
        jar.add(&cookie(name, "1"), None).unwrap();
    }
    c.submit(CrawlRequest {
        cookies: netweir_core::RequestCookies {
            jar: Some("ada".into()),
            ..Default::default()
        },
        ..request(1, site.url("/"), true)
    });
    let events = drain(&c).await;
    let Some(Event::Fetched {
        page: Some(page), ..
    }) = main(&events)
    else {
        panic!("{events:?}");
    };
    // In Chrome, a logout and a new cookie; meanwhile, over HTTP, the site
    // gives the session a new id.
    page.page()
        .evaluate(
            "() => { document.cookie = 'logged_in=; max-age=0; path=/'; \
             document.cookie = 'fresh=1; path=/'; }",
        )
        .await
        .unwrap();
    jar.add(&cookie("sid", "2"), None).unwrap();
    page.close().await;
    let mut left: Vec<(String, String)> =
        jar.all().into_iter().map(|c| (c.name, c.value)).collect();
    left.sort();
    let pair = |n: &str, v: &str| (n.to_string(), v.to_string());
    assert_eq!(
        left,
        [pair("fresh", "1"), pair("kept", "1"), pair("sid", "2")]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_request_in_the_crawls_own_jar_takes_it_and_gives_it_back() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![
        ("/", Page::html("<p>in</p>")),
        ("/after", Page::html("after")),
    ])
    .await;
    let c = crawler(settings(BrowserMode::Off));
    let cookie = |name: &str, value: &str| netweir_core::cookies::Cookie {
        name: name.into(),
        value: value.into(),
        domain: String::new(),
        path: String::new(),
        expires: -1.0,
        http_only: false,
        secure: false,
        same_site: None,
        partition_key: None,
    };
    c.jar(None)
        .add(
            &netweir_core::cookies::Cookie {
                domain: "127.0.0.1".into(),
                path: "/".into(),
                ..cookie("crawl", "1")
            },
            None,
        )
        .unwrap();
    c.submit(CrawlRequest {
        cookies: netweir_core::RequestCookies {
            add: vec![cookie("own", "2")],
            ..Default::default()
        },
        ..request(1, site.url("/"), true)
    });
    let events = drain(&c).await;
    let Some(Event::Fetched {
        page: Some(page), ..
    }) = main(&events)
    else {
        panic!("{events:?}");
    };
    page.page()
        .evaluate("() => { document.cookie = 'chrome=3; path=/'; }")
        .await
        .unwrap();
    page.close().await;
    c.submit(request(2, site.url("/after"), false));
    drain(&c).await;
    let sent = |path: &str| {
        site.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, h)| {
                h.iter()
                    .find(|(k, _)| k == "cookie")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(sent("/"), ["crawl=1; own=2"]);
    assert_eq!(sent("/after"), ["crawl=1; own=2; chrome=3"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jars_page_stays_out_of_a_kept_profile() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![
        ("/", Page::html("<p>in</p>")),
        ("/plain", Page::html("plain")),
    ])
    .await;
    let profile = std::env::temp_dir().join(format!("netweir-jar-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    let mut with_profile = settings(BrowserMode::Off);
    with_profile.browser_launch.user_data_dir = Some(profile.clone());
    let c = crawler(with_profile);
    c.submit(CrawlRequest {
        cookies: netweir_core::RequestCookies {
            jar: Some("ada".into()),
            add: vec![netweir_core::cookies::Cookie {
                name: "sid".into(),
                value: "ada".into(),
                domain: String::new(),
                path: String::new(),
                expires: -1.0,
                http_only: false,
                secure: false,
                same_site: None,
                partition_key: None,
            }],
            ..Default::default()
        },
        ..request(1, site.url("/"), true)
    });
    drain_closing(&c).await;
    // A page that leaves the jar out opens in the profile, which never
    // saw Ada's cookie.
    c.submit(CrawlRequest {
        cookies: netweir_core::RequestCookies {
            dont_merge: true,
            ..Default::default()
        },
        ..request(2, site.url("/plain"), true)
    });
    drain_closing(&c).await;
    let cookie = site
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p == "/plain")
        .and_then(|(_, h)| {
            h.iter()
                .find(|(k, _)| k == "cookie")
                .map(|(_, v)| v.clone())
        });
    assert_eq!(cookie, None);
    if let Some(browser) = c.browser().await {
        let _ = browser.close().await;
    }
    let _ = std::fs::remove_dir_all(&profile);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_takes_and_gives_back_only_its_sites_cookies() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![("/", Page::html("<p>in</p>"))]).await;
    let c = crawler(settings(BrowserMode::Off));
    let cookie = |name: String, domain: &str| netweir_core::cookies::Cookie {
        name,
        value: "1".into(),
        domain: domain.into(),
        path: "/".into(),
        expires: -1.0,
        http_only: false,
        secure: false,
        same_site: None,
        partition_key: None,
    };
    let jar = c.jar(None);
    // Thousands of cookies of other sites, which the page has no use for.
    for i in 0..20_000 {
        jar.add(
            &cookie(format!("o{i}"), &format!("site{}.example", i / 100)),
            None,
        )
        .unwrap();
    }
    jar.add(&cookie("own".into(), "127.0.0.1"), None).unwrap();
    c.submit(request(1, site.url("/"), true));
    let events = drain(&c).await;
    let Some(Event::Fetched {
        page: Some(page), ..
    }) = main(&events)
    else {
        panic!("{events:?}");
    };
    let in_chrome = page.page().cookies().await.unwrap();
    let names: Vec<&str> = in_chrome.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["own"]);
    // A cookie Chrome got for another site stays in Chrome.
    page.page()
        .set_cookies(&[cookie("tracker".into(), ".ads.example")])
        .await
        .unwrap();
    let closing = std::time::Instant::now();
    page.close().await;
    assert!(
        closing.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        closing.elapsed()
    );
    assert!(jar.find("tracker", None, None).is_empty());
    assert_eq!(jar.len(), 20_001);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cookie_that_expired_in_chrome_isnt_taken_from_the_jar_that_renewed_it() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![("/", Page::html("<p>in</p>"))]).await;
    let c = crawler(settings(BrowserMode::Off));
    let jar = c.jar(Some("ada"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let sid = |value: &str, expires: f64| netweir_core::cookies::Cookie {
        name: "sid".into(),
        value: value.into(),
        domain: "127.0.0.1".into(),
        path: "/".into(),
        expires,
        http_only: false,
        secure: false,
        same_site: None,
        partition_key: None,
    };
    jar.add(&sid("short", now + 2.0), None).unwrap();
    c.submit(CrawlRequest {
        cookies: netweir_core::RequestCookies {
            jar: Some("ada".into()),
            ..Default::default()
        },
        ..request(1, site.url("/"), true)
    });
    let events = drain(&c).await;
    let Some(Event::Fetched {
        page: Some(page), ..
    }) = main(&events)
    else {
        panic!("{events:?}");
    };
    // Chrome's copy runs out while, over HTTP, the site renews it.
    jar.add(&sid("renewed", now + 3600.0), None).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    page.close().await;
    let left: Vec<String> = jar.all().into_iter().map(|c| c.value).collect();
    assert_eq!(left, ["renewed"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_chrome_handed_back_comes_back_on_resume_with_per_request() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    // per_request needs proxies: one, answering for a site of its own.
    let proxy = Site::start(vec![
        ("http://shop.test/guarded", challenge()),
        ("http://shop.test/also", challenge()),
    ])
    .await;
    let dir = std::env::temp_dir().join(format!("netweir-handed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let saving = || CrawlSettings {
        checkpoint: Some(dir.join("crawl.sqlite3")),
        proxy_rotation: netweir_core::ProxyRotation::PerRequest,
        proxies: vec![format!("http://127.0.0.1:{}", proxy.port)],
        ..settings(BrowserMode::OnBlock)
    };
    let c = crawler(saving());
    c.submit(request(1, "http://shop.test/guarded".into(), false));
    drain_closing(&c).await;
    assert_eq!(c.stats().browser_unblocked, 1);
    c.close().unwrap();
    drop(c);
    // Resumed, the site's session is Chrome's again: no challenge, no
    // Chrome, no warning.
    let c = crawler(saving());
    c.submit(request(2, "http://shop.test/also".into(), false));
    let events = drain_closing(&c).await;
    let events: Vec<Event> = events.into_iter().map(|(e, _)| e).collect();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Warning { message } if message.contains("new visitor"))),
        "{events:?}"
    );
    let Some(Event::Fetched { response, page, .. }) = main(&events) else {
        panic!("{events:?}");
    };
    assert!(page.is_none(), "fetched over HTTP");
    assert_eq!(response.status, 200);
    assert_eq!(c.stats().browser_fetches, 0);
    c.close().unwrap();
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_naming_its_own_proxy_after_a_hand_back_still_looks_like_chrome() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![("/guarded", challenge())]).await;
    let own = Site::start(vec![(
        site.url("/mine").as_str(),
        Page::html("<p>through my proxy</p>"),
    )])
    .await;
    // The crawl looks like Firefox; Chrome's cookies go to a session that
    // looks like the Chrome that earned them.
    let c = Crawler::new(
        FetchOptions::new(Profile::named("firefox").unwrap()),
        settings(BrowserMode::OnBlock),
    )
    .unwrap();
    c.submit(request(1, site.url("/guarded"), false));
    let events = drain(&c).await;
    if events.iter().any(
        |e| matches!(e, Event::Warning { message } if message.contains("no matching netweir profile")),
    ) {
        eprintln!("skipped: no profile for this Chrome");
        return;
    }
    for e in &events {
        if let Event::Fetched {
            page: Some(page), ..
        } = e
        {
            page.close().await;
        }
    }
    c.submit(CrawlRequest {
        proxy: Some(format!("http://127.0.0.1:{}", own.port)),
        ..request(2, site.url("/mine"), false)
    });
    drain(&c).await;
    let requests = own.requests.lock().unwrap().clone();
    let (_, headers) = requests.first().expect("the own proxy carried it");
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert!(header("user-agent").contains("Chrome/"), "{headers:?}");
    assert!(header("cookie").contains("pass=1"), "{headers:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_escalation_a_block_goes_to_on_block() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![("/guarded", challenge())]).await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/guarded"), false));
    let events = drain(&c).await;
    assert!(
        matches!(main(&events), Some(Event::Blocked { .. })),
        "{events:?}"
    );
    assert_eq!(c.stats().browser_fetches, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chrome_that_wont_start_fails_its_requests() {
    let site = Site::start(vec![("/", Page::html("x"))]).await;
    let c = crawler(CrawlSettings {
        browser_launch: LaunchOptions {
            executable: Some("/nonexistent/chrome".into()),
            ..LaunchOptions::default()
        },
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/"), true));
    let events = drain(&c).await;
    let Some(Event::Failed { error, .. }) = main(&events) else {
        panic!("{events:?}");
    };
    assert!(
        error.message.contains("/nonexistent/chrome"),
        "{}",
        error.message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_challenge_that_moves_on_at_once_still_counts_as_passed() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    // Real challenges navigate whenever they like, including before the
    // first page has been read. Each sets its own cookie: with one cookie
    // for both, the first to pass hands it back to the HTTP client, and the
    // second can get through without being blocked at all.
    let site = Site::start(vec![
        (
            "/soon",
            challenge_moving_on(
                "soon=1",
                "setTimeout(() => { document.cookie = 'soon=1; path=/'; location.reload() }, 0)",
            ),
        ),
        (
            "/now",
            challenge_moving_on(
                "now=1",
                "document.cookie = 'now=1; path=/'; location.reload()",
            ),
        ),
        ("/warm", Page::html("warm")),
    ])
    .await;
    let c = crawler(settings(BrowserMode::OnBlock));
    // Chrome starts first, so what's timed is the challenges alone: slow
    // runners take seconds to start it.
    c.submit(request(0, site.url("/warm"), true));
    drain_closing(&c).await;
    c.submit(request(1, site.url("/soon"), false));
    c.submit(request(2, site.url("/now"), false));
    let started = std::time::Instant::now();
    let events = drain_closing(&c).await;
    let passed = events
        .iter()
        .filter(|(e, _)| matches!(e, Event::Fetched { response, .. } if response.status == 200))
        .count();
    assert_eq!(passed, 2, "{events:?}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(c.stats().browser_unblocked, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_request_that_cant_reach_chrome_says_why() {
    let site = Site::start(vec![("/guarded", challenge())]).await;
    let c = crawler(CrawlSettings {
        browser_launch: LaunchOptions {
            executable: Some("/nonexistent/chrome".into()),
            ..LaunchOptions::default()
        },
        ..settings(BrowserMode::OnBlock)
    });
    c.submit(request(1, site.url("/guarded"), false));
    let events = drain(&c).await;
    assert!(
        matches!(main(&events), Some(Event::Blocked { .. })),
        "{events:?}"
    );
    let warned = events.iter().any(
        |e| matches!(e, Event::Warning { message } if message.contains("/nonexistent/chrome")),
    );
    assert!(warned, "{events:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_requests_waiting_for_a_page_dont_hold_up_the_rest() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let slow = Page::html(BUILT_BY_SCRIPT).slow(Duration::from_millis(500));
    let site = Site::start(vec![
        ("/a", slow.clone()),
        ("/b", slow.clone()),
        ("/c", slow.clone()),
    ])
    .await;
    let other = Site::start(vec![("/plain", Page::html("plain"))]).await;
    let c = crawler(CrawlSettings {
        concurrency: 2,
        browser_pages: 1,
        ..settings(BrowserMode::Off)
    });
    for (i, path) in ["/a", "/b", "/c"].iter().enumerate() {
        c.submit(request(i as u64, site.url(path), true));
    }
    // Another host: localhost, where the browser requests go to 127.0.0.1.
    c.submit(request(
        9,
        format!("http://localhost:{}/plain", other.port),
        false,
    ));
    let events = drain_closing(&c).await;
    let order: Vec<u64> = events
        .iter()
        .filter_map(|(e, _)| match e {
            Event::Fetched { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(order.len(), 4, "{events:?}");
    // The plain page needn't wait behind Chrome's queue.
    assert!(order.iter().position(|&id| id == 9) < Some(2), "{order:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_error_in_chrome_is_tried_again() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::scripted(vec![(
        "/flaky",
        vec![Page::status(503, "busy"), Page::html(BUILT_BY_SCRIPT)],
    )])
    .await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/flaky"), true));
    let events = drain_closing(&c).await;
    let statuses: Vec<u16> = events
        .iter()
        .filter_map(|(e, _)| match e {
            Event::Fetched { response, .. } => Some(response.status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, [200], "{events:?}");
    assert_eq!(c.stats().retries, 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_chrome_that_dies_is_started_again() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![
        ("/a", Page::html(BUILT_BY_SCRIPT)),
        ("/b", Page::html(BUILT_BY_SCRIPT)),
    ])
    .await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/a"), true));
    drain_closing(&c).await;
    let first = c.browser().await.expect("Chrome was started");
    unsafe { libc::kill(first.pid().unwrap() as i32, libc::SIGKILL) };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !first.is_closed() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.submit(request(2, site.url("/b"), true));
    let events = drain_closing(&c).await;
    assert!(
        events
            .iter()
            .any(|(e, _)| matches!(e, Event::Fetched { id: 2, .. })),
        "{events:?}"
    );
    assert_ne!(c.browser().await.unwrap().pid(), first.pid());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_chrome_follows_obeys_the_next_sites_robots_txt() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let other = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nDisallow: /secret\n"),
        ),
        ("/secret", Page::html("secret")),
        ("/open", Page::html(BUILT_BY_SCRIPT)),
    ])
    .await;
    // localhost, so it's another host from 127.0.0.1 to the crawl.
    let other_url = |path: &str| format!("http://localhost:{}{path}", other.port);
    let site = Site::start(vec![
        (
            "/to-secret",
            Page::status(302, "").with_header("location", &other_url("/secret")),
        ),
        (
            "/to-open",
            Page::status(302, "").with_header("location", &other_url("/open")),
        ),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: true,
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/to-secret"), true));
    c.submit(request(2, site.url("/to-open"), true));
    let events = drain_closing(&c).await;
    assert!(
        events.iter().any(|(e, _)| matches!(
            e,
            Event::Dropped {
                id: 1,
                reason: netweir_core::DropReason::Robots
            }
        )),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|(e, _)| matches!(e, Event::Fetched { id: 2, .. })),
        "{events:?}"
    );
    assert!(
        !other.paths().iter().any(|p| p == "/secret"),
        "Chrome never asked for the disallowed page"
    );
    assert_eq!(c.stats().dropped_robots, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_503_with_retry_after_is_waited_out_and_tried_again_in_chrome() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::scripted(vec![(
        "/busy",
        vec![
            Page::status(503, "busy").with_header("retry-after", "0"),
            Page::html(BUILT_BY_SCRIPT),
        ],
    )])
    .await;
    let c = crawler(settings(BrowserMode::Off));
    c.submit(request(1, site.url("/busy"), true));
    let events = drain_closing(&c).await;
    let statuses: Vec<u16> = events
        .iter()
        .filter_map(|(e, _)| match e {
            Event::Fetched { response, .. } => Some(response.status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, [200], "{events:?}");
    let stats = c.stats();
    assert_eq!((stats.retries, stats.throttled), (1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_navigation_a_pages_script_starts_obeys_robots_txt_too() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let other = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nDisallow: /secret\n"),
        ),
        ("/secret", Page::html("secret")),
    ])
    .await;
    let secret = format!("http://localhost:{}/secret", other.port);
    let site = Site::start(vec![(
        "/moves",
        Page::html(&format!("<script>location.replace('{secret}')</script>")),
    )])
    .await;
    let c = crawler(CrawlSettings {
        obey_robots: true,
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/moves"), true));
    let events = drain_closing(&c).await;
    assert!(
        events.iter().any(|(e, _)| matches!(
            e,
            Event::Dropped {
                id: 1,
                reason: netweir_core::DropReason::Robots
            }
        )),
        "{events:?}"
    );
    assert!(!other.paths().iter().any(|p| p == "/secret"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_chrome_follows_obeys_the_next_sites_tdmrep() {
    let Some(_chrome) = chrome().await else {
        return;
    };
    let tdm = r#"[{"location": "/reserved", "tdm-reservation": 1}]"#;
    let other = Site::start(vec![
        ("/.well-known/tdmrep.json", Page::status(200, tdm)),
        ("/reserved", Page::html("reserved")),
    ])
    .await;
    let site = Site::start(vec![(
        "/go",
        Page::status(302, "").with_header(
            "location",
            &format!("http://localhost:{}/reserved", other.port),
        ),
    )])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: true,
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/go"), true));
    let events = drain_closing(&c).await;
    assert!(
        events.iter().any(|(e, _)| matches!(
            e,
            Event::Dropped {
                id: 1,
                reason: netweir_core::DropReason::TdmReserved
            }
        )),
        "{events:?}"
    );
    assert!(!other.paths().iter().any(|p| p == "/reserved"));
}

/// A chrome-headless-shell `netweir install chrome --headless-shell` put
/// in place, if there is one.
fn headless_shell() -> Option<std::path::PathBuf> {
    let platform = netweir_browser::cft_platform()?;
    let mut found: Vec<_> = std::fs::read_dir(netweir_browser::netweir_home().join("chrome"))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| {
            e.path()
                .join(netweir_browser::cft_executable(platform, true))
        })
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    let shell = found.pop();
    if shell.is_none() {
        eprintln!("skipped: no chrome-headless-shell installed");
    }
    shell
}

/// The shell preloads what a page's speculation rules name whatever its
/// settings say, past the robots.txt guard, so a crawl that obeys
/// robots.txt won't use it; one that doesn't still can.
#[tokio::test(flavor = "multi_thread")]
async fn a_crawl_obeying_robots_txt_wont_use_chrome_headless_shell() {
    let Some(shell) = headless_shell() else {
        return;
    };
    let Some(_chrome) = chrome().await else {
        return;
    };
    let site = Site::start(vec![("/", Page::html("<title>x</title>"))]).await;
    let launch = LaunchOptions {
        executable: Some(shell),
        ..LaunchOptions::default()
    };
    let c = crawler(CrawlSettings {
        obey_robots: true,
        browser_launch: launch.clone(),
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/"), true));
    let events = drain(&c).await;
    let Some(Event::Failed { error, .. }) = main(&events) else {
        panic!("{events:?}");
    };
    for words in [
        "chrome-headless-shell",
        "netweir install chrome",
        "obey_robots",
    ] {
        assert!(error.message.contains(words), "{}", error.message);
    }
    assert!(!site.paths().iter().any(|p| p == "/"), "fetched anyway");

    let c = crawler(CrawlSettings {
        browser_launch: launch,
        ..settings(BrowserMode::Off)
    });
    c.submit(request(1, site.url("/"), true));
    let events = drain(&c).await;
    assert!(
        matches!(main(&events), Some(Event::Fetched { .. })),
        "{events:?}"
    );
}
