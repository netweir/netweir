//! Dialogs, popups and downloads: what a page does that a driven browser
//! has to answer, follow or catch. See docs/design/dialogs.md.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use common::{Reply, browser, html, serve};
use netweir_browser::{Dialog, Download, Error, Page, PageEvent, WaitUntil};
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;

/// The next event `pick` takes, skipping the rest.
async fn next<T>(
    events: &mut UnboundedReceiver<PageEvent>,
    mut pick: impl FnMut(PageEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let event = events.recv().await.expect("the page went away");
            if let Some(found) = pick(event) {
                return found;
            }
        }
    })
    .await
    .expect("nothing came in 20 seconds")
}

async fn dialog(dialogs: &mut UnboundedReceiver<Dialog>) -> Dialog {
    tokio::time::timeout(Duration::from_secs(20), dialogs.recv())
        .await
        .expect("no dialog came in 20 seconds")
        .expect("the page went away")
}

async fn popup(events: &mut UnboundedReceiver<PageEvent>) -> Page {
    next(events, |e| match e {
        PageEvent::Popup(p) => Some(p),
        _ => None,
    })
    .await
}

async fn download(events: &mut UnboundedReceiver<PageEvent>) -> Download {
    next(events, |e| match e {
        PageEvent::Download(d) => Some(d),
        _ => None,
    })
    .await
}

const ASKS: &str = "<title>asks</title><p>asks</p>";

#[tokio::test]
async fn a_dialog_nobody_answers_is_dismissed_as_a_person_would() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(ASKS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    for (js, dismissed) in [
        ("confirm('sure?')", json!(false)),
        ("prompt('name?', 'ann')", json!(null)),
        ("alert('hi') === undefined", json!(true)),
    ] {
        let started = Instant::now();
        let answer = tokio::time::timeout(Duration::from_secs(10), page.evaluate(js))
            .await
            .unwrap_or_else(|_| panic!("{js} left the page waiting"))
            .unwrap();
        assert_eq!(answer, dismissed, "{js}");
        // Never sooner than a person could read it and press a key.
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(500),
            "{js} answered in {took:?}"
        );
        assert!(took < Duration::from_secs(5), "{js} answered in {took:?}");
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_listener_answers_a_dialog_and_reads_what_it_says() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(ASKS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.dialogs();

    let asking = tokio::spawn({
        let page = page.clone();
        async move { page.evaluate("prompt('name?', 'ann')").await }
    });
    let d = dialog(&mut events).await;
    assert_eq!(
        (d.kind(), d.message(), d.default_value()),
        ("prompt", "name?", "ann")
    );
    assert!(d.url().starts_with(&server.url), "{}", d.url());
    d.accept(Some("bob")).await.unwrap();
    assert!(d.accept(None).await.is_err(), "a dialog is answered once");
    assert_eq!(asking.await.unwrap().unwrap(), json!("bob"));

    let asking = tokio::spawn({
        let page = page.clone();
        async move { page.evaluate("confirm('sure?')").await }
    });
    let d = dialog(&mut events).await;
    assert_eq!((d.kind(), d.message()), ("confirm", "sure?"));
    d.accept(None).await.unwrap();
    assert_eq!(asking.await.unwrap().unwrap(), json!(true));

    // An answer whose future is dropped unpolled still reaches the page:
    // the dialog counts as answered, and nothing else would answer it.
    let asking = tokio::spawn({
        let page = page.clone();
        async move { page.evaluate("prompt('again?')").await }
    });
    let d = dialog(&mut events).await;
    drop(d.accept(Some("dropped")));
    drop(d);
    let answer = tokio::time::timeout(Duration::from_secs(10), asking)
        .await
        .expect("the page was left waiting")
        .unwrap()
        .unwrap();
    assert_eq!(answer, json!("dropped"));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn the_pause_before_an_answer_is_the_browsers_to_set() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        dialog_pause: (Duration::from_millis(50), Duration::from_millis(60)),
        ..Default::default()
    })
    .await;
    let server = serve(vec![("/", html(ASKS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let started = Instant::now();
    assert_eq!(
        page.evaluate("confirm('quick?')").await.unwrap(),
        json!(false)
    );
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(50) && took < Duration::from_millis(450),
        "answered in {took:?}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_dialog_from_a_frame_on_another_site_is_heard_and_answered() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<script>document.write(`<iframe src="http://localhost:${location.port}/inner"></iframe>`)</script>"#,
            ),
        ),
        ("/inner", html("<p>inner</p>")),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let frame = page
        .frames()
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.url().starts_with("http://localhost:"))
        .expect("the frame from another site");
    let mut events = page.dialogs();
    let asking = tokio::spawn(async move { frame.evaluate("confirm('from the frame')").await });
    let d = dialog(&mut events).await;
    assert_eq!(d.message(), "from the frame");
    assert!(d.url().starts_with("http://localhost:"), "{}", d.url());
    d.accept(None).await.unwrap();
    assert_eq!(asking.await.unwrap().unwrap(), json!(true));
    browser.close().await.unwrap();
}

const LEAVING: &str = r#"<title>leaving</title><p id=p>stay a while</p>
<script>addEventListener('beforeunload', e => { e.preventDefault(); e.returnValue = ''; })</script>"#;

#[tokio::test]
async fn a_page_that_asks_before_it_unloads_is_left_and_closed() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(LEAVING)), ("/next", html("<p>next</p>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    // Chrome only asks once a person has done something on the page.
    page.click("#p", None).await.unwrap();
    let r = tokio::time::timeout(
        Duration::from_secs(15),
        page.goto(&format!("{}/next", server.url), WaitUntil::Load, None),
    )
    .await
    .expect("leaving waited on the page's question")
    .unwrap();
    assert_eq!(r.status, 200, "nobody answered, and the page was left");

    // A listener hears the question, and leaves.
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#p", None).await.unwrap();
    let mut dialogs = page.dialogs();
    let heard = tokio::spawn(async move {
        let d = dialog(&mut dialogs).await;
        d.accept(None).await.unwrap();
        d.kind().to_string()
    });
    page.goto(&format!("{}/next", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(heard.await.unwrap(), "beforeunload");

    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#p", None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), page.close())
        .await
        .expect("closing waited on the page's question")
        .unwrap();
    browser.close().await.unwrap();
}

/// A page a popup opens: an image, and a call to an API, both of which
/// the opener's rules reach.
const POPPED: &str = r#"<title>popped</title><p id=p>popped up</p>
<img src="/pop.png">
<script>fetch('/api').then(r => r.text()).then(t => document.querySelector('#p').dataset.api = t)</script>"#;

#[tokio::test]
async fn a_popup_from_a_link_is_read_and_carries_its_openers_rules() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<a id=a target=_blank href=/popped>open</a>
<button id=b onclick="window.open('/popped')">open</button>
<script>
  window.seen = [];
  addEventListener('visibilitychange', () => seen.push(document.visibilityState));
</script>"#,
            ),
        ),
        ("/popped", html(POPPED)),
        ("/api", html("from the server")),
    ]);
    let page = browser.new_page().await.unwrap();
    page.block(&["image"], &[]).await.unwrap();
    let mut routes = page.route().await.unwrap();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            if route.request().url.ends_with("/api") {
                route.fulfill(200, &[], b"routed").await.unwrap();
            }
        }
    });
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    for opener in ["#a", "#b"] {
        page.click(opener, None).await.unwrap();
        let popped = popup(&mut events).await;
        let r = popped.wait_for_load(WaitUntil::Load, None).await.unwrap();
        assert_eq!(r.status, 200, "{opener}: the document's own response");
        assert_eq!(popped.url(), format!("{}/popped", server.url), "{opener}");
        assert!(
            popped.content().await.unwrap().contains("popped up"),
            "{opener}"
        );
        let api = popped
            .wait_for_function_value("document.querySelector('#p').dataset.api")
            .await;
        assert_eq!(api, json!("routed"), "{opener}: the opener's route");
        assert_eq!(
            popped
                .evaluate("navigator.userAgent.includes('Headless')")
                .await
                .unwrap(),
            json!(false),
            "{opener}"
        );
        // Left open: the opener opens the next one all the same.
    }
    // Each popup hid its opener, as a tab opened in front does, and the
    // second click brought the opener back first, as a person would.
    assert_eq!(
        page.evaluate("seen.slice(0, 2)").await.unwrap(),
        json!(["hidden", "visible"])
    );
    let seen: Vec<String> = server
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    assert!(
        !seen.iter().any(|p| p == "/pop.png"),
        "the opener's block: {seen:?}"
    );
    assert!(
        !seen.iter().any(|p| p == "/api"),
        "the opener's route: {seen:?}"
    );
    let headers = server.headers_for("/popped");
    let agent = headers.iter().find(|(k, _)| k == "user-agent").unwrap();
    assert!(!agent.1.contains("Headless"), "{agent:?}");
    browser.close().await.unwrap();
}

/// `evaluate` until `js` is something other than null, for at most ten
/// seconds.
trait Until {
    async fn wait_for_function_value(&self, js: &str) -> serde_json::Value;
}

impl Until for Page {
    async fn wait_for_function_value(&self, js: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let v = self.evaluate(js).await.unwrap();
            if !v.is_null() || Instant::now() > deadline {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// How long the opener's timers stopped while a same-site popup it
/// opened was set up: the longest gap between ticks of a 5 ms interval
/// from just before the click until the popup has loaded.
async fn longest_stall(rules: bool) -> Duration {
    let browser = browser().await.expect("Chrome");
    let server = serve(vec![
        (
            "/",
            html(
                r#"<button id=b onclick="window.open('/popped')">open</button>
<script>
  window.gaps = [];
  let last = performance.now();
  setInterval(() => { const now = performance.now(); gaps.push(now - last); last = now; }, 5);
</script>"#,
            ),
        ),
        ("/popped", html("<p>popped up</p>")),
    ]);
    let page = browser.new_page().await.unwrap();
    if rules {
        page.block(&["font"], &[]).await.unwrap();
    }
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    tokio::time::sleep(Duration::from_millis(300)).await;
    page.evaluate("gaps.length = 0").await.unwrap();
    page.click("#b", None).await.unwrap();
    let popped = popup(&mut events).await;
    popped.wait_for_load(WaitUntil::Load, None).await.unwrap();
    let longest = page
        .evaluate("Math.max(...gaps)")
        .await
        .unwrap()
        .as_f64()
        .unwrap();
    browser.close().await.unwrap();
    Duration::from_secs_f64(longest / 1000.0)
}

#[tokio::test]
async fn opening_a_popup_barely_stops_the_openers_timers() {
    if common::chrome().is_none() {
        return;
    }
    for rules in [false, true] {
        let mut stalls: Vec<Duration> = Vec::new();
        for _ in 0..3 {
            stalls.push(longest_stall(rules).await);
        }
        stalls.sort();
        eprintln!("rules {rules}: longest gaps {stalls:?}");
        // A tab opening in front, as a person's does, already costs the
        // opener a frame or so; holding the popup mustn't add much.
        assert!(
            stalls[1] < Duration::from_millis(250),
            "rules {rules}: {stalls:?}"
        );
    }
}

#[tokio::test]
async fn a_popup_of_a_shared_context_is_routed_by_the_context() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        ("/", html("<a id=a target=_blank href=/popped>open</a>")),
        ("/popped", html(POPPED)),
    ]);
    let context = browser.new_context().await.unwrap();
    let mut routes = context.route().await.unwrap();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            if route.request().url.ends_with("/popped") {
                route
                    .fulfill(200, &[], b"<p id=p>from the context</p>")
                    .await
                    .unwrap();
            }
        }
    });
    let page = context.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    page.click("#a", None).await.unwrap();
    let popped = popup(&mut events).await;
    popped.wait_for_load(WaitUntil::Load, None).await.unwrap();
    assert!(popped.content().await.unwrap().contains("from the context"));
    context.close().await.unwrap();
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_popup_from_a_frame_on_another_site_is_its_pages() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<script>document.write(`<iframe src="http://localhost:${location.port}/inner"></iframe>`)</script>"#,
            ),
        ),
        (
            "/inner",
            html(r#"<a id=a target=_blank href="/popped">open</a>"#),
        ),
        ("/popped", html(POPPED)),
        ("/api", html("from the server")),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let frame = page
        .frames()
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.url().starts_with("http://localhost:"))
        .expect("the frame from another site");
    let mut events = page.listen();
    frame.click("#a", None).await.unwrap();
    let popped = popup(&mut events).await;
    popped.wait_for_load(WaitUntil::Load, None).await.unwrap();
    assert!(
        popped.url().starts_with("http://localhost:"),
        "{}",
        popped.url()
    );
    assert!(popped.content().await.unwrap().contains("popped up"));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_blank_popup_a_script_writes_into_is_read() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<button id=b onclick="window.open('').document.write('<p>written</p>')">open</button>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    page.click("#b", None).await.unwrap();
    let popped = popup(&mut events).await;
    popped.wait_for_load(WaitUntil::Load, None).await.unwrap();
    assert!(popped.content().await.unwrap().contains("written"));
    browser.close().await.unwrap();
}

const FILE: &str = "name,price\ntea,3\n";

fn attachment(name: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![
            ("Content-Type", "text/csv".into()),
            (
                "Content-Disposition",
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        body: FILE.into(),
    }
}

#[tokio::test]
async fn a_download_is_saved_in_its_contexts_own_folder() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                "<a id=dl href=/prices.csv>prices</a><a id=new target=_blank href=/prices.csv>in a new tab</a>",
            ),
        ),
        ("/prices.csv", attachment("prices.csv")),
    ]);
    let mut files = Vec::new();
    let mut contexts = Vec::new();
    for _ in 0..2 {
        let context = browser.new_context().await.unwrap();
        let page = context.new_page().await.unwrap();
        page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
        let mut events = page.listen();
        page.click("#dl", None).await.unwrap();
        let d = download(&mut events).await;
        assert_eq!(d.suggested_filename(), "prices.csv");
        assert_eq!(d.url(), format!("{}/prices.csv", server.url));
        assert_eq!(d.failure(None).await.unwrap(), None);
        let path = d.path(None).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), FILE.as_bytes());
        files.push(path);
        contexts.push(context);
    }
    assert_ne!(
        files[0].parent(),
        files[1].parent(),
        "each context downloads into a folder of its own"
    );
    contexts[0].close().await.unwrap();
    assert!(!files[0].exists(), "the folder goes with its context");
    assert!(files[1].exists());

    // A link to a file that opens a new tab: Chrome closes the tab again,
    // and the download is the opener's.
    let page = contexts[1].new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    page.click("#new", None).await.unwrap();
    let d = download(&mut events).await;
    assert_eq!(
        std::fs::read(d.path(None).await.unwrap()).unwrap(),
        FILE.as_bytes()
    );
    browser.close().await.unwrap();
    assert!(!files[1].exists(), "everything goes with the browser");
}

/// A server of downloads, to see what Chrome does with them.
struct Stalling {
    url: String,
    /// Lets each `/slow` go.
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// How many `/slow` downloads Chrome hung up on.
    hung_up: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// Serves `/slow` as a download that sends ten bytes of a million and
/// then waits, `/broken` as one that sends ten and hangs up, and
/// `/whole` as ten bytes that are all of it.
fn stalling_server() -> Stalling {
    use std::io::Read;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let done = Arc::new(AtomicBool::new(false));
    let hung_up = Arc::new(AtomicUsize::new(0));
    let (stop, hangups) = (done.clone(), hung_up.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (stop, hangups) = (stop.clone(), hangups.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                }
                let length = if path == "/whole" { 10 } else { 1_000_000 };
                let reply = match path.as_str() {
                    "/" => {
                        let body = "<a id=slow href=/slow>slow</a><a id=broken href=/broken>broken</a>\
                                    <a id=whole href=/whole>whole</a>";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    _ => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                         Content-Disposition: attachment; filename=\"big.bin\"\r\n\
                         Content-Length: {length}\r\nConnection: close\r\n\r\n0123456789"
                    ),
                };
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
                if path == "/slow" {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(50)))
                        .unwrap();
                    while !stop.load(Ordering::SeqCst) {
                        match stream.read(&mut [0; 64]) {
                            Ok(0) => {
                                hangups.fetch_add(1, Ordering::SeqCst);
                                return;
                            }
                            Err(e)
                                if e.kind() != std::io::ErrorKind::WouldBlock
                                    && e.kind() != std::io::ErrorKind::TimedOut =>
                            {
                                hangups.fetch_add(1, Ordering::SeqCst);
                                return;
                            }
                            _ => {}
                        }
                    }
                }
            });
        }
    });
    Stalling { url, done, hung_up }
}

#[tokio::test]
async fn a_cancelled_or_broken_download_says_it_didnt_finish() {
    let Some(browser) = browser().await else {
        return;
    };
    let Stalling { url, done, .. } = stalling_server();
    let page = browser.new_page().await.unwrap();
    page.goto(&url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();

    page.click("#slow", None).await.unwrap();
    let d = download(&mut events).await;
    d.cancel().await.unwrap();
    assert_eq!(d.failure(None).await.unwrap().as_deref(), Some("canceled"));
    assert!(d.path(None).await.is_err());

    page.click("#broken", None).await.unwrap();
    let d = download(&mut events).await;
    assert!(
        d.failure(None).await.unwrap().is_some(),
        "a broken download failed"
    );
    assert!(d.path(None).await.is_err());
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn choosing_on_an_opener_behind_its_popup_brings_it_back_first() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<a id=a target=_blank href=/popped>open</a>
<select id=s><option>tea<option>coffee</select>
<input id=f type=file>
<script>
  window.seen = [];
  for (const id of ['s', 'f']) {
    document.getElementById(id).addEventListener('change', () => seen.push(id + ' ' + document.visibilityState));
  }
</script>"#,
            ),
        ),
        ("/popped", html(POPPED)),
    ]);
    let file = std::env::temp_dir().join(format!("netweir-chosen-{}.txt", std::process::id()));
    std::fs::write(&file, "chosen").unwrap();
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    page.click("#a", None).await.unwrap();
    popup(&mut events).await;
    let coffee = netweir_browser::Pick::Labels(vec!["coffee".into()]);
    page.select_option("#s", &coffee, None).await.unwrap();
    page.click("#a", None).await.unwrap();
    popup(&mut events).await;
    page.set_input_files("#f", std::slice::from_ref(&file), None)
        .await
        .unwrap();
    std::fs::remove_file(&file).unwrap();
    assert_eq!(
        page.evaluate("seen").await.unwrap(),
        json!(["s visible", "f visible"]),
        "a person switches back to the tab before choosing"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_popup_of_a_guarded_page_is_guarded_from_its_first_request() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<a id=a target=_blank href=/refused>open</a>
<button id=b onclick="window.open('/refused')">open</button>"#,
            ),
        ),
        ("/refused", html("<p>refused</p>")),
    ]);
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = asked.clone();
    let guard: netweir_browser::Guard = std::sync::Arc::new(move |url: String| {
        log.lock().unwrap().push(url.clone());
        Box::pin(async move { !url.ends_with("/refused") })
    });
    let page = browser.new_guarded_page(guard).await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    for (n, opener) in ["#a", "#b"].into_iter().enumerate() {
        page.click(opener, None).await.unwrap();
        popup(&mut events).await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while asked
            .lock()
            .unwrap()
            .iter()
            .filter(|u| u.ends_with("/refused"))
            .count()
            <= n
        {
            assert!(
                Instant::now() < deadline,
                "{opener}: the guard was never asked about the popup's document: {:?}",
                asked.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let seen: Vec<String> = server
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    assert!(
        !seen.iter().any(|p| p == "/refused"),
        "a popup fetched what the guard refuses: {seen:?}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_popup_on_another_site_keeps_its_openers_route() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<button id=b onclick="window.open(`http://localhost:${location.port}/popped`)">open</button>"#,
            ),
        ),
        ("/popped", html(POPPED)),
        ("/api", html("from the server")),
    ]);
    let page = browser.new_page().await.unwrap();
    page.block(&["image"], &[]).await.unwrap();
    let mut routes = page.route().await.unwrap();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            if route.request().url.ends_with("/api") {
                route.fulfill(200, &[], b"routed").await.unwrap();
            }
        }
    });
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    assert!(server.url.contains("127.0.0.1"), "{}", server.url);
    let mut events = page.listen();
    page.click("#b", None).await.unwrap();
    let popped = popup(&mut events).await;
    popped.wait_for_load(WaitUntil::Load, None).await.unwrap();
    assert!(
        popped.url().starts_with("http://localhost:"),
        "{}",
        popped.url()
    );
    let api = popped
        .wait_for_function_value("document.querySelector('#p').dataset.api")
        .await;
    assert_eq!(api, json!("routed"), "the opener's route");
    let seen: Vec<String> = server
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    assert!(
        !seen.iter().any(|p| p == "/api" || p == "/pop.png"),
        "{seen:?}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn waiting_on_a_stalled_download_gives_up_at_the_timeout() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .await;
    let Stalling { url, done, .. } = stalling_server();
    let page = browser.new_page().await.unwrap();
    page.goto(&url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    page.click("#slow", None).await.unwrap();
    let d = download(&mut events).await;

    // The browser's timeout, as for any other wait.
    let started = Instant::now();
    let waited = tokio::time::timeout(Duration::from_secs(10), d.path(None))
        .await
        .expect("waited on a stalled download past the browser's timeout");
    assert!(matches!(waited, Err(Error::Timeout(_))), "{waited:?}");
    assert!(started.elapsed() >= Duration::from_secs(2));
    // Or one of its own.
    let started = Instant::now();
    let failure = d.failure(Some(Duration::from_millis(300))).await;
    assert!(matches!(failure, Err(Error::Timeout(_))), "{failure:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    // Giving up on it leaves it going, for cancel() to stop.
    d.cancel().await.unwrap();
    assert_eq!(d.failure(None).await.unwrap().as_deref(), Some("canceled"));
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    browser.close().await.unwrap();
}

/// Waits up to ten seconds for Chrome to have hung up on `n` downloads.
async fn hung_up_on(server: &Stalling, n: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.hung_up.load(std::sync::atomic::Ordering::SeqCst) < n {
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    true
}

#[tokio::test]
async fn a_download_nobody_holds_is_cancelled() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = stalling_server();
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    page.click("#whole", None).await.unwrap();
    let whole = download(&mut events).await;
    let folder = whole
        .path(None)
        .await
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    drop(events);

    // Nobody listening: nothing would ever read it.
    page.click("#slow", None).await.unwrap();
    assert!(
        hung_up_on(&server, 1).await,
        "a download nobody heard went on"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(&folder).unwrap().count() > 1 {
        assert!(Instant::now() < deadline, "its file stayed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Held, it goes on; let go unfinished, it's cancelled.
    let mut events = page.listen();
    page.click("#slow", None).await.unwrap();
    let held = download(&mut events).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(server.hung_up.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(held);
    assert!(hung_up_on(&server, 2).await, "a download let go went on");

    // One that's finished is kept, held or not.
    let path = whole.path(None).await.unwrap();
    drop(whole);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(path.exists());
    server.done.store(true, std::sync::atomic::Ordering::SeqCst);
    browser.close().await.unwrap();
}
