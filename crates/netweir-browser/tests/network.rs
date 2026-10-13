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
