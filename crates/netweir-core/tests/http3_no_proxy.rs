//! With a system proxy for https in the environment, a host NO_PROXY
//! exempts goes direct, as wreq sends it, and so may move to HTTP/3 as
//! Firefox's AltSvcMapping::AcceptableProxy lets it; every other host
//! stays on TCP. Its own test binary, as it sets the process's
//! environment.

#![cfg(feature = "http3")]

use std::time::{Duration, Instant};

use netweir_core::{FetchOptions, Fetcher, Profile};
use netweir_fingerprint::Server;

#[tokio::test]
async fn a_host_no_proxy_exempts_gets_http3() {
    // SAFETY: the only test in this binary, set before any thread reads
    // the environment.
    unsafe {
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:9");
        std::env::set_var("NO_PROXY", "example.com, localhost");
    }
    let server = Server::start_h3(0, None).await.unwrap();
    let mut options = FetchOptions::new(Profile::named("firefox-160-macos").unwrap());
    options.verify_certificates = false;
    let fetcher = Fetcher::new(options).unwrap();

    // 127.0.0.1 goes through the proxy, so it gets no HTTP/3.
    let proxied = format!("https://127.0.0.1:{}", server.port);
    let alternative = format!("127.0.0.1:{}", server.port);
    assert!(fetcher.use_http3(&proxied, &alternative).is_err());

    // localhost is exempt: it goes direct, and moves to HTTP/3.
    let url = format!("https://localhost:{}/", server.port);
    let started = Instant::now();
    while fetcher.get(&url, &[]).await.unwrap().version != "HTTP/3" {
        assert!(started.elapsed() < Duration::from_secs(10), "never moved");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
