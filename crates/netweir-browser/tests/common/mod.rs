#![allow(dead_code)]

use std::path::PathBuf;

/// The Chrome to test with, or None (and a note) when there isn't one, so
/// the browser tests skip on machines without Chrome. CI sets
/// NETWEIR_REQUIRE_CHROME to make that a failure instead.
pub fn chrome() -> Option<PathBuf> {
    match netweir_browser::find_chrome() {
        Ok(path) => Some(path),
        Err(e) if std::env::var_os("NETWEIR_REQUIRE_CHROME").is_some() => panic!("{e}"),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

pub fn html(body: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("Content-Type", "text/html; charset=utf-8".into())],
        body: body.to_string(),
    }
}

/// Each request's path and headers.
pub type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

/// A local HTTP/1.1 server for the given paths; anything else is a 404.
/// Records each request's path and headers.
pub struct Server {
    pub url: String,
    pub seen: Seen,
}

impl Server {
    pub fn headers_for(&self, path: &str) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, h)| h.clone())
            .unwrap_or_default()
    }
}

pub fn serve(routes: Vec<(&'static str, Reply)>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let routes = Arc::new(routes);
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let routes = routes.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                // Chrome opens connections ahead of need and closes some
                // unused: one that sends nothing is no request for "/".
                if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    return;
                }
                let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                let mut headers = Vec::new();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.trim_end().split_once(':') {
                        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                    }
                }
                log.lock().unwrap().push((path.clone(), headers));
                let missing = Reply {
                    status: 404,
                    headers: vec![("Content-Type", "text/html".into())],
                    body: "<p>not here</p>".into(),
                };
                let reply = routes
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, r)| r)
                    .unwrap_or(&missing);
                // A test asks for a slow answer with this header, which
                // isn't sent.
                let mut reply_headers = reply.headers.clone();
                if let Some(i) = reply_headers.iter().position(|(k, _)| *k == "X-Delay-Ms") {
                    let (_, ms) = reply_headers.remove(i);
                    std::thread::sleep(std::time::Duration::from_millis(ms.parse().unwrap()));
                }
                let mut out = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (k, v) in &reply_headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&reply.body);
                let _ = stream.write_all(out.as_bytes());
            });
        }
    });
    Server { url, seen }
}

/// A headless browser for a test, or None when there's no Chrome.
/// A turn with Chrome. Only two tests have a Chrome at once: a small CI
/// runner with one each, all at once, takes longer over a page than a test
/// waits. Blocking, so the helpers that start Chrome by hand can take one.
pub struct Turn(());

static TURNS: (Mutex<usize>, std::sync::Condvar) = (Mutex::new(2), std::sync::Condvar::new());

pub fn turn() -> Turn {
    let mut free = TURNS.0.lock().unwrap_or_else(|e| e.into_inner());
    while *free == 0 {
        free = TURNS.1.wait(free).unwrap_or_else(|e| e.into_inner());
    }
    *free -= 1;
    Turn(())
}

impl Drop for Turn {
    fn drop(&mut self) {
        *TURNS.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        TURNS.1.notify_one();
    }
}

/// A launched Chrome, with its turn, for the test.
pub struct TestBrowser {
    browser: netweir_browser::Browser,
    _turn: Turn,
}

impl std::ops::Deref for TestBrowser {
    type Target = netweir_browser::Browser;
    fn deref(&self) -> &netweir_browser::Browser {
        &self.browser
    }
}

/// Launches Chrome with `options`, once it's this test's turn.
pub async fn launch(options: netweir_browser::LaunchOptions) -> TestBrowser {
    let turn = turn();
    TestBrowser {
        browser: netweir_browser::Browser::launch(options).await.unwrap(),
        _turn: turn,
    }
}

pub async fn browser() -> Option<TestBrowser> {
    let executable = chrome()?;
    Some(
        launch(netweir_browser::LaunchOptions {
            executable: Some(executable),
            // Generous: a small CI runner can take many seconds over a
            // page. Tests that expect a timeout set a short one themselves.
            timeout: std::time::Duration::from_secs(30),
            ..Default::default()
        })
        .await,
    )
}

/// An HTTP proxy that wants `user:secret` and answers every request itself
/// with a page naming the URL it was asked for.
pub struct Proxy {
    pub url: String,
}

pub fn proxy() -> Proxy {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let target = line.split(' ').nth(1).unwrap_or("").to_string();
                    let mut authorised = false;
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                            break;
                        }
                        // "user:secret", base64-encoded.
                        if h.to_ascii_lowercase().starts_with("proxy-authorization:")
                            && h.contains("dXNlcjpzZWNyZXQ=")
                        {
                            authorised = true;
                        }
                    }
                    let reply = if authorised {
                        // Scripts, for the workers a page starts.
                        let (kind, body) = if target.ends_with(".js") {
                            (
                                "application/javascript",
                                "postMessage('worker ran')".to_string(),
                            )
                        } else {
                            ("text/html", format!("<p id=via>via proxy: {target}</p>"))
                        };
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n".to_string()
                    };
                    if stream.write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    Proxy { url }
}
