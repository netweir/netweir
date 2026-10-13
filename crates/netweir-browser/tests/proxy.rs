mod common;

use common::{chrome, proxy};
use netweir_browser::{Error, LaunchOptions, WaitUntil};
use serde_json::json;

async fn browser_through(proxy: &str) -> Option<common::TestBrowser> {
    let executable = chrome()?;
    Some(
        common::launch(LaunchOptions {
            executable: Some(executable),
            proxy: Some(proxy.to_string()),
            timeout: std::time::Duration::from_secs(30),
            ..LaunchOptions::default()
        })
        .await,
    )
}

#[tokio::test]
async fn a_proxy_that_wants_a_login_gets_one() {
    let proxy = proxy();
    let with_login = proxy.url.replace("http://", "http://user:secret@");
    let Some(browser) = browser_through(&with_login).await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    // Not a loopback address, which Chrome would reach directly.
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
    // The login is answered through DevTools, never shown to the page.
    assert!(browser.methods_sent().iter().any(|m| m == "Fetch.enable"));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_wrong_login_fails_instead_of_asking_forever() {
    let proxy = proxy();
    let wrong = proxy.url.replace("http://", "http://user:nope@");
    let Some(browser) = browser_through(&wrong).await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    let started = std::time::Instant::now();
    let r = page
        .goto("http://shop.test/item", WaitUntil::Load, None)
        .await;
    // Chrome gives up once netweir stops answering: the 407 itself, or a
    // navigation error. Not the 10-second timeout of a login asked for
    // again and again.
    match r {
        Ok(r) => assert_eq!(r.status, 407),
        Err(Error::Navigation { .. }) => {}
        Err(e) => panic!("{e}"),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_pages_workers_get_through_the_proxy_too() {
    let proxy = proxy();
    let with_login = proxy.url.replace("http://", "http://user:secret@");
    let Some(browser) = browser_through(&with_login).await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    page.goto("http://shop.test/item", WaitUntil::Load, None)
        .await
        .unwrap();
    let ran = page
        .evaluate(
            "new Promise((r, no) => { const w = new Worker('/w.js'); w.onmessage = (e) => r(e.data); w.onerror = () => no('worker failed'); setTimeout(() => no('timed out'), 5000); })",
        )
        .await;
    assert_eq!(ran.unwrap(), json!("worker ran"));
    browser.close().await.unwrap();
}
