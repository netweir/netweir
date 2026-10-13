//! What a page fetches: listening to its requests and responses, reading
//! their bodies, and blocking, changing and answering requests.

mod common;

use std::time::Duration;

use common::{Reply, browser, html, serve};
use netweir_browser::{Continue, Error, NetworkEvent, Response, WaitUntil};
use serde_json::json;

const SHOP: &str = r#"<!DOCTYPE html><img src="/cat.png">
<button id=more onclick="fetch('/api/items?page=2').then(r => r.json()).then(d => {
  document.body.insertAdjacentHTML('beforeend', '<p id=got>' + d.items.join(',') + '</p>')
})">more</button>"#;

fn json_reply(body: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("Content-Type", "application/json".into())],
        body: body.to_string(),
    }
}

fn png() -> Reply {
    Reply {
        status: 200,
        headers: vec![("Content-Type", "image/png".into())],
        body: "not really a png".into(),
    }
}

fn shop() -> common::Server {
    serve(vec![
        ("/", html(SHOP)),
        ("/other", html("<p>other</p>")),
        ("/cat.png", png()),
        (
            "/api/items?page=2",
            json_reply(r#"{"items": ["tea", "cake"]}"#),
        ),
    ])
}

/// The next response whose URL ends with `tail`.
async fn response_to(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<NetworkEvent>,
    tail: &str,
) -> Response {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Some(NetworkEvent::Response(r)) if r.url.ends_with(tail) => return r,
                Some(_) => {}
                None => panic!("the page stopped telling before {tail}"),
            }
        }
    })
    .await
    .expect("no response came")
}

#[tokio::test]
async fn a_listener_hears_subresources_and_reads_an_xhrs_json() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = shop();
    let page = browser.new_page().await.unwrap();
    let mut events = page.listen();
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let image = response_to(&mut events, "/cat.png").await;
    assert_eq!((image.status, image.resource_type.as_str()), (200, "image"));

    page.click("#more", None).await.unwrap();
    let api = response_to(&mut events, "/api/items?page=2").await;
    assert_eq!(
        (api.method.as_str(), api.resource_type.as_str()),
        ("GET", "fetch")
    );
    let body: serde_json::Value = serde_json::from_slice(&page.body(&api).await.unwrap()).unwrap();
    assert_eq!(body, json!({"items": ["tea", "cake"]}));

    // Chrome lets go of bodies when the page moves on.
    page.goto(&format!("{}/other", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let gone = page.body(&api).await.unwrap_err();
    assert!(
        matches!(&gone, Error::Gone(m) if m.contains("/api/items")),
        "{gone}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn blocked_types_and_urls_never_leave_chrome() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = shop();
    let page = browser.new_page().await.unwrap();
    page.block(&["image"], &["**/api/*".to_string()])
        .await
        .unwrap();
    assert!(page.block(&["pictures"], &[]).await.is_err());
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    page.click("#more", None).await.unwrap();
    // The fetch fails, so the page never shows what it got; the document
    // itself still loaded.
    let failed = page
        .evaluate("fetch('/api/items?page=2').then(() => 'loaded', () => 'failed')")
        .await
        .unwrap();
    assert_eq!(failed, json!("failed"));
    let seen: Vec<String> = server
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    assert!(seen.contains(&"/".to_string()), "{seen:?}");
    assert!(
        !seen
            .iter()
            .any(|p| p == "/cat.png" || p.starts_with("/api/")),
        "{seen:?}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn routes_abort_change_and_answer_requests() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = shop();
    let page = browser.new_page().await.unwrap();
    let mut routes = page.route().await.unwrap();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            let url = route.request().url.clone();
            let outcome = if url.ends_with("/cat.png") {
                route.abort().await
            } else if url.contains("/api/") {
                route
                    .fulfill(
                        200,
                        &[("Content-Type".into(), "application/json".into())],
                        br#"{"items": ["made", "up"]}"#,
                    )
                    .await
            } else {
                let mut headers = route.request().headers.clone();
                headers.push(("X-Routed".into(), "yes".into()));
                route
                    .continue_(Continue {
                        headers: Some(headers),
                        ..Continue::default()
                    })
                    .await
            };
            outcome.unwrap();
        }
    });
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    page.click("#more", None).await.unwrap();
    page.wait_for("#got", "attached", None).await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#got').textContent")
            .await
            .unwrap(),
        json!("made,up")
    );
    assert!(
        server
            .headers_for("/")
            .iter()
            .any(|(k, v)| k == "x-routed" && v == "yes"),
        "{:?}",
        server.headers_for("/")
    );
    let seen = server.seen.lock().unwrap().clone();
    assert!(
        !seen
            .iter()
            .any(|(p, _)| p == "/cat.png" || p.starts_with("/api/")),
        "{seen:?}"
    );

    // Without routes, requests go straight through again.
    page.unroute().await.unwrap();
    page.goto(&format!("{}/other", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert!(
        server
            .headers_for("/other")
            .iter()
            .all(|(k, _)| k != "x-routed")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_route_nobody_decides_lets_the_request_go_on() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        timeout: Duration::from_secs(3),
        ..Default::default()
    })
    .await;
    let server = shop();
    let page = browser.new_page().await.unwrap();
    let mut routes = page.route().await.unwrap();
    let held = tokio::spawn(async move {
        // The first is dropped undecided; the rest are held and never
        // decided, until the browser's timeout lets them go.
        let first = routes.recv().await;
        drop(first);
        let mut held = Vec::new();
        while let Some(route) = routes.recv().await {
            held.push(route);
        }
        held.len()
    });
    let r = page
        .goto(
            &format!("{}/", server.url),
            WaitUntil::Load,
            Some(Duration::from_secs(30)),
        )
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    page.wait_for("img", "attached", None).await.unwrap();
    let loaded = page
        .evaluate(
            "new Promise(r => { const i = document.querySelector('img'); \
             i.complete ? r(true) : (i.onload = i.onerror = () => r(true)) })",
        )
        .await
        .unwrap();
    assert_eq!(loaded, json!(true));
    assert!(
        server
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| p == "/cat.png")
    );
    page.unroute().await.unwrap();
    assert!(held.await.unwrap() >= 1);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_contexts_routes_reach_every_page_in_it() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = shop();
    let context = browser.new_context().await.unwrap();
    let before = context.new_page().await.unwrap();
    let mut routes = context.route().await.unwrap();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            route
                .fulfill(
                    200,
                    &[("Content-Type".into(), "text/html".into())],
                    b"<title>from the route</title>",
                )
                .await
                .unwrap();
        }
    });
    let after = context.new_page().await.unwrap();
    for page in [&before, &after] {
        page.goto(&format!("{}/other", server.url), WaitUntil::Load, None)
            .await
            .unwrap();
        assert_eq!(page.title().await.unwrap(), "from the route");
    }
    let seen = server.seen.lock().unwrap().clone();
    // Chrome may open a connection ahead of time, which the server logs
    // with no request; none of the pages reached it.
    assert!(seen.iter().all(|(_, h)| h.is_empty()), "{seen:?}");
    context.unroute().await.unwrap();
    after
        .goto(&format!("{}/other", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter().filter(|(p, _)| p == "/other").count(),
        1,
        "{seen:?}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_document_the_guard_refuses_stays_refused_whatever_a_route_says() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/fine", html("<title>fine</title>"))]);
    let guard: netweir_browser::Guard =
        std::sync::Arc::new(|url: String| Box::pin(async move { !url.ends_with("/secret") }));
    let page = browser.new_guarded_page(guard).await.unwrap();
    let mut routes = page.route().await.unwrap();
    let answered = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = answered.clone();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            log.lock().unwrap().push(route.request().url.clone());
            route
                .fulfill(200, &[], b"<title>from the route</title>")
                .await
                .unwrap();
        }
    });
    let err = page
        .goto(&format!("{}/secret", server.url), WaitUntil::Load, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Navigation { error: m, .. } if m.contains("BLOCKED_BY_CLIENT")),
        "{err}"
    );
    // What the guard allows, the route still gets.
    page.goto(&format!("{}/fine", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(page.title().await.unwrap(), "from the route");
    let answered = answered.lock().unwrap().clone();
    assert!(
        answered.contains(&format!("{}/fine", server.url))
            && !answered.iter().any(|u| u.ends_with("/secret")),
        "{answered:?}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_proxys_login_still_works_with_routes_on() {
    let proxy = common::proxy();
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        proxy: Some(proxy.url.replace("http://", "http://user:secret@")),
        timeout: Duration::from_secs(30),
        ..Default::default()
    })
    .await;
    let page = browser.new_page().await.unwrap();
    let mut routes = page.route().await.unwrap();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            route.continue_(Continue::default()).await.unwrap();
        }
    });
    let r = page
        .goto("http://shop.test/item", WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(
        page.evaluate("document.querySelector('#via').textContent")
            .await
            .unwrap(),
        json!("via proxy: http://shop.test/item")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn listening_and_routing_send_nothing_a_page_can_see() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = shop();
    let page = browser.new_page().await.unwrap();
    let mut events = page.listen();
    page.block(&["font"], &[]).await.unwrap();
    let mut routes = page.route().await.unwrap();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            route.continue_(Continue::default()).await.unwrap();
        }
    });
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    page.click("#more", None).await.unwrap();
    let api = response_to(&mut events, "/api/items?page=2").await;
    page.body(&api).await.unwrap();
    page.unroute().await.unwrap();
    let sent = browser.methods_sent();
    for forbidden in ["Runtime.enable", "Console.enable", "Log.enable"] {
        assert!(!sent.iter().any(|m| m == forbidden), "{sent:?}");
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_route_cant_send_a_guarded_document_where_the_guard_refuses() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        ("/fine", html("<title>fine</title>")),
        ("/secret", html("<title>secret</title>")),
    ]);
    let guard: netweir_browser::Guard =
        std::sync::Arc::new(|url: String| Box::pin(async move { !url.ends_with("/secret") }));
    let page = browser.new_guarded_page(guard).await.unwrap();
    let mut routes = page.route().await.unwrap();
    let secret = format!("{}/secret", server.url);
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            let url = route
                .request()
                .url
                .ends_with("/fine")
                .then(|| secret.clone());
            route
                .continue_(Continue {
                    url,
                    ..Continue::default()
                })
                .await
                .unwrap();
        }
    });
    let err = page
        .goto(&format!("{}/fine", server.url), WaitUntil::Load, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Navigation { error: m, .. } if m.contains("BLOCKED_BY_CLIENT")),
        "{err}"
    );
    let seen = server.seen.lock().unwrap().clone();
    assert!(!seen.iter().any(|(p, _)| p == "/secret"), "{seen:?}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn closing_a_page_ends_its_routes() {
    let Some(browser) = browser().await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    let mut routes = page.route().await.unwrap();
    page.close().await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), routes.recv()).await;
    assert!(
        matches!(ended, Ok(None)),
        "the routes' receiver still waits"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_decided_route_leaves_no_timer_behind() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = shop();
    let page = browser.new_page().await.unwrap();
    let mut routes = page.route().await.unwrap();
    let (decided, mut count) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            route.continue_(Continue::default()).await.unwrap();
            let _ = decided.send(());
        }
    });
    let metrics = tokio::runtime::Handle::current().metrics();
    let before = metrics.num_alive_tasks();
    page.goto(&format!("{}/other", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    count.recv().await.unwrap();
    // The browser's timeout is 30 seconds; a timer that outlived its
    // decision would still be counted long after this.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while metrics.num_alive_tasks() > before && std::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        metrics.num_alive_tasks() <= before,
        "{} tasks, {before} before",
        metrics.num_alive_tasks()
    );
    browser.close().await.unwrap();
}

fn script(body: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("Content-Type", "application/javascript".into())],
        body: body.to_string(),
    }
}

/// A frame's page: an image, and an API its tests fetch from the frame.
const CHILD: &str = r#"<!DOCTYPE html><title>child</title>
<img src="/secret.png" onload="window.pic = 'done'" onerror="window.pic = 'done'">"#;

/// Pages whose frames come from another site, which Chrome runs in
/// processes of their own: 127.0.0.1 and localhost are different sites.
fn framed() -> common::Server {
    serve(vec![
        (
            "/framed",
            html(
                r#"<!DOCTYPE html><script>
  document.write(`<iframe name=child src="http://localhost:${location.port}/child"></iframe>`);
</script>"#,
            ),
        ),
        (
            "/nested",
            html(
                r#"<!DOCTYPE html><script>
  document.write(`<iframe name=middle src="http://localhost:${location.port}/middle"></iframe>`);
</script>"#,
            ),
        ),
        (
            "/middle",
            html(
                r#"<!DOCTYPE html><script>
  document.write(`<iframe name=inner src="http://127.0.0.1:${location.port}/child"></iframe>`);
</script>"#,
            ),
        ),
        (
            "/moving",
            html(r#"<!DOCTYPE html><iframe name=child src="/child"></iframe>"#),
        ),
        ("/child", html(CHILD)),
        ("/secret.png", png()),
        ("/secret-api", json_reply(r#"{"word": "secret"}"#)),
        ("/api", json_reply(r#"{"word": "tea"}"#)),
        (
            "/worker",
            html(
                r#"<!DOCTYPE html><script>
  window.said = new Promise(r => { new Worker('/worker.js').onmessage = e => r(e.data) });
</script>"#,
            ),
        ),
        (
            "/worker.js",
            script(
                "fetch('/secret-api').then(() => postMessage('loaded'), () => postMessage('failed'))",
            ),
        ),
        // A worker that starts a worker, and passes on what it says.
        (
            "/outer.js",
            script("new Worker('/worker.js').onmessage = e => postMessage(e.data)"),
        ),
        (
            "/guarded",
            html(
                r#"<!DOCTYPE html><script>
  document.write(`<iframe name=child src="http://localhost:${location.port}/leaving"></iframe>`);
</script>"#,
            ),
        ),
        // A frame with a link that takes the whole page somewhere else.
        (
            "/leaving",
            html(
                r#"<!DOCTYPE html><script>
  window.pic = true;
  document.write(`<a id=out target=_top href="http://127.0.0.1:${location.port}/refused">out</a>`);
</script>"#,
            ),
        ),
        ("/refused", html("<title>refused</title>")),
        (
            "/audio.js",
            script(
                "registerProcessor('quiet', class extends AudioWorkletProcessor { process() { return true } })",
            ),
        ),
        (
            "/paint.js",
            script("registerPaint('plain', class { paint() {} })"),
        ),
    ])
}

/// The frame named `name` once it shows `path` from a host starting
/// `host`, with its image done loading or failing.
async fn frame_at(
    page: &netweir_browser::Page,
    name: &str,
    host: &str,
    path: &str,
) -> netweir_browser::Frame {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let frame = loop {
        let frames = page.frames().await.unwrap();
        if let Some(f) = frames
            .into_iter()
            .find(|f| f.name() == name && f.url().starts_with(host) && f.url().ends_with(path))
        {
            break f;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the frame never got to {host}...{path}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let done = frame
        .evaluate(
            "new Promise(r => { const t = () => window.pic ? r(true) : setTimeout(t, 20); t() })",
        )
        .await
        .unwrap();
    assert_eq!(done, json!(true));
    frame
}

/// What a fetch of `path` from `frame` came to: "loaded" or "failed".
async fn fetch_from(frame: &netweir_browser::Frame, path: &str) -> serde_json::Value {
    frame
        .evaluate(&format!(
            "fetch('{path}').then(() => 'loaded', () => 'failed')"
        ))
        .await
        .unwrap()
}

fn times_seen(server: &common::Server, path: &str) -> usize {
    server
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == path)
        .count()
}

/// Aborts what's under /secret, lets the rest go, and keeps each URL.
fn abort_secrets(
    mut routes: tokio::sync::mpsc::UnboundedReceiver<netweir_browser::Route>,
) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    let routed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = routed.clone();
    tokio::spawn(async move {
        while let Some(route) = routes.recv().await {
            let url = route.request().url.clone();
            log.lock().unwrap().push(url.clone());
            if url.contains("/secret") {
                route.abort().await.unwrap();
            }
        }
    });
    routed
}

#[tokio::test]
async fn a_frame_from_another_site_is_routed_and_heard() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = framed();
    let page = browser.new_page().await.unwrap();
    let mut events = page.listen();
    let routed = abort_secrets(page.route().await.unwrap());
    page.goto(&format!("{}/framed", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let frame = frame_at(&page, "child", "http://localhost:", "/child").await;
    assert_eq!(fetch_from(&frame, "/secret-api").await, json!("failed"));
    assert_eq!(times_seen(&server, "/secret.png"), 0);
    assert_eq!(times_seen(&server, "/secret-api"), 0);
    let routed = routed.lock().unwrap().clone();
    assert!(
        routed
            .iter()
            .any(|u| u.starts_with("http://localhost:") && u.ends_with("/secret.png")),
        "{routed:?}"
    );

    // Its responses reach listeners, and their bodies can be read.
    let word = frame
        .evaluate("fetch('/api').then(r => r.json()).then(d => d.word)")
        .await
        .unwrap();
    assert_eq!(word, json!("tea"));
    let mut asked = false;
    let api = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Some(NetworkEvent::Request(r)) if r.url.ends_with("/api") => asked = true,
                Some(NetworkEvent::Response(r)) if r.url.ends_with("/api") => return r,
                Some(_) => {}
                None => panic!("the page stopped telling before /api"),
            }
        }
    })
    .await
    .expect("no response came");
    assert!(asked, "the request for /api wasn't heard");
    assert!(api.url.starts_with("http://localhost:"), "{}", api.url);
    let body: serde_json::Value = serde_json::from_slice(&page.body(&api).await.unwrap()).unwrap();
    assert_eq!(body, json!({"word": "tea"}));

    // Without routes, the frame's requests go straight through again.
    page.unroute().await.unwrap();
    assert_eq!(fetch_from(&frame, "/secret-api").await, json!("loaded"));
    assert_eq!(times_seen(&server, "/secret-api"), 1);

    let sent = browser.methods_sent();
    for forbidden in ["Runtime.enable", "Console.enable", "Log.enable"] {
        assert!(!sent.iter().any(|m| m == forbidden), "{sent:?}");
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn block_stops_a_frame_from_another_site_too() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = framed();
    let page = browser.new_page().await.unwrap();
    page.block(&["image"], &[]).await.unwrap();
    page.goto(&format!("{}/framed", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let frame = frame_at(&page, "child", "http://localhost:", "/child").await;
    assert_eq!(times_seen(&server, "/secret.png"), 0);
    // Blocking more once the frame is there reaches it too.
    assert_eq!(fetch_from(&frame, "/secret-api").await, json!("loaded"));
    page.block(&[], &["**/secret-api".to_string()])
        .await
        .unwrap();
    assert_eq!(fetch_from(&frame, "/secret-api").await, json!("failed"));
    assert_eq!(times_seen(&server, "/secret-api"), 1);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_route_added_later_reaches_a_frame_already_there() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = framed();
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/framed", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let frame = frame_at(&page, "child", "http://localhost:", "/child").await;
    assert_eq!(times_seen(&server, "/secret.png"), 1);
    abort_secrets(page.route().await.unwrap());
    assert_eq!(fetch_from(&frame, "/secret-api").await, json!("failed"));
    assert_eq!(times_seen(&server, "/secret-api"), 0);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_frame_that_moves_to_another_site_is_still_routed() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = framed();
    let page = browser.new_page().await.unwrap();
    abort_secrets(page.route().await.unwrap());
    page.goto(&format!("{}/moving", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    frame_at(&page, "child", "http://127.0.0.1:", "/child").await;
    page.evaluate(
        "document.querySelector('iframe').src = `http://localhost:${location.port}/child`",
    )
    .await
    .unwrap();
    let frame = frame_at(&page, "child", "http://localhost:", "/child").await;
    assert_eq!(fetch_from(&frame, "/secret-api").await, json!("failed"));
    assert_eq!(times_seen(&server, "/secret.png"), 0);
    assert_eq!(times_seen(&server, "/secret-api"), 0);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_frame_inside_a_frame_from_another_site_is_routed() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = framed();
    let page = browser.new_page().await.unwrap();
    let routed = abort_secrets(page.route().await.unwrap());
    page.goto(&format!("{}/nested", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let inner = frame_at(&page, "inner", "http://127.0.0.1:", "/child").await;
    assert_eq!(fetch_from(&inner, "/secret-api").await, json!("failed"));
    assert_eq!(times_seen(&server, "/secret.png"), 0);
    assert_eq!(times_seen(&server, "/secret-api"), 0);
    let routed = routed.lock().unwrap().clone();
    assert!(routed.iter().any(|u| u.ends_with("/middle")), "{routed:?}");
    browser.close().await.unwrap();
}

/// A dedicated worker's requests are paused by its frame's session and
/// reported by its own, which is held until it's set up: a worker that
/// was never let go would hang here.
#[tokio::test]
async fn a_workers_requests_are_routed_and_heard() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = framed();
    let page = browser.new_page().await.unwrap();
    let mut events = page.listen();
    let routed = abort_secrets(page.route().await.unwrap());
    page.goto(&format!("{}/worker", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(page.evaluate("window.said").await.unwrap(), json!("failed"));
    // And one started by a worker.
    let said = page
        .evaluate("new Promise(r => { new Worker('/outer.js').onmessage = e => r(e.data) })")
        .await
        .unwrap();
    assert_eq!(said, json!("failed"));
    assert_eq!(times_seen(&server, "/secret-api"), 0);
    let routed = routed.lock().unwrap().clone();
    assert_eq!(
        routed.iter().filter(|u| u.ends_with("/secret-api")).count(),
        2,
        "{routed:?}"
    );
    // Counted up to a request made after both, so one heard twice fails.
    page.evaluate("fetch('/api')").await.unwrap();
    let heard = tokio::time::timeout(Duration::from_secs(30), async {
        let mut heard = 0;
        while let Some(event) = events.recv().await {
            match event {
                NetworkEvent::Request(r) if r.url.ends_with("/secret-api") => heard += 1,
                NetworkEvent::Request(r) if r.url.ends_with("/api") => return heard,
                _ => {}
            }
        }
        heard
    })
    .await;
    assert_eq!(heard, Ok(2));
    browser.close().await.unwrap();
}

/// Chrome holds every kind of target it attaches while it waits for a
/// debugger, worklets included: each must be let go, or it never runs.
#[tokio::test]
async fn worklets_still_load_in_the_page_and_in_a_frame_from_another_site() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = framed();
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/framed", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let frame = frame_at(&page, "child", "http://localhost:", "/child").await;
    let audio = "new OfflineAudioContext(1, 1, 44100).audioWorklet.addModule('/audio.js')\
                 .then(() => 'loaded', e => String(e))";
    // Paint worklets may not exist headless: then there's nothing to load.
    let paint = "'paintWorklet' in CSS ? CSS.paintWorklet.addModule('/paint.js')\
                 .then(() => 'loaded', e => String(e)) : 'loaded'";
    for script in [audio, paint] {
        let on_page = tokio::time::timeout(Duration::from_secs(20), page.evaluate(script)).await;
        assert_eq!(on_page.map(Result::unwrap), Ok(json!("loaded")), "{script}");
        let in_frame = tokio::time::timeout(Duration::from_secs(20), frame.evaluate(script)).await;
        assert_eq!(
            in_frame.map(Result::unwrap),
            Ok(json!("loaded")),
            "{script}"
        );
    }
    browser.close().await.unwrap();
}

/// The guard sees only the page's own documents, but a frame from another
/// site can't take the page anywhere the guard hasn't seen.
#[tokio::test]
async fn a_frame_from_another_site_cant_take_the_page_past_the_guard() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = framed();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = asked.clone();
    let guard: netweir_browser::Guard = std::sync::Arc::new(move |url: String| {
        log.lock().unwrap().push(url.clone());
        Box::pin(async move { !url.ends_with("/refused") })
    });
    let page = browser.new_guarded_page(guard).await.unwrap();
    page.goto(&format!("{}/guarded", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let frame = frame_at(&page, "child", "http://localhost:", "/leaving").await;
    frame.click("#out", None).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !asked
        .lock()
        .unwrap()
        .iter()
        .any(|u| u.ends_with("/refused"))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the guard was never asked about /refused"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(times_seen(&server, "/refused"), 0);
    // Refused, the page shows Chrome's error page, as after a refused goto.
    assert!(!page.url().ends_with("/refused"), "{}", page.url());
    browser.close().await.unwrap();
}
