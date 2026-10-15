mod common;

use common::{Reply, browser, serve};
use netweir_browser::{Cookie, StorageState, WaitUntil};
use serde_json::json;

fn page_setting_storage() -> Reply {
    Reply {
        status: 200,
        headers: vec![
            ("Content-Type", "text/html".into()),
            ("Set-Cookie", "session=abc; Path=/; HttpOnly".into()),
        ],
        body: "<link rel=icon href=\"data:,\"><script>localStorage.setItem('cart', '[1,2]'); \
               localStorage.setItem('theme', 'dark');</script>"
            .into(),
    }
}

#[tokio::test]
async fn a_storage_state_carries_cookies_and_local_storage_to_a_new_context() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        ("/", page_setting_storage()),
        ("/other", common::html("<p>other</p>")),
    ]);
    let context = browser.new_context().await.unwrap();
    let page = context.new_page().await.unwrap();
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let requests = server.seen.lock().unwrap().len();
    let state = context.storage_state().await.unwrap();
    // Reading localStorage sent nothing to the site.
    assert_eq!(server.seen.lock().unwrap().len(), requests);
    assert!(
        state
            .cookies
            .iter()
            .any(|c| c.name == "session" && c.value == "abc" && c.http_only),
        "{:?}",
        state.cookies
    );
    let origin = server.url.trim_end_matches('/').to_string();
    assert_eq!(state.origins.len(), 1, "{:?}", state.origins);
    assert_eq!(state.origins[0].origin, origin);
    let mut items = state.origins[0].local_storage.clone();
    items.sort();
    assert_eq!(
        items,
        [
            ("cart".to_string(), "[1,2]".to_string()),
            ("theme".to_string(), "dark".to_string())
        ]
    );

    // Through JSON, as a file would take it, into a new context.
    let state = StorageState::from_json(&state.to_json()).unwrap();
    let fresh = browser.new_context().await.unwrap();
    fresh.set_storage_state(&state).await.unwrap();
    let requests = server.seen.lock().unwrap().len();
    let page = fresh.new_page().await.unwrap();
    page.goto(&format!("{}/other", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(server.seen.lock().unwrap().len(), requests + 1);
    let cookie = server
        .headers_for("/other")
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
        .map(|(_, v)| v);
    assert_eq!(cookie.as_deref(), Some("session=abc"));
    let kept = page
        .evaluate("() => [localStorage.getItem('cart'), localStorage.getItem('theme')]")
        .await
        .unwrap();
    assert_eq!(kept, json!(["[1,2]", "dark"]));
    // The new context knows the origin as its own.
    assert_eq!(fresh.storage_state().await.unwrap().origins, state.origins);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_hosts_own_cookie_stays_its_own_in_chrome() {
    let Some(browser) = browser().await else {
        return;
    };
    let context = browser.new_context().await.unwrap();
    let cookie = |name: &str, domain: &str| Cookie {
        name: name.into(),
        value: "1".into(),
        domain: domain.into(),
        path: "/".into(),
        expires: -1.0,
        http_only: false,
        secure: true,
        same_site: None,
        partition_key: None,
    };
    context
        .set_cookies(&[
            cookie("own", "shop.example"),
            cookie("wide", ".shop.example"),
        ])
        .await
        .unwrap();
    let mut back = context.cookies().await.unwrap();
    back.sort_by(|a, b| a.name.cmp(&b.name));
    let domains: Vec<&str> = back.iter().map(|c| c.domain.as_str()).collect();
    assert_eq!(domains, ["shop.example", ".shop.example"]);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_lone_pages_storage_state_reads_its_own_context() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", page_setting_storage())]);
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let state = page.storage_state().await.unwrap();
    assert_eq!(state.cookies.len(), 1);
    assert_eq!(state.origins.len(), 1);
    let other = browser.new_page().await.unwrap();
    assert_eq!(
        other.storage_state().await.unwrap(),
        StorageState::default()
    );
    browser.close().await.unwrap();
}
