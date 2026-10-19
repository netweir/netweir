//! A phone given to one page says, once, what giving it to the whole
//! browser would hide. A test binary of its own: netweir's warning hook
//! is the process's, set once.

mod common;

use std::sync::Mutex;

use common::{html, serve};
use netweir_browser::{Environment, LaunchOptions, WaitUntil};

static SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[tokio::test]
async fn one_pages_phone_says_once_what_a_whole_browsers_hides() {
    netweir_browser::on_warning(|m| SAID.lock().unwrap().push(m.to_string()));
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        ..Default::default()
    })
    .await;
    let server = serve(vec![("/", html("<p>x</p>"))]);
    let mut pages = Vec::new();
    for _ in 0..3 {
        let page = browser
            .new_page_with(Environment::device("Galaxy S21").unwrap())
            .await
            .unwrap();
        page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
        pages.push(page);
    }
    // Shown beside the browser's own screen: Chrome has two.
    assert_eq!(pages[0].evaluate("screen.isExtended").await.unwrap(), true);
    let said = SAID.lock().unwrap().clone();
    let about: Vec<_> = said
        .iter()
        .filter(|m| m.contains("whole browser"))
        .collect();
    assert_eq!(about.len(), 1, "{said:?}");
    browser.close().await.unwrap();
}

/// A dedicated worker's session has no navigator.onLine of its own to
/// set, and its worker follows the page: going offline with one running
/// warns of nothing.
#[tokio::test]
async fn going_offline_with_a_worker_running_warns_of_nothing() {
    netweir_browser::on_warning(|m| SAID.lock().unwrap().push(m.to_string()));
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        ..Default::default()
    })
    .await;
    let worker = common::Reply {
        status: 200,
        headers: vec![("Content-Type", "application/javascript".into())],
        body: "onmessage = () => postMessage(navigator.onLine)".into(),
    };
    let server = serve(vec![
        (
            "/",
            html(
                "<script>window.w = new Worker('/w.js'); window.ask = () => new Promise((ok) => { w.onmessage = (m) => ok(m.data); w.postMessage(1); });</script>",
            ),
        ),
        ("/w.js", worker),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    assert_eq!(page.evaluate("ask()").await.unwrap(), true);
    page.set_offline(true).await.unwrap();
    assert_eq!(page.evaluate("ask()").await.unwrap(), false);
    page.set_offline(false).await.unwrap();
    let said = SAID.lock().unwrap().clone();
    assert!(
        !said.iter().any(|m| m.contains("Not supported")),
        "{said:?}"
    );
    browser.close().await.unwrap();
}
