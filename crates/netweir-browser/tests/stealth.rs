//! A page that looks for automation the way bot detection does, from page
//! script, and a check that netweir never sent what it promises not to.

mod common;

use common::{html, serve};
use netweir_browser::WaitUntil;
use serde_json::json;

const DETECT: &str = r#"<!DOCTYPE html><script>
window.findings = (async () => {
  const workerAgent = await new Promise((resolve) => {
    const w = new Worker(URL.createObjectURL(new Blob(['postMessage(navigator.userAgent)'])));
    w.onmessage = (m) => resolve(m.data);
  });
  const serviceWorkerAgent = await (async () => {
    const reg = await navigator.serviceWorker.register('/sw.js');
    await navigator.serviceWorker.ready;
    const sw = reg.active || reg.waiting || reg.installing;
    return await new Promise((resolve) => {
      navigator.serviceWorker.onmessage = (m) => resolve(m.data);
      sw.postMessage('ua?');
    });
  })();
  const hints = await navigator.userAgentData.getHighEntropyValues(['architecture', 'fullVersionList', 'platformVersion']);
  return {
    webdriver: navigator.webdriver,
    agent: navigator.userAgent,
    workerAgent,
    serviceWorkerAgent,
    brands: navigator.userAgentData.brands.map((b) => b.brand),
    architecture: hints.architecture,
    fullVersions: hints.fullVersionList.length,
  };
})();
</script>"#;

#[tokio::test]
async fn a_page_finds_no_sign_of_automation() {
    let Some(executable) = common::chrome() else {
        return;
    };
    // As netweir launches it: Chrome for Testing names itself Chromium
    // alone, and is given the brands Google Chrome presents (netweir-core
    // reads them from its profiles; these are their shape).
    let brands: netweir_browser::Brands = std::sync::Arc::new(|major: &str| {
        Some(format!(
            "\"Google Chrome\";v=\"{major}\", \"Chromium\";v=\"{major}\", \"Not)A;Brand\";v=\"24\""
        ))
    });
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        brands: Some(brands),
        timeout: std::time::Duration::from_secs(30),
        ..Default::default()
    })
    .await;
    let mut detect = html(DETECT);
    // Asks for the high-entropy hints on the next request.
    detect
        .headers
        .push(("Accept-CH", "Sec-CH-UA-Full-Version-List".into()));
    let worker = common::Reply {
        status: 200,
        headers: vec![("Content-Type", "application/javascript".into())],
        body: "self.onmessage = (e) => e.source.postMessage(navigator.userAgent);".into(),
    };
    let server = serve(vec![
        ("/", detect),
        ("/again", html("<p>again</p>")),
        ("/sw.js", worker),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let found = page.evaluate("findings").await.unwrap();

    assert_eq!(found["webdriver"], json!(false), "{found}");
    for agent in [
        &found["agent"],
        &found["workerAgent"],
        &found["serviceWorkerAgent"],
    ] {
        let agent = agent.as_str().unwrap();
        assert!(
            agent.contains("Chrome/") && !agent.contains("Headless"),
            "{agent}"
        );
    }
    assert!(!found["brands"].to_string().contains("Headless"), "{found}");
    assert_ne!(
        found["architecture"],
        json!(""),
        "high-entropy hints are empty: {found}"
    );
    assert_ne!(found["fullVersions"], json!(0), "{found}");

    page.goto(&format!("{}/again", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let headers = server.headers_for("/again");
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("no {name} in {headers:?}"))
    };
    assert!(!header("user-agent").contains("Headless"));
    assert!(!header("sec-ch-ua").contains("Headless"));
    assert!(header("sec-ch-ua-full-version-list").contains("Chrome"));

    // Runtime.enable used to show itself to page script through console
    // previews that read an error's stack; Chrome 154 no longer does, so
    // the only check left is that it's never sent.
    page.click("p", None).await.unwrap();
    page.fill("p", "", Some(std::time::Duration::from_millis(100)))
        .await
        .unwrap_err();
    page.screenshot(false).await.unwrap();
    let sent = browser.methods_sent();
    for forbidden in ["Runtime.enable", "Console.enable", "Log.enable"] {
        assert!(
            !sent.iter().any(|m| m == forbidden),
            "{forbidden} was sent: {sent:?}"
        );
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn pages_present_the_brands_netweir_is_told_to() {
    let Some(executable) = common::chrome() else {
        return;
    };
    // Chrome for Testing calls itself Chromium alone; netweir's profiles
    // know what Google Chrome of the same version says.
    let brands: netweir_browser::Brands = std::sync::Arc::new(|major: &str| {
        Some(format!(
            "\"Chromium\";v=\"{major}\", \"Brand X\";v=\"{major}\", \"Not A(Brand\";v=\"99\""
        ))
    });
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        brands: Some(brands),
        ..Default::default()
    })
    .await;
    let major = browser
        .version()
        .trim_start_matches("Chrome/")
        .split('.')
        .next()
        .unwrap()
        .to_string();
    let mut page_html = html("<p>x</p>");
    page_html
        .headers
        .push(("Accept-CH", "Sec-CH-UA-Full-Version-List".into()));
    let server = serve(vec![("/", page_html), ("/again", html("<p>again</p>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let seen = page
        .evaluate("navigator.userAgentData.getHighEntropyValues(['fullVersionList']).then(h => [navigator.userAgentData.brands.map(b => b.brand + '/' + b.version), h.fullVersionList.map(b => b.brand + '/' + b.version)])")
        .await
        .unwrap();
    let full = browser.version().trim_start_matches("Chrome/").to_string();
    assert_eq!(
        seen,
        json!([
            [
                "Chromium/".to_string() + &major,
                "Brand X/".to_string() + &major,
                "Not A(Brand/99".to_string()
            ],
            [
                "Chromium/".to_string() + &full,
                "Brand X/".to_string() + &full,
                "Not A(Brand/99.0.0.0".to_string()
            ],
        ])
    );
    page.goto(&format!("{}/again", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let sent = server.headers_for("/again");
    let ua = sent
        .iter()
        .find(|(k, _)| k == "sec-ch-ua")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert!(ua.contains("\"Brand X\""), "{ua}");
    browser.close().await.unwrap();
}

/// The names of the headers of the request for `path`, in order.
fn order(server: &common::Server, path: &str) -> Vec<String> {
    server
        .headers_for(path)
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

fn names(s: &str) -> Vec<String> {
    s.split(' ').map(str::to_string).collect()
}

/// What Google Chrome 155 on a Mac sent, started by hand with no
/// DevTools at all: the order of its headers for a document, a
/// stylesheet, a worker's script and what the worker fetches.
pub const DOCUMENT: &str = "host connection sec-ch-ua sec-ch-ua-mobile sec-ch-ua-platform \
    upgrade-insecure-requests user-agent accept sec-fetch-site sec-fetch-mode sec-fetch-user \
    sec-fetch-dest accept-encoding accept-language";
pub const STYLESHEET: &str = "host connection sec-ch-ua-platform user-agent sec-ch-ua \
    sec-ch-ua-mobile accept sec-fetch-site sec-fetch-mode sec-fetch-dest referer accept-encoding \
    accept-language";
pub const WORKER_SCRIPT: &str = "host connection accept sec-fetch-site sec-fetch-mode \
    sec-fetch-dest referer user-agent accept-encoding accept-language";
pub const WORKER_FETCH: &str = "host connection user-agent accept sec-fetch-site sec-fetch-mode \
    sec-fetch-dest referer accept-encoding accept-language";

/// As Google Chrome 155 on a Mac shows itself started by hand: the window
/// around its page, its languages, and the order of its headers.
#[tokio::test]
async fn a_page_has_the_frame_languages_and_header_order_of_a_persons_chrome() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        ..Default::default()
    })
    .await;
    let js = |body: &str| common::Reply {
        status: 200,
        headers: vec![("Content-Type", "application/javascript".into())],
        body: body.into(),
    };
    let server = serve(vec![
        (
            "/",
            html(
                "<link rel=stylesheet href=/s.css><script>window.w = new Promise((ok) => \
                 { const w = new Worker('/w.js'); w.onmessage = ok; });</script>",
            ),
        ),
        (
            "/s.css",
            common::Reply {
                status: 200,
                headers: vec![("Content-Type", "text/css".into())],
                body: "p{}".into(),
            },
        ),
        ("/w.js", js("fetch('/f').finally(() => postMessage(1))")),
        ("/f", html("x")),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    page.evaluate("w.then(() => 1)").await.unwrap();
    // Chrome for Testing shows an infobar a person's Chrome doesn't, and
    // its field trial config holds the languages back to one. A headless
    // window's frame is Google Chrome's on every system.
    assert_eq!(
        page.evaluate("outerHeight - innerHeight").await.unwrap(),
        json!(87)
    );
    assert_eq!(
        page.evaluate("navigator.languages").await.unwrap(),
        json!(["en-US", "en"])
    );
    assert_eq!(order(&server, "/"), names(DOCUMENT));
    assert_eq!(order(&server, "/s.css"), names(STYLESHEET));
    assert_eq!(order(&server, "/f"), names(WORKER_FETCH));
    browser.close().await.unwrap();
}

/// Every kind of worker script, as Google Chrome 155 on a Mac sent each
/// with no DevTools attached (the probe is in the design note): the
/// worker's own script after Referer, what it imports with User-Agent
/// first.
const WORKER_KINDS: &[(&str, &str)] = &[
    ("/classic.js", WORKER_SCRIPT),
    ("/imported.js", WORKER_FETCH),
    ("/nested.js", WORKER_SCRIPT),
    ("/module.js", WORKER_SCRIPT),
    (
        "/dep.js",
        "host connection user-agent origin accept sec-fetch-site sec-fetch-mode \
         sec-fetch-dest referer accept-encoding accept-language",
    ),
    ("/shared.js", WORKER_SCRIPT),
    ("/shared-module.js", WORKER_SCRIPT),
    (
        "/shared-dep.js",
        "host connection user-agent origin accept sec-fetch-site sec-fetch-mode \
         sec-fetch-dest referer accept-encoding accept-language",
    ),
];

fn worker_site() -> common::Server {
    let js = |body: &str| common::Reply {
        status: 200,
        headers: vec![("Content-Type", "application/javascript".into())],
        body: body.into(),
    };
    serve(vec![
        (
            "/",
            html(
                "<script>const wait = (w, port) => new Promise((ok) => { (port || w).onmessage = ok; \
                 setTimeout(ok, 4000); }); window.done = (async () => { \
                 await wait(new Worker('/classic.js')); \
                 await wait(new Worker('/module.js', { type: 'module' })); \
                 const s = new SharedWorker('/shared.js'); await wait(s, s.port); \
                 const m = new SharedWorker('/shared-module.js', { type: 'module' }); await wait(m, m.port); \
                 return 1; })();</script>",
            ),
        ),
        (
            "/classic.js",
            js(
                "importScripts('/imported.js'); const n = new Worker('/nested.js'); n.onmessage = () => postMessage(1);",
            ),
        ),
        ("/imported.js", js("1;")),
        ("/nested.js", js("postMessage(1);")),
        ("/module.js", js("import '/dep.js'; postMessage(1);")),
        ("/dep.js", js("export const x = 1;")),
        (
            "/shared.js",
            js("onconnect = (e) => e.ports[0].postMessage(1);"),
        ),
        (
            "/shared-module.js",
            js("import '/shared-dep.js'; onconnect = (e) => e.ports[0].postMessage(1);"),
        ),
        ("/shared-dep.js", js("export const y = 1;")),
    ])
}

/// The user agent override every page has (it keeps the client hints the
/// --user-agent switch empties) puts User-Agent, and a language's
/// Accept-Language, early in a worker script's headers, unless
/// `worker_header_order` puts them back.
#[tokio::test]
async fn worker_scripts_send_their_headers_in_chromes_own_order() {
    let Some(executable) = common::chrome() else {
        return;
    };
    for environment in [
        netweir_browser::Environment::default(),
        netweir_browser::Environment {
            locale: Some("de-DE".into()),
            ..Default::default()
        },
    ] {
        let browser = common::launch(netweir_browser::LaunchOptions {
            executable: Some(executable.clone()),
            environment: environment.clone(),
            worker_header_order: true,
            ..Default::default()
        })
        .await;
        let server = worker_site();
        let page = browser.new_page().await.unwrap();
        page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
            .await
            .unwrap();
        page.evaluate("done").await.unwrap();
        for (path, want) in WORKER_KINDS {
            assert_eq!(
                order(&server, path),
                names(want),
                "{path} with {environment:?}"
            );
        }
        browser.close().await.unwrap();
    }
}

/// On a page that isn't a secure context (plain http, not loopback) a
/// request sends no client hints and no Sec-Fetch headers, so a favicon's
/// request looks like a worker script's but for its Accept: as Google
/// Chrome 155 sent them to a LAN address, its User-Agent first, the
/// worker's after Referer.
#[tokio::test]
async fn a_page_that_isnt_a_secure_context_keeps_chromes_order() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        worker_header_order: true,
        args: vec!["--host-resolver-rules=MAP plain.test 127.0.0.1".into()],
        ..Default::default()
    })
    .await;
    let js = |body: &str| common::Reply {
        status: 200,
        headers: vec![("Content-Type", "application/javascript".into())],
        body: body.into(),
    };
    let server = serve(vec![
        (
            "/",
            html(
                "<link rel=icon href=/fav.ico><script>window.w = new Promise((ok) => \
                 { const w = new Worker('/w.js'); w.onmessage = ok; });</script>",
            ),
        ),
        ("/fav.ico", html("")),
        ("/w.js", js("postMessage(1)")),
    ]);
    let url = server.url.replace("127.0.0.1", "plain.test");
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{url}/"), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(page.evaluate("isSecureContext").await.unwrap(), false);
    page.evaluate("w.then(() => 1)").await.unwrap();
    for _ in 0..100 {
        if !server.headers_for("/fav.ico").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        order(&server, "/"),
        names(
            "host connection upgrade-insecure-requests user-agent accept accept-encoding accept-language"
        )
    );
    assert_eq!(
        order(&server, "/fav.ico"),
        names("host connection user-agent accept referer accept-encoding accept-language")
    );
    assert_eq!(
        order(&server, "/w.js"),
        names("host connection accept referer user-agent accept-encoding accept-language")
    );
    browser.close().await.unwrap();
}
