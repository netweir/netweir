//! WARC and HAR files of what a crawl fetched, read back byte by byte.

mod site;

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use netweir_core::{CrawlRequest, CrawlSettings, Crawler, Event, FetchOptions, Profile};
use sha1::{Digest, Sha1};
use site::{Page, Site};

fn settings(dir: &Path) -> CrawlSettings {
    CrawlSettings {
        throttle: false,
        start_delay: Duration::ZERO,
        backoff_base: Duration::from_millis(10),
        backoff_max: Duration::from_millis(20),
        warc: Some(dir.join("crawl.warc.gz")),
        har: Some(dir.join("crawl.har")),
        ..CrawlSettings::default()
    }
}

fn crawler(settings: CrawlSettings) -> Crawler {
    let c = Crawler::new(
        FetchOptions::new(Profile::named("chrome").unwrap()),
        settings,
    )
    .unwrap();
    c.start_archive().unwrap();
    c
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

/// A directory of its own for each test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("netweir-archive-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[derive(Debug)]
struct Record {
    headers: Vec<(String, String)>,
    block: Vec<u8>,
}

impl Record {
    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn kind(&self) -> &str {
        self.get("WARC-Type").unwrap()
    }

    fn uri(&self) -> &str {
        self.get("WARC-Target-URI").unwrap_or("")
    }

    /// The HTTP message's head and body.
    fn http(&self) -> (&[u8], &[u8]) {
        let at = find(&self.block, b"\r\n\r\n").expect("an HTTP head") + 4;
        (&self.block[..at], &self.block[at..])
    }

    fn http_header(&self, name: &str) -> Option<String> {
        let head = String::from_utf8_lossy(self.http().0).into_owned();
        head.lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The records of a plain WARC file, checking each one's framing.
fn records(bytes: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let end = find(rest, b"\r\n\r\n").expect("a WARC head");
        let head = std::str::from_utf8(&rest[..end]).unwrap();
        let mut lines = head.split("\r\n");
        assert_eq!(lines.next(), Some("WARC/1.1"));
        let headers: Vec<(String, String)> = lines
            .map(|l| {
                let (k, v) = l.split_once(": ").expect("a WARC header line");
                (k.to_string(), v.to_string())
            })
            .collect();
        let length: usize = headers
            .iter()
            .find(|(k, _)| k == "Content-Length")
            .unwrap()
            .1
            .parse()
            .unwrap();
        let block = rest[end + 4..end + 4 + length].to_vec();
        assert_eq!(&rest[end + 4 + length..end + 8 + length], b"\r\n\r\n");
        rest = &rest[end + 8 + length..];
        out.push(Record { headers, block });
    }
    out
}

/// Each gzip member of a file, decompressed on its own.
fn members(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut reader = std::io::BufReader::new(bytes);
    let mut out = Vec::new();
    while !reader.fill_buf().unwrap().is_empty() {
        let mut member = Vec::new();
        flate2::bufread::GzDecoder::new(&mut reader)
            .read_to_end(&mut member)
            .unwrap();
        out.push(member);
    }
    out
}

/// The records of a .warc.gz, each of which must be a gzip member of its
/// own.
fn gz_records(path: &Path) -> Vec<Record> {
    let members = members(&std::fs::read(path).unwrap());
    members
        .iter()
        .map(|m| {
            let mut one = records(m);
            assert_eq!(one.len(), 1, "one record per gzip member");
            one.remove(0)
        })
        .collect()
}

fn sha1_b32(bytes: &[u8]) -> String {
    format!(
        "sha1:{}",
        data_encoding::BASE32.encode(&Sha1::digest(bytes))
    )
}

fn check_digests(records: &[Record]) {
    for r in records.iter().filter(|r| r.kind() != "warcinfo") {
        assert_eq!(r.get("WARC-Block-Digest").unwrap(), sha1_b32(&r.block));
        assert_eq!(
            r.get("WARC-Payload-Digest").unwrap(),
            sha1_b32(r.http().1),
            "{}",
            r.uri()
        );
    }
}

fn har(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).expect("a closed HAR file is JSON")
}

#[tokio::test]
async fn every_response_is_recorded_redirect_hops_and_robots_txt_among_them() {
    let dir = temp_dir("hops");
    let site = Site::start(vec![
        (
            "/robots.txt",
            Page::status(200, "User-agent: *\nAllow: /\n"),
        ),
        (
            "/go",
            Page::status(302, "moved")
                .with_header("location", "/target")
                .with_header("set-cookie", "visited=yes; Path=/"),
        ),
        ("/target", Page::html("<p>here</p>")),
    ])
    .await;
    let c = crawler(settings(&dir));
    c.submit(request(1, site.url("/go")));
    drain(&c).await;
    c.close().unwrap();

    let records = gz_records(&dir.join("crawl.warc.gz"));
    assert_eq!(records[0].kind(), "warcinfo");
    let info = String::from_utf8_lossy(&records[0].block).into_owned();
    assert!(info.contains(&format!("software: netweir/{}", env!("CARGO_PKG_VERSION"))));
    check_digests(&records);

    // A request and its response for each, in the order fetched.
    let pairs: Vec<(&str, &str)> = records[1..].iter().map(|r| (r.kind(), r.uri())).collect();
    let robots = site.url("/robots.txt");
    let tdm = site.url("/.well-known/tdmrep.json");
    let (go, target) = (site.url("/go"), site.url("/target"));
    for url in [&robots, &tdm, &go, &target] {
        assert!(pairs.contains(&("request", url)), "{url}");
        assert!(pairs.contains(&("response", url)), "{url}");
    }
    assert_eq!(pairs.len(), 8);
    let pos = |kind: &str, url: &str| pairs.iter().position(|p| *p == (kind, url)).unwrap();
    assert!(pos("response", &go) < pos("request", &target));

    for pair in records[1..].chunks(2) {
        let (req, resp) = (&pair[0], &pair[1]);
        assert_eq!((req.kind(), resp.kind()), ("request", "response"));
        assert_eq!(req.get("WARC-Concurrent-To"), resp.get("WARC-Record-ID"));
        assert_eq!(req.get("WARC-Date"), resp.get("WARC-Date"));
        assert_eq!(resp.get("WARC-Protocol"), Some("http/1.1"));
        assert_eq!(
            resp.get("Content-Type"),
            Some("application/http; msgtype=response")
        );
        assert_eq!(
            req.get("Content-Type"),
            Some("application/http; msgtype=request")
        );
        let id = resp.get("WARC-Record-ID").unwrap();
        assert!(id.starts_with("<urn:uuid:") && id.ends_with('>'), "{id}");
    }
    let hop = &records[pos("response", &go) + 1];
    assert!(hop.http().0.starts_with(b"HTTP/1.1 302 X\r\n"));
    assert_eq!(hop.http_header("location").as_deref(), Some("/target"));
    assert_eq!(hop.http().1, b"moved");
    let page = &records[pos("response", &target) + 1];
    assert_eq!(page.http().1, b"<p>here</p>");
    assert_eq!(page.http_header("content-length").as_deref(), Some("11"));
    let page_request = &records[pos("request", &target) + 1];
    assert_eq!(
        page_request.http_header("cookie").as_deref(),
        Some("visited=yes"),
        "the jar's cookie, as sent"
    );
    let not_found = &records[pos("response", &tdm) + 1];
    assert!(not_found.http().0.starts_with(b"HTTP/1.1 404 X\r\n"));

    // The HAR file has the same responses.
    let har = har(&dir.join("crawl.har"));
    let entries = har["log"]["entries"].as_array().unwrap();
    let urls: Vec<&str> = entries
        .iter()
        .map(|e| e["request"]["url"].as_str().unwrap())
        .collect();
    assert_eq!(urls.len(), 4);
    for url in [&robots, &tdm, &go, &target] {
        assert!(urls.contains(&url.as_str()), "{url}");
    }
    let hop = &entries[urls.iter().position(|u| *u == go).unwrap()];
    assert_eq!(hop["response"]["status"], 302);
    assert_eq!(hop["response"]["redirectURL"], "/target");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_request_record_holds_the_head_the_site_received() {
    let dir = temp_dir("head");
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/a", Page::html("a")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        ..settings(&dir)
    });
    let mut asked = request(1, site.url("/a?x=1"));
    asked.headers = vec![("X-Custom".into(), "Yes".into())];
    c.submit(asked);
    drain(&c).await;
    c.close().unwrap();
    let records = gz_records(&dir.join("crawl.warc.gz"));
    let heads = site.heads.lock().unwrap().clone();
    let sent: Vec<&[u8]> = records
        .iter()
        .filter(|r| r.kind() == "request")
        .map(|r| r.http().0)
        .collect();
    assert_eq!(sent.len(), 2);
    for head in &heads {
        assert!(
            sent.contains(&head.as_slice()),
            "the site got\n{}\nbut the WARC has\n{}",
            String::from_utf8_lossy(head),
            sent.iter()
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A first run with a checkpoint, killed part way through a write, and a
/// second that resumes it.
async fn resumed(dir: &Path, tear: impl FnOnce(&Path, &Path)) {
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/one", Page::html("one")),
        ("/two", Page::html("two")),
    ])
    .await;
    let with_checkpoint = |dir: &Path| CrawlSettings {
        obey_tdmrep: false,
        checkpoint: Some(dir.join("crawl.sqlite3")),
        ..settings(dir)
    };
    let c = crawler(with_checkpoint(dir));
    c.submit(request(1, site.url("/one")));
    drain(&c).await;
    c.close().unwrap();
    drop(c);
    tear(&dir.join("crawl.warc.gz"), &dir.join("crawl.har"));
    let c = crawler(with_checkpoint(dir));
    c.submit(request(2, site.url("/two")));
    drain(&c).await;
    c.close().unwrap();
}

#[tokio::test]
async fn a_resumed_crawl_cuts_off_a_torn_record_and_adds_to_the_files() {
    let dir = temp_dir("resume-gz");
    resumed(&dir, |warc, har| {
        // A record cut off part way: the start of a gzip member.
        let mut whole = Vec::new();
        let mut gz = flate2::write::GzEncoder::new(&mut whole, flate2::Compression::default());
        gz.write_all(b"WARC/1.1\r\nWARC-Type: response\r\nContent-Length: 999\r\n\r\nHTTP")
            .unwrap();
        gz.finish().unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(warc).unwrap();
        f.write_all(&whole[..whole.len() / 2]).unwrap();
        // And the HAR file without its end, half an entry on.
        let text = std::fs::read_to_string(har).unwrap();
        let open = text.strip_suffix("\n]}}\n").unwrap();
        std::fs::write(har, format!("{open},\n{{\"startedDateTime\": \"20")).unwrap();
    })
    .await;
    let records = gz_records(&dir.join("crawl.warc.gz"));
    check_digests(&records);
    let kinds: Vec<&str> = records.iter().map(Record::kind).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "warcinfo").count(), 2);
    let uris: Vec<&str> = records
        .iter()
        .filter(|r| r.kind() == "response")
        .map(Record::uri)
        .collect();
    assert!(uris.iter().any(|u| u.ends_with("/one")));
    assert!(uris.iter().any(|u| u.ends_with("/two")));
    let har = har(&dir.join("crawl.har"));
    let urls: Vec<&str> = har["log"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["request"]["url"].as_str().unwrap())
        .collect();
    assert!(urls.iter().any(|u| u.ends_with("/one")));
    assert!(urls.iter().any(|u| u.ends_with("/two")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_plain_warc_is_cut_back_to_its_last_whole_record() {
    let dir = temp_dir("resume-plain");
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/one", Page::html("one")),
        ("/two", Page::html("two")),
    ])
    .await;
    let plain = |dir: &Path| CrawlSettings {
        obey_tdmrep: false,
        checkpoint: Some(dir.join("crawl.sqlite3")),
        warc: Some(dir.join("crawl.warc")),
        har: None,
        ..settings(dir)
    };
    let c = crawler(plain(&dir));
    c.submit(request(1, site.url("/one")));
    drain(&c).await;
    c.close().unwrap();
    drop(c);
    let path = dir.join("crawl.warc");
    let whole = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"WARC/1.1\r\nWARC-Type: response\r\nContent-Length: 500\r\n\r\nHTTP/1.1 200")
        .unwrap();
    let c = crawler(plain(&dir));
    c.submit(request(2, site.url("/two")));
    drain(&c).await;
    c.close().unwrap();
    let after = std::fs::read(&path).unwrap();
    assert!(
        after.starts_with(&whole),
        "the first run's records are kept"
    );
    let records = records(&after);
    check_digests(&records);
    assert!(
        records
            .iter()
            .any(|r| r.kind() == "response" && r.uri().ends_with("/two"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_crawl_that_never_starts_its_archive_opens_it_when_read() {
    let dir = temp_dir("lazy");
    let paths: Vec<String> = (0..80).map(|i| format!("/p{i}")).collect();
    let mut pages = vec![("/robots.txt", Page::status(404, ""))];
    pages.extend(paths.iter().map(|p| (p.as_str(), Page::html("p"))));
    let site = Site::start(pages).await;
    // More pages than the queue holds, and no start_archive.
    let c = Crawler::new(
        FetchOptions::new(Profile::named("chrome").unwrap()),
        CrawlSettings {
            obey_tdmrep: false,
            ..settings(&dir)
        },
    )
    .unwrap();
    for (i, p) in paths.iter().enumerate() {
        c.submit(request(i as u64, site.url(p)));
    }
    drain(&c).await;
    c.close().unwrap();
    let records = gz_records(&dir.join("crawl.warc.gz"));
    let responses = records.iter().filter(|r| r.kind() == "response").count();
    assert_eq!(responses, 81, "every page and robots.txt");
    assert_eq!(c.stats().archive_errors, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_crawl_dropped_without_closing_still_ends_its_har_file() {
    let dir = temp_dir("dropped");
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/one", Page::html("one")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        ..settings(&dir)
    });
    c.submit(request(1, site.url("/one")));
    drain(&c).await;
    drop(c);
    let entries = har(&dir.join("crawl.har"))["log"]["entries"].clone();
    assert_eq!(entries.as_array().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_long_redirect_body_is_recorded_as_far_as_it_was_read() {
    let dir = temp_dir("long-hop");
    let long = "x".repeat(100_000);
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        (
            "/go",
            Page::status(301, &long).with_header("location", "/there"),
        ),
        ("/there", Page::html("there")),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        ..settings(&dir)
    });
    c.submit(request(1, site.url("/go")));
    drain(&c).await;
    c.close().unwrap();
    let records = gz_records(&dir.join("crawl.warc.gz"));
    check_digests(&records);
    let hop = records
        .iter()
        .find(|r| r.kind() == "response" && r.uri().ends_with("/go"))
        .unwrap();
    assert_eq!(hop.get("WARC-Truncated"), Some("length"));
    assert_eq!(hop.http().1.len(), 64 << 10);
    // The header says what the server sent; the record says it's cut.
    assert_eq!(hop.http_header("content-length").as_deref(), Some("100000"));
    let page = records
        .iter()
        .find(|r| r.kind() == "response" && r.uri().ends_with("/there"))
        .unwrap();
    assert_eq!(page.get("WARC-Truncated"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_crawl_without_a_checkpoint_starts_its_files_afresh() {
    let dir = temp_dir("fresh");
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        ("/one", Page::html("one")),
    ])
    .await;
    for _ in 0..2 {
        let c = crawler(CrawlSettings {
            obey_tdmrep: false,
            ..settings(&dir)
        });
        c.submit(request(1, site.url("/one")));
        drain(&c).await;
        c.close().unwrap();
    }
    let records = gz_records(&dir.join("crawl.warc.gz"));
    assert_eq!(records.len(), 5, "warcinfo and two pairs, from one run");
    let entries = har(&dir.join("crawl.har"))["log"]["entries"].clone();
    assert_eq!(entries.as_array().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_file_that_isnt_an_archive_is_not_added_to() {
    let dir = temp_dir("foreign");
    let site = Site::start(vec![("/robots.txt", Page::status(404, ""))]).await;
    let resuming = |dir: &Path, warc: &str| CrawlSettings {
        checkpoint: Some(dir.join("crawl.sqlite3")),
        warc: Some(dir.join(warc)),
        har: None,
        ..settings(dir)
    };
    // A checkpoint with something in it: the next crawl resumes.
    let c = crawler(resuming(&dir, "first.warc"));
    c.submit(request(1, site.url("/x")));
    drain(&c).await;
    c.close().unwrap();
    drop(c);
    for name in ["notes.warc", "notes.warc.gz"] {
        std::fs::write(dir.join(name), "my notes\n").unwrap();
        let refused = Crawler::new(
            FetchOptions::new(Profile::named("chrome").unwrap()),
            resuming(&dir, name),
        );
        let message = refused.err().expect("refused").message;
        assert!(message.contains(name), "{message}");
        assert_eq!(std::fs::read(dir.join(name)).unwrap(), b"my notes\n");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn har_entries_have_what_a_network_panel_needs() {
    let dir = temp_dir("har");
    let site = Site::start(vec![
        ("/robots.txt", Page::status(404, "")),
        (
            "/a?q=1&r=two",
            Page::html("<p>caf\u{e9}</p>").with_header("x-made", "here"),
        ),
    ])
    .await;
    let c = crawler(CrawlSettings {
        obey_tdmrep: false,
        ..settings(&dir)
    });
    c.submit(request(1, site.url("/a?q=1&r=two")));
    drain(&c).await;
    c.close().unwrap();
    let har = har(&dir.join("crawl.har"));
    let log = &har["log"];
    assert_eq!(log["version"], "1.2");
    assert_eq!(log["creator"]["name"], "netweir");
    assert_eq!(log["creator"]["version"], env!("CARGO_PKG_VERSION"));
    let entry = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["request"]["url"].as_str().unwrap().contains("/a?"))
        .unwrap();
    let started = entry["startedDateTime"].as_str().unwrap();
    assert!(started.ends_with('Z') && started.len() == 24, "{started}");
    let request = &entry["request"];
    assert_eq!(request["method"], "GET");
    assert_eq!(request["httpVersion"], "HTTP/1.1");
    assert_eq!(
        request["queryString"],
        serde_json::json!([{"name": "q", "value": "1"}, {"name": "r", "value": "two"}])
    );
    let names: Vec<&str> = request["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap())
        .collect();
    assert_eq!(names[0], "Host");
    assert!(names.contains(&"User-Agent"), "{names:?}");
    let response = &entry["response"];
    assert_eq!(response["status"], 200);
    assert_eq!(
        response["statusText"], "X",
        "the server's own reason phrase"
    );
    assert_eq!(response["content"]["text"], "<p>caf\u{e9}</p>");
    assert_eq!(response["content"]["size"], 12);
    assert_eq!(response["content"]["mimeType"], "text/html; charset=utf-8");
    assert_eq!(response["redirectURL"], "");
    assert!(
        response["headers"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!({"name": "x-made", "value": "here"}))
    );
    let timings = &entry["timings"];
    let total = timings["wait"].as_f64().unwrap() + timings["receive"].as_f64().unwrap();
    assert!((entry["time"].as_f64().unwrap() - total).abs() < 1e-6);
    assert_eq!(timings["dns"], -1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every kind of request each profile makes, over HTTP/2 and HTTP/1.1:
/// typed navigations with and without cookies and extra headers, links,
/// forms, scripts' requests, and redirects after each.
async fn scenario(
    fetcher: &netweir_core::Fetcher,
    server: &mut netweir_fingerprint::Server,
) -> Vec<netweir_fingerprint::Capture> {
    use netweir_core::Outgoing;
    let port = server.port;
    let base = format!("https://localhost:{port}");
    let page = |p: &str| format!("{base}{p}");
    // The URL, the request, the caller's headers and the hops it takes.
    type Step = (String, Outgoing, Vec<(String, String)>, usize);
    let steps: Vec<Step> = vec![
        (page("/?set=nw"), Outgoing::navigate(), vec![], 1),
        (
            page("/"),
            Outgoing::navigate(),
            vec![
                ("X-Extra".into(), "1".into()),
                ("accept-language".into(), "fr".into()),
            ],
            1,
        ),
        (
            format!("{base}/redirect?to=https://127.0.0.1:{port}/after&set=r"),
            Outgoing::navigate(),
            vec![],
            2,
        ),
        (
            format!("{base}/redirect?to={base}/final&set=s"),
            Outgoing::navigate(),
            vec![],
            2,
        ),
        (
            page("/submitted"),
            Outgoing::form(b"q=rust&page=2".to_vec(), None, Some(page("/form"))),
            vec![],
            1,
        ),
        (
            page("/api"),
            Outgoing::fetch(
                "POST",
                Some(br#"{"q":"rust"}"#.to_vec()),
                Some("application/json".into()),
                Some(page("/fetch")),
            ),
            vec![],
            1,
        ),
        (
            page("/api"),
            Outgoing::fetch("GET", None, None, Some(page("/fetch"))),
            vec![("X-Requested-With".into(), "x".into())],
            1,
        ),
        (
            page("/redirect?to=/after"),
            Outgoing::form(b"user=ada".to_vec(), None, Some(page("/form-redirect"))),
            vec![],
            2,
        ),
        (page("/linked"), Outgoing::link(page("/link")), vec![], 1),
        (
            page("/redirect?to=/after2"),
            Outgoing::link(page("/link")),
            vec![],
            2,
        ),
        (
            page("/redirect?to=/after3"),
            Outgoing::fetch("GET", None, None, Some(page("/fetch"))),
            vec![],
            2,
        ),
    ];
    let mut seen = Vec::new();
    for (url, outgoing, headers, hops) in steps {
        let r = fetcher.send(&url, &outgoing, &headers).await.unwrap();
        assert_eq!(r.status, 200, "{url}");
        for _ in 0..hops {
            seen.push(server.next().await.unwrap());
        }
    }
    seen
}

/// A capture with the server's port taken out, for comparing two runs.
fn comparable(c: &netweir_fingerprint::Capture, port: u16) -> String {
    // Which connection a request went on only over HTTP/2, where one
    // carries them all. Over HTTP/1.1 the client puts a connection back in
    // its pool in the background once the answer is read, and the next
    // request can sometimes get there first and open another: a race in
    // the client, archive or not.
    let (connection, request) = if c.protocol == "h2" {
        (Some(c.connection), Some(c.request))
    } else {
        (None, None)
    };
    serde_json::json!({
        "connection": connection,
        "request": request,
        "protocol": c.protocol,
        "method": c.method,
        "path": c.path,
        "body": c.body,
        "headers": c.headers,
        "http2": c.http2,
    })
    .to_string()
    // Only where it's the port: its digits can turn up in other numbers,
    // such as an HTTP/2 window size.
    .replace(&format!("localhost:{port}"), "localhost:PORT")
    .replace(&format!("127.0.0.1:{port}"), "127.0.0.1:PORT")
}

#[tokio::test(flavor = "multi_thread")]
async fn archiving_changes_nothing_sent_and_records_exactly_what_was() {
    let dir = temp_dir("scenario");
    // Every profile netweir ships: one per profiles/*.toml.
    let profiles = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let shipped: Vec<String> = std::fs::read_dir(profiles)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".toml").map(str::to_string)
        })
        .collect();
    assert!(!shipped.is_empty());
    for profile in shipped.iter().map(String::as_str) {
        for http1 in [false, true] {
            let mut runs = Vec::new();
            for archived in [false, true] {
                let mut server = if http1 {
                    netweir_fingerprint::Server::start_http1(0).await.unwrap()
                } else {
                    netweir_fingerprint::Server::start().await.unwrap()
                };
                let mut options = FetchOptions::new(Profile::named(profile).unwrap());
                options.verify_certificates = false;
                let path = dir.join(format!("{profile}-{http1}.warc"));
                if archived {
                    let archive = netweir_core::Archive::new(Some(&path), None, false).unwrap();
                    archive.start().unwrap();
                    options.archive = Some(archive);
                }
                let archive = options.archive.clone();
                let fetcher = netweir_core::Fetcher::new(options).unwrap();
                let seen = scenario(&fetcher, &mut server).await;
                let port = server.port;
                runs.push(seen.iter().map(|c| comparable(c, port)).collect::<Vec<_>>());
                let Some(archive) = archive else { continue };
                archive.close().unwrap();
                let records = records(&std::fs::read(&path).unwrap());
                check_digests(&records);
                let requests: Vec<&Record> =
                    records.iter().filter(|r| r.kind() == "request").collect();
                assert_eq!(requests.len(), seen.len(), "{profile}");
                for (record, capture) in requests.iter().zip(&seen) {
                    let what = format!("{profile} {} {}", capture.protocol, capture.path);
                    let protocol = if http1 { "http/1.1" } else { "h2" };
                    assert_eq!(capture.protocol, protocol);
                    assert_eq!(record.get("WARC-Protocol"), Some(protocol), "{what}");
                    let (head, body) = record.http();
                    let head = String::from_utf8_lossy(head).into_owned();
                    let mut lines = head.trim_end().split("\r\n");
                    assert_eq!(
                        lines.next(),
                        Some(format!("{} {} HTTP/1.1", capture.method, capture.path).as_str()),
                        "{what}"
                    );
                    let recorded: Vec<(String, String)> = lines
                        .map(|l| {
                            let (k, v) = l.split_once(':').unwrap();
                            (k.to_string(), v.strip_prefix(' ').unwrap_or(v).to_string())
                        })
                        .collect();
                    let sent: Vec<(String, String)> = capture
                        .headers
                        .iter()
                        .filter(|(k, _)| !k.starts_with(':'))
                        .cloned()
                        .collect();
                    assert_eq!(recorded, sent, "{what}");
                    assert_eq!(body, capture.body.as_bytes(), "{what}");
                }
            }
            assert_eq!(
                runs[0], runs[1],
                "{profile}, http1 {http1}: the same on the wire"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A login is the crawl's own, like a proxy's: the site gets it, the
/// archive, which gets shared, doesn't.
#[tokio::test]
async fn a_login_is_sent_but_left_out_of_the_archive() {
    let dir = temp_dir("login");
    let mut server = netweir_fingerprint::Server::start().await.unwrap();
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.verify_certificates = false;
    let (warc, har_path) = (dir.join("login.warc"), dir.join("login.har"));
    let archive = netweir_core::Archive::new(Some(&warc), Some(&har_path), false).unwrap();
    archive.start().unwrap();
    options.archive = Some(archive.clone());
    let fetcher = netweir_core::Fetcher::new(options).unwrap();
    let mut outgoing = netweir_core::Outgoing::navigate();
    let password = format!("pw-{}", std::process::id());
    outgoing.policy.auth = Some(netweir_core::Auth::new("ada", password, None));
    let url = format!("https://localhost:{}/", server.port);
    fetcher.send(&url, &outgoing, &[]).await.unwrap();
    let sent = server.next().await.unwrap();
    assert!(sent.headers.iter().any(|(k, _)| k == "authorization"));
    archive.close().unwrap();
    let records = records(&std::fs::read(&warc).unwrap());
    let request = records.iter().find(|r| r.kind() == "request").unwrap();
    let head = String::from_utf8_lossy(request.http().0).to_ascii_lowercase();
    assert!(head.contains("user-agent:"), "{head}");
    assert!(!head.contains("authorization"), "{head}");
    // The server echoes the request as its answer, so only the request's
    // own headers are looked at.
    let har = har(&har_path);
    let names: Vec<String> = har["log"]["entries"][0]["request"]["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    assert!(names.contains(&"user-agent".to_string()), "{names:?}");
    assert!(!names.contains(&"authorization".to_string()), "{names:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A login written into a redirect's Location goes neither to the site
/// nor into the archive, the URL it records included.
#[tokio::test]
async fn a_login_in_a_redirects_location_stays_out_of_the_archive() {
    let dir = temp_dir("location-login");
    let mut server = netweir_fingerprint::Server::start().await.unwrap();
    let port = server.port;
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.verify_certificates = false;
    let (warc, har_path) = (dir.join("login.warc"), dir.join("login.har"));
    let archive = netweir_core::Archive::new(Some(&warc), Some(&har_path), false).unwrap();
    archive.start().unwrap();
    options.archive = Some(archive.clone());
    let fetcher = netweir_core::Fetcher::new(options).unwrap();
    let password = format!("pw-{}", std::process::id());
    let url = format!(
        "https://localhost:{port}/redirect?to=https://bob:{password}@localhost:{port}/after"
    );
    let page = fetcher
        .send(&url, &netweir_core::Outgoing::navigate(), &[])
        .await
        .unwrap();
    assert!(!page.url.contains(&password), "{}", page.url);
    for _ in 0..2 {
        let sent = server.next().await.unwrap();
        assert!(!sent.headers.iter().any(|(k, _)| k == "authorization"));
    }
    archive.close().unwrap();
    let records = records(&std::fs::read(&warc).unwrap());
    // Only the hop after the redirect: the first URL holds the login in
    // its query, as the test wrote it.
    let after = format!("https://localhost:{port}/after");
    let targets: Vec<_> = records
        .iter()
        .filter(|r| r.kind() == "request")
        .map(|r| r.uri().to_string())
        .collect();
    assert_eq!(targets.len(), 2, "{targets:?}");
    assert_eq!(targets[1], after);
    let entries = &har(&har_path)["log"]["entries"];
    assert_eq!(entries[1]["request"]["url"].as_str(), Some(after.as_str()));
    let _ = std::fs::remove_dir_all(&dir);
}
