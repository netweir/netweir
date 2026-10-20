//! A scripted local website for crawl tests: plain HTTP/1.1, a fixed set
//! of pages, and a log of every request with the time it arrived.

// Each test crate that includes this uses only some of it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct Page {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub delay: Duration,
    /// Served instead when the request's Cookie header contains `.0`.
    pub pass: Option<(String, Box<Page>)>,
}

impl Page {
    pub fn html(body: &str) -> Page {
        Page {
            status: 200,
            headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
            body: body.to_string(),
            delay: Duration::ZERO,
            pass: None,
        }
    }

    pub fn status(status: u16, body: &str) -> Page {
        Page {
            status,
            headers: Vec::new(),
            body: body.to_string(),
            delay: Duration::ZERO,
            pass: None,
        }
    }

    /// This page, or `page` for a request that carries `cookie`.
    pub fn unless_cookie(mut self, cookie: &str, page: Page) -> Page {
        self.pass = Some((cookie.to_string(), Box::new(page)));
        self
    }

    pub fn with_header(mut self, k: &str, v: &str) -> Page {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn slow(mut self, delay: Duration) -> Page {
        self.delay = delay;
        self
    }
}

/// A request's path and headers.
pub type Request = (String, Vec<(String, String)>);

pub struct Site {
    pub port: u16,
    pub hits: Arc<Mutex<Vec<(String, Instant)>>>,
    /// Each request's headers, in arrival order, lowercase names.
    pub requests: Arc<Mutex<Vec<Request>>>,
    /// Each request's head, request line to blank line, byte for byte.
    pub heads: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Site {
    pub async fn start(pages: Vec<(&str, Page)>) -> Site {
        Site::scripted(pages.into_iter().map(|(p, page)| (p, vec![page])).collect()).await
    }

    /// Each path answers with its pages in turn, the last one for good.
    pub async fn scripted(pages: Vec<(&str, Vec<Page>)>) -> Site {
        let pages: Arc<HashMap<String, Vec<Page>>> = Arc::new(
            pages
                .into_iter()
                .map(|(p, page)| (p.to_string(), page))
                .collect(),
        );
        let served: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let seen = requests.clone();
        let heads: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let raw = heads.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let log = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let pages = pages.clone();
                let log = log.clone();
                let served = served.clone();
                let seen = seen.clone();
                let raw = raw.clone();
                tokio::spawn(async move {
                    loop {
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        // When the request's first byte came: read a byte
                        // at a time, its last could come a scheduler's
                        // turn or two later on a busy machine, and the
                        // gap between two requests look shorter than it
                        // was (most likely the 263 ms for 300 a CI Mac saw).
                        let mut arrived = None;
                        while !head.ends_with(b"\r\n\r\n") {
                            match stream.read(&mut byte).await {
                                Ok(1) => {
                                    arrived.get_or_insert_with(Instant::now);
                                    head.push(byte[0]);
                                }
                                _ => return,
                            }
                        }
                        let arrived = arrived.unwrap_or_else(Instant::now);
                        raw.lock().unwrap().push(head.clone());
                        let text = String::from_utf8_lossy(&head).to_string();
                        let path = text.split(' ').nth(1).unwrap_or("/").to_string();
                        log.lock().unwrap().push((path.clone(), arrived));
                        let headers: Vec<(String, String)> = text
                            .lines()
                            .skip(1)
                            .filter_map(|l| l.split_once(':'))
                            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                            .collect();
                        let cookie = headers
                            .iter()
                            .find(|(k, _)| k == "cookie")
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default();
                        seen.lock().unwrap().push((path.clone(), headers));
                        let page = match pages.get(&path) {
                            Some(script) => {
                                let mut served = served.lock().unwrap();
                                let n = served.entry(path.clone()).or_insert(0);
                                let page = script[(*n).min(script.len() - 1)].clone();
                                *n += 1;
                                page
                            }
                            None => Page::status(404, "not found"),
                        };
                        let page = match &page.pass {
                            Some((wanted, pass)) if cookie.contains(wanted.as_str()) => {
                                (**pass).clone()
                            }
                            _ => page,
                        };
                        tokio::time::sleep(page.delay).await;
                        let mut out = format!(
                            "HTTP/1.1 {} X\r\ncontent-length: {}\r\n",
                            page.status,
                            page.body.len()
                        );
                        for (k, v) in &page.headers {
                            out.push_str(&format!("{k}: {v}\r\n"));
                        }
                        out.push_str("\r\n");
                        out.push_str(&page.body);
                        if stream.write_all(out.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Site {
            port,
            hits,
            requests,
            heads,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    /// Paths requested, in order.
    pub fn paths(&self) -> Vec<String> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// When each request for `path` arrived.
    pub fn times(&self, path_prefix: &str) -> Vec<Instant> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.starts_with(path_prefix))
            .map(|(_, t)| *t)
            .collect()
    }
}
