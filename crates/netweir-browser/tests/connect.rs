mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use common::{chrome, html, serve};
use netweir_browser::{Browser, LaunchOptions, WaitUntil};
use serde_json::json;

/// A Chrome started the usual way, with DevTools on a port, as one
/// running elsewhere would be.
struct Running {
    child: std::process::Child,
    ws: String,
    profile: std::path::PathBuf,
    _turn: common::Turn,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn running() -> Option<Running> {
    running_with(&[])
}

fn running_with(extra: &[&str]) -> Option<Running> {
    let executable = chrome()?;
    let turn = common::turn();
    static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    // One each: Chromes sharing a profile hand off to the first.
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let profile = std::env::temp_dir().join(format!("netweir-running-{}-{n}", std::process::id()));
    let mut child = Command::new(executable)
        .args([
            "--headless",
            "--remote-debugging-port=0",
            "--no-first-run",
            &format!("--user-data-dir={}", profile.display()),
        ])
        .args(extra)
        // As netweir's own launches take them.
        .args(
            std::env::var("NETWEIR_CHROME_ARGS")
                .unwrap_or_default()
                .split_whitespace(),
        )
        .arg("about:blank")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = Vec::new();
    let ws = stderr
        .lines()
        .map_while(Result::ok)
        .find_map(|l| {
            let ws = l.strip_prefix("DevTools listening on ").map(str::to_string);
            said.push(l);
            ws
        })
        .unwrap_or_else(|| panic!("Chrome didn't print its DevTools URL:\n{}", said.join("\n")));
    Some(Running {
        child,
        ws,
        profile,
        _turn: turn,
    })
}

#[tokio::test]
async fn a_running_browser_is_driven_and_left_running() {
    let Some(mut chrome) = running() else {
        return;
    };
    let server = serve(vec![("/", html("<p id=x>hello</p>"))]);
    let browser = Browser::connect(&chrome.ws, LaunchOptions::default())
        .await
        .unwrap();
    assert!(browser.version().starts_with("Chrome/"));
    assert_eq!(browser.profile_dir(), None);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#x').textContent")
            .await
            .unwrap(),
        json!("hello")
    );
    // It still looks like Chrome, not a headless one.
    let agent = page.evaluate("navigator.userAgent").await.unwrap();
    assert!(!agent.as_str().unwrap().contains("Headless"), "{agent}");
    browser.close().await.unwrap();
    assert!(page.is_closed() || page.title().await.is_err());
    assert!(
        chrome.child.try_wait().unwrap().is_none(),
        "Chrome kept running"
    );

    // And through the http:// address the WebSocket URL is listed at.
    let port = chrome
        .ws
        .trim_start_matches("ws://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let again = Browser::connect(
        &format!("http://127.0.0.1:{port}"),
        LaunchOptions::default(),
    )
    .await
    .unwrap();
    let page = again.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    again.close().await.unwrap();
}

#[tokio::test]
async fn nothing_listening_is_an_error_that_says_where() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let err = Browser::connect(
        &format!("ws://127.0.0.1:{port}/devtools/browser/x"),
        LaunchOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert!(err.to_string().contains(&port.to_string()), "{err}");
}

/// How many pages the DevTools HTTP endpoint lists at `url`.
fn pages_at(port: &str, url: &str) -> usize {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    write!(
        stream,
        "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
    )
    .unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply);
    String::from_utf8_lossy(&reply)
        .matches(&format!("\"url\": \"{url}\""))
        .count()
}

#[tokio::test]
async fn a_connected_browser_dropped_without_closing_cleans_up_after_itself() {
    let Some(chrome) = running() else {
        return;
    };
    let port = chrome
        .ws
        .trim_start_matches("ws://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .to_string();
    // Only netweir's page shows this URL; Chrome's own tabs come and go
    // on their own time.
    let server = serve(vec![("/mine", html("<p>mine</p>"))]);
    let url = format!("{}/mine", server.url);
    let browser = Browser::connect(&chrome.ws, LaunchOptions::default())
        .await
        .unwrap();
    let page = browser.new_page().await.unwrap();
    page.goto(&url, WaitUntil::Load, None).await.unwrap();
    assert_eq!(pages_at(&port, &url), 1);
    drop(page);
    drop(browser);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while pages_at(&port, &url) > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(pages_at(&port, &url), 0, "netweir's page was left behind");
}

#[tokio::test]
async fn a_refusal_from_devtools_is_shown_as_it_was_given() {
    // What Chrome answers when it's asked by a host name other than an IP
    // address or localhost, as in Docker's http://chrome:9222.
    let server = common::serve(vec![(
        "/json/version",
        common::Reply {
            status: 500,
            headers: vec![],
            body: "Host header is specified and is not an IP address or localhost.".into(),
        },
    )]);
    let err = Browser::connect(&server.url, LaunchOptions::default())
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("Host header"), "{err}");
}

/// A TLS server with a self-signed certificate, which no browser trusts.
fn untrusted_tls() -> u16 {
    use btls::pkey::PKey;
    use btls::ssl::{SslAcceptor, SslMethod};
    use btls::x509::X509;
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = X509::from_pem(made.cert.pem().as_bytes()).unwrap();
    let key = PKey::private_key_from_pem(made.signing_key.serialize_pem().as_bytes()).unwrap();
    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    acceptor.set_private_key(&key).unwrap();
    let acceptor = acceptor.build();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = acceptor.accept(stream);
        }
    });
    port
}

#[tokio::test]
async fn wss_checks_the_certificate() {
    let port = untrusted_tls();
    let err = Browser::connect(
        &format!("wss://localhost:{port}/devtools/browser/x"),
        LaunchOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert!(err.to_string().contains("certificate"), "{err}");
}

#[tokio::test]
async fn a_websocket_that_isnt_devtools_says_so() {
    use futures_util::{SinkExt, StreamExt};
    // Echoes whatever it's sent, as no browser would.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(message)) = ws.next().await {
                    if message.is_text() && ws.send(message).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let err = Browser::connect(&format!("ws://127.0.0.1:{port}/"), LaunchOptions::default())
        .await
        .err()
        .expect("an echo isn't a browser");
    assert!(err.to_string().contains("DevTools"), "{err}");
}

#[tokio::test]
async fn a_connected_chrome_whose_hints_a_flag_emptied_is_left_as_it_is() {
    let Some(chrome) = running_with(&[
        "--user-agent=Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36",
    ]) else {
        return;
    };
    let browser = Browser::connect(&chrome.ws, LaunchOptions::default())
        .await
        .unwrap();
    let page = browser.new_page().await.unwrap();
    page.goto("about:blank", WaitUntil::Load, None)
        .await
        .unwrap();
    // Overriding with the empty hints Chrome reports under the flag would
    // send them as empty headers; Chrome is left to its own.
    assert!(
        !browser
            .methods_sent()
            .iter()
            .any(|m| m == "Network.setUserAgentOverride"),
        "{:?}",
        browser.methods_sent()
    );
    browser.close().await.unwrap();
}

/// Whether DevTools at `ws` says some client is attached to the tab it
/// was started with.
async fn first_tab_attached(ws: &str) -> bool {
    use futures_util::{SinkExt, StreamExt};
    let (mut socket, _) = tokio_tungstenite::connect_async(ws).await.unwrap();
    let ask = json!({"id": 1, "method": "Target.getTargets"}).to_string();
    socket
        .send(tokio_tungstenite::tungstenite::Message::text(ask))
        .await
        .unwrap();
    while let Some(Ok(message)) = socket.next().await {
        let text = message.to_text().unwrap_or("");
        let Ok(reply) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        if reply["id"] == 1 {
            return reply["result"]["targetInfos"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| {
                    t["type"] == "page" && t["url"] == "about:blank" && t["attached"] == true
                });
        }
    }
    panic!("DevTools didn't answer");
}

#[tokio::test]
async fn a_connected_browsers_own_tabs_are_left_alone() {
    let Some(chrome) = running() else {
        return;
    };
    assert!(!first_tab_attached(&chrome.ws).await);
    let browser = Browser::connect(&chrome.ws, LaunchOptions::default())
        .await
        .unwrap();
    // Seen when netweir starts watching for popups, and let go.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while first_tab_attached(&chrome.ws).await && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !first_tab_attached(&chrome.ws).await,
        "netweir stayed attached to a tab that isn't its"
    );
    browser.close().await.unwrap();
}
