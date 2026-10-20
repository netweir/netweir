//! deltafetch: a page that gave items is skipped by later crawls.

mod site;

use std::path::PathBuf;
use std::time::Duration;

use netweir_core::deltafetch::key;
use netweir_core::{
    CrawlRequest, CrawlSettings, Crawler, DeltaKey, Event, FetchOptions, Profile, Submitted,
};
use site::{Page, Site};

/// A fresh directory, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("netweir-delta-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Dir(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
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
        deltafetch: Some(dir.file("deltafetch.sqlite3")),
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

fn fetched(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::Fetched { .. }))
        .count()
}

fn get(url: &str) -> [u8; 16] {
    key("GET", url, None, None).unwrap()
}

async fn catalogue() -> Site {
    Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/list", Page::html("list")),
        ("/p/1", Page::html("one")),
        ("/p/2", Page::html("two")),
    ])
    .await
}

#[tokio::test]
async fn a_page_that_gave_items_is_skipped_by_the_next_crawl() {
    let site = catalogue().await;
    let dir = Dir::new("skip");
    let c = crawler(settings(&dir));
    for (i, p) in ["/list", "/p/1", "/p/2"].iter().enumerate() {
        assert_eq!(c.submit(request(i as u64, site.url(p))), Submitted::Queued);
    }
    assert_eq!(fetched(&drain(&c).await), 3);
    // The products gave items; the listing gave only links.
    c.produced(&[get(&site.url("/p/1")), get(&site.url("/p/2"))], &[])
        .unwrap();
    c.close().unwrap();
    drop(c);
    let before = site.paths().len();

    let c = crawler(settings(&dir));
    assert_eq!(c.submit(request(0, site.url("/list"))), Submitted::Queued);
    assert_eq!(c.submit(request(1, site.url("/p/1"))), Submitted::Delta);
    // Written another way, it's the same page.
    assert_eq!(
        c.submit(request(2, site.url("/p/2?utm_source=x"))),
        Submitted::Delta
    );
    // A second link to a skipped page is a duplicate: it was skipped once.
    assert_eq!(c.submit(request(3, site.url("/p/1"))), Submitted::Duplicate);
    assert_eq!(fetched(&drain(&c).await), 1);
    let stats = c.stats();
    assert_eq!((stats.skipped_delta, stats.duplicates), (2, 1));
    let asked: Vec<_> = site.paths()[before..]
        .iter()
        .filter(|p| !p.starts_with("/robots.txt") && !p.starts_with("/.well-known"))
        .cloned()
        .collect();
    assert_eq!(asked, ["/list"]);
}

#[tokio::test]
async fn dont_filter_fetches_a_recorded_page() {
    let site = catalogue().await;
    let dir = Dir::new("dont-filter");
    let c = crawler(settings(&dir));
    c.produced(&[get(&site.url("/p/1"))], &[]).unwrap();
    drop(c);

    let c = crawler(settings(&dir));
    let forced = CrawlRequest {
        dont_filter: true,
        ..request(1, site.url("/p/1"))
    };
    assert_eq!(c.submit(forced), Submitted::Queued);
    assert_eq!(fetched(&drain(&c).await), 1);
    assert_eq!(c.stats().skipped_delta, 0);
}

#[tokio::test]
async fn a_custom_key_stands_for_the_request() {
    let site = catalogue().await;
    let dir = Dir::new("custom");
    let c = crawler(settings(&dir));
    c.produced(&[key("GET", "", None, Some(b"sku-1")).unwrap()], &[])
        .unwrap();
    drop(c);

    let c = crawler(settings(&dir));
    let keyed = |id, path: &str, k: &str| CrawlRequest {
        delta_key: DeltaKey::Custom(k.as_bytes().to_vec()),
        ..request(id, site.url(path))
    };
    // Any URL with the key is skipped; the key's own URL isn't recorded.
    assert_eq!(c.submit(keyed(1, "/p/2", "sku-1")), Submitted::Delta);
    assert_eq!(c.submit(keyed(2, "/p/1", "sku-2")), Submitted::Queued);
    assert_eq!(fetched(&drain(&c).await), 1);
}

#[tokio::test]
async fn keys_recorded_in_a_crawl_count_in_that_crawl() {
    let site = catalogue().await;
    let dir = Dir::new("same-run");
    let c = crawler(settings(&dir));
    c.produced(&[key("GET", "", None, Some(b"sku-1")).unwrap()], &[])
        .unwrap();
    let keyed = CrawlRequest {
        delta_key: DeltaKey::Custom(b"sku-1".to_vec()),
        ..request(1, site.url("/p/1"))
    };
    assert_eq!(c.submit(keyed), Submitted::Delta);
}

#[tokio::test]
async fn reset_empties_the_record() {
    let site = catalogue().await;
    let dir = Dir::new("reset");
    let c = crawler(settings(&dir));
    c.produced(&[get(&site.url("/p/1"))], &[]).unwrap();
    drop(c);

    let c = crawler(CrawlSettings {
        deltafetch_reset: true,
        ..settings(&dir)
    });
    assert_eq!(c.submit(request(1, site.url("/p/1"))), Submitted::Queued);
    drain(&c).await;
    drop(c);
    // Nothing was recorded since, so the page is fetched again.
    let c = crawler(settings(&dir));
    assert_eq!(c.submit(request(1, site.url("/p/1"))), Submitted::Queued);
}

#[tokio::test]
async fn without_a_record_nothing_is_skipped() {
    let site = catalogue().await;
    let c = crawler(CrawlSettings {
        throttle: false,
        start_delay: Duration::ZERO,
        ..CrawlSettings::default()
    });
    c.produced(&[get(&site.url("/p/1"))], &[]).unwrap();
    assert_eq!(c.submit(request(1, site.url("/p/1"))), Submitted::Queued);
}

#[tokio::test]
async fn a_request_read_back_from_the_checkpoint_is_not_skipped() {
    let site = catalogue().await;
    let dir = Dir::new("checkpoint");
    let with_checkpoint = || CrawlSettings {
        checkpoint: Some(dir.file("crawl.sqlite3")),
        // Nothing is sent, so the request stays pending in the checkpoint.
        max_pages: Some(0),
        ..settings(&dir)
    };
    let c = crawler(with_checkpoint());
    assert_eq!(c.submit(request(1, site.url("/p/1"))), Submitted::Queued);
    // Its page gave an item that got into the record but, as far as the
    // checkpoint knows, never into the output.
    c.produced(&[get(&site.url("/p/1"))], &[]).unwrap();
    c.close().unwrap();
    drop(c);

    let c = crawler(with_checkpoint());
    let saved = c.saved().unwrap();
    assert_eq!(saved.pending.len(), 1);
    let row = saved.pending[0].row;
    assert_eq!(
        c.resubmit(request(1, site.url("/p/1")), row),
        Submitted::Queued
    );
    // A new request for it is skipped as usual.
    assert_eq!(c.submit(request(2, site.url("/p/1"))), Submitted::Duplicate);
    assert_eq!(c.stats().skipped_delta, 0);
}

#[tokio::test]
async fn a_record_that_cannot_be_opened_is_an_error() {
    let dir = Dir::new("bad");
    std::fs::create_dir_all(&dir.0).unwrap();
    std::fs::write(
        dir.file("deltafetch.sqlite3"),
        b"not a database at all, honestly",
    )
    .unwrap();
    let err = Crawler::new(
        FetchOptions::new(Profile::named("chrome").unwrap()),
        settings(&dir),
    )
    .err()
    .unwrap();
    assert!(err.message.contains("deltafetch record"), "{}", err.message);
}

#[tokio::test]
async fn a_request_with_deltafetch_off_is_never_skipped() {
    let site = catalogue().await;
    let dir = Dir::new("off");
    let c = crawler(settings(&dir));
    c.produced(&[get(&site.url("/p/1"))], &[]).unwrap();
    let off = CrawlRequest {
        delta_key: DeltaKey::Off,
        ..request(1, site.url("/p/1"))
    };
    assert_eq!(c.submit(off), Submitted::Queued);
    assert_eq!(c.stats().skipped_delta, 0);
}

#[tokio::test]
async fn output_lengths_are_kept_for_the_next_crawl() {
    let dir = Dir::new("lengths");
    let c = crawler(settings(&dir));
    assert_eq!(c.delta_files(), Some(Default::default()));
    assert_eq!(c.delta_was_empty(), Some(true));
    c.produced(
        &[get("https://example.com/")],
        &[("/out/items.jsonl".into(), 120, Some("end".into()), true)],
    )
    .unwrap();
    c.close().unwrap();
    drop(c);
    let c = crawler(settings(&dir));
    let files = c.delta_files().unwrap();
    assert_eq!(
        files.get("/out/items.jsonl"),
        Some(&(120, Some("end".to_string()), true))
    );
    assert_eq!(c.delta_was_empty(), Some(false));
    drop(c);
    let c = crawler(CrawlSettings {
        deltafetch_reset: true,
        ..settings(&dir)
    });
    assert_eq!(c.delta_files(), Some(Default::default()));
}
