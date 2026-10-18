//! A crawl's cookies under the caller's control: a request's own cookies,
//! named jars, requests that leave the jar out, and jars kept in the
//! checkpoint.

mod site;

use std::time::Duration;

use netweir_core::cookies::Cookie;
use netweir_core::{
    CrawlRequest, CrawlSettings, Crawler, Event, FetchOptions, Profile, RequestCookies,
};
use site::{Page, Site};

fn settings() -> CrawlSettings {
    CrawlSettings {
        throttle: false,
        start_delay: Duration::ZERO,
        backoff_base: Duration::from_millis(10),
        backoff_max: Duration::from_millis(20),
        obey_robots: false,
        obey_tdmrep: false,
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
        dont_filter: true,
        depth: 0,
        browser: false,
        outgoing: netweir_core::Outgoing::navigate(),
        retry_post: false,
        proxy: None,
        delta_key: Default::default(),
        cookies: RequestCookies::default(),
    }
}

fn in_jar(id: u64, url: String, jar: &str) -> CrawlRequest {
    let mut r = request(id, url);
    r.cookies.jar = Some(jar.to_string());
    r
}

fn cookie(name: &str, value: &str) -> Cookie {
    Cookie {
        name: name.into(),
        value: value.into(),
        domain: String::new(),
        path: String::new(),
        expires: -1.0,
        http_only: false,
        secure: false,
        same_site: None,
        partition_key: None,
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

/// Fetches each request in turn, so cookies one sets are there for the
/// next.
async fn one_by_one(c: &Crawler, requests: Vec<CrawlRequest>) {
    for r in requests {
        c.submit(r);
        drain(c).await;
    }
}

/// The Cookie header each request for `path` carried, in order.
fn carried(site: &Site, path: &str) -> Vec<String> {
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
        .collect()
}

fn site_pages() -> Vec<(&'static str, Page)> {
    vec![
        (
            "/login/ada",
            Page::html("hi ada").with_header("set-cookie", "user=ada; Path=/"),
        ),
        (
            "/login/bob",
            Page::html("hi bob").with_header("set-cookie", "user=bob; Path=/"),
        ),
        (
            "/tracked",
            Page::html("t").with_header("set-cookie", "tracker=1; Path=/"),
        ),
        ("/me", Page::html("me")),
    ]
}

#[tokio::test]
async fn a_requests_cookies_join_its_jar_and_go_with_later_requests() {
    let site = Site::start(site_pages()).await;
    let c = crawler(settings());
    let mut first = request(1, site.url("/me"));
    first.cookies.add = vec![cookie("currency", "EUR")];
    one_by_one(&c, vec![first, request(2, site.url("/me"))]).await;
    assert_eq!(carried(&site, "/me"), ["currency=EUR", "currency=EUR"]);
}

#[tokio::test]
async fn named_jars_are_separate_visitors_on_one_site() {
    let site = Site::start(site_pages()).await;
    let c = crawler(settings());
    one_by_one(
        &c,
        vec![
            in_jar(1, site.url("/login/ada"), "ada"),
            in_jar(2, site.url("/login/bob"), "bob"),
            in_jar(3, site.url("/me"), "ada"),
            in_jar(4, site.url("/me"), "bob"),
            request(5, site.url("/me")),
        ],
    )
    .await;
    assert_eq!(carried(&site, "/me"), ["user=ada", "user=bob", ""]);
    assert_eq!(c.jar(Some("ada")).all()[0].value, "ada");
    assert!(c.jar(None).is_empty());
}

#[tokio::test]
async fn leaving_the_jar_out_sends_only_the_requests_own_and_keeps_nothing() {
    let site = Site::start(site_pages()).await;
    let c = crawler(settings());
    let mut alone = request(2, site.url("/tracked"));
    alone.cookies.dont_merge = true;
    alone.cookies.add = vec![cookie("only", "1")];
    one_by_one(
        &c,
        vec![
            request(1, site.url("/login/ada")),
            alone,
            request(3, site.url("/me")),
        ],
    )
    .await;
    assert_eq!(carried(&site, "/tracked"), ["only=1"]);
    assert_eq!(carried(&site, "/me"), ["user=ada"]);
}

#[tokio::test]
async fn a_named_jar_outlives_a_block() {
    let site = Site::scripted(vec![
        (
            "/login",
            vec![Page::html("in").with_header("set-cookie", "user=ada; Path=/")],
        ),
        (
            "/guarded",
            vec![
                Page::status(
                    403,
                    "<title>Attention Required! | Cloudflare</title>\
                     <h1>Sorry, you have been blocked</h1>",
                )
                .with_header("server", "cloudflare"),
                Page::html("welcome"),
            ],
        ),
    ])
    .await;
    let c = crawler(settings());
    one_by_one(
        &c,
        vec![
            in_jar(1, site.url("/login"), "ada"),
            in_jar(2, site.url("/guarded"), "ada"),
        ],
    )
    .await;
    assert_eq!(carried(&site, "/guarded"), ["user=ada", "user=ada"]);
    assert_eq!(c.stats().sessions_replaced, 1);
}

#[tokio::test]
async fn jars_are_kept_in_the_checkpoint_and_back_on_resume() {
    let site = Site::start(site_pages()).await;
    let dir = std::env::temp_dir().join(format!("netweir-jars-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("crawl.sqlite3");
    let with_checkpoint = || CrawlSettings {
        checkpoint: Some(path.clone()),
        ..settings()
    };
    let c = crawler(with_checkpoint());
    one_by_one(
        &c,
        vec![
            in_jar(1, site.url("/login/ada"), "ada"),
            request(2, site.url("/login/bob")),
        ],
    )
    .await;
    c.settle(Vec::new(), "{}".into());
    c.close().unwrap();
    drop(c);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the checkpoint holds cookies");
    }
    let c = crawler(with_checkpoint());
    one_by_one(
        &c,
        vec![
            in_jar(3, site.url("/me"), "ada"),
            request(4, site.url("/me")),
        ],
    )
    .await;
    assert_eq!(carried(&site, "/me"), ["user=ada", "user=bob"]);
    c.close().unwrap();
    drop(c);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn on_the_wire_samesite_holds_and_one_client_sends_for_any_jar() {
    use netweir_core::{Fetcher, Outgoing};
    let site = Site::start(site_pages()).await;
    let fetcher = Fetcher::new(FetchOptions::new(Profile::named("chrome").unwrap())).unwrap();
    let me = site.url("/me");
    fetcher.add_set_cookies(
        &me,
        &Outgoing::navigate(),
        &["strict=1; SameSite=Strict".into(), "lax=1".into()],
    );
    // Typed: everything. A link from another site: no Strict cookie.
    fetcher.get(&me, &[]).await.unwrap();
    let from_elsewhere = Outgoing::link("https://elsewhere.example/".into());
    fetcher.send(&me, &from_elsewhere, &[]).await.unwrap();
    // Another visitor through the same client, and the first again.
    let other = fetcher.with_jar(std::sync::Arc::new(netweir_core::cookies::Jar::new()));
    other.get(&me, &[]).await.unwrap();
    fetcher.get(&me, &[]).await.unwrap();
    assert_eq!(
        carried(&site, "/me"),
        ["strict=1; lax=1", "lax=1", "", "strict=1; lax=1"]
    );
}

#[tokio::test]
async fn a_requests_own_cookies_stay_with_its_host_through_a_redirect() {
    let other = Site::start(vec![("/land", Page::html("landed"))]).await;
    let away = format!("http://localhost:{}/land", other.port);
    let site = Site::start(vec![
        (
            "/away",
            Page::status(302, "").with_header("location", &away),
        ),
        (
            "/home",
            Page::status(302, "").with_header("location", "/me"),
        ),
        ("/me", Page::html("me")),
    ])
    .await;
    let c = crawler(settings());
    let mut merged = request(1, site.url("/away"));
    merged.cookies.add = vec![cookie("sid", "secret")];
    let mut alone = request(2, site.url("/away"));
    alone.cookies.dont_merge = true;
    alone.cookies.add = vec![cookie("only", "1")];
    let mut home = request(3, site.url("/home"));
    home.cookies.add = vec![cookie("more", "2")];
    let mut home_alone = request(4, site.url("/home"));
    home_alone.cookies.dont_merge = true;
    home_alone.cookies.add = vec![cookie("solo", "3")];
    one_by_one(&c, vec![merged, alone, home, home_alone]).await;
    assert_eq!(carried(&site, "/away"), ["sid=secret", "only=1"]);
    // Another host never sees them, and the jar keeps them for theirs.
    assert_eq!(carried(&other, "/land"), ["", ""]);
    let kept: Vec<(String, String)> = c
        .jar(None)
        .all()
        .into_iter()
        .map(|c| (c.name, c.domain))
        .collect();
    let ours = |name: &str| (name.to_string(), "127.0.0.1".to_string());
    assert_eq!(kept, [ours("sid"), ours("more")]);
    // A redirect on the same host still carries them.
    assert_eq!(carried(&site, "/me"), ["sid=secret; more=2", "solo=3"]);
}
