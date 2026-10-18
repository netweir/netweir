//! Per-request fetch policy: timeouts, redirect limits, reading a redirect
//! instead of following it, and where a login may and may not go.

use std::time::{Duration, Instant};

use netweir_core::{
    Auth, FetchErrorKind, FetchOptions, Fetcher, Outgoing, Policy, Profile, Response,
};
use netweir_fingerprint::{Capture, Server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn options() -> FetchOptions {
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.verify_certificates = false;
    options
}

fn fetcher() -> Fetcher {
    Fetcher::new(options()).unwrap()
}

fn with(policy: Policy) -> Outgoing {
    let mut outgoing = Outgoing::navigate();
    outgoing.policy = policy;
    outgoing
}

/// A password made as the test runs: none sits in the source.
fn password() -> String {
    format!("pw-{}", std::process::id())
}

/// The Authorization a login of ada's with `password()` makes, worked
/// out here rather than by netweir.
fn basic() -> String {
    let pair = format!("ada:{}", password());
    format!("Basic {}", data_encoding::BASE64.encode(pair.as_bytes()))
}

fn login(domain: Option<&str>) -> Policy {
    Policy {
        auth: Some(Auth::new("ada", password(), domain.map(str::to_string))),
        ..Default::default()
    }
}

fn authorization(c: &Capture) -> Option<&str> {
    c.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.as_str())
}

/// A plain-HTTP server on 127.0.0.1 that answers every request after
/// `delay` with `answer` and hands over each request head it read.
async fn plain(
    delay: Duration,
    answer: &'static str,
) -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let _ = tx.send(String::from_utf8_lossy(&head).into_owned());
                tokio::time::sleep(delay).await;
                let _ = stream.write_all(answer.as_bytes()).await;
            });
        }
    });
    (port, rx)
}

const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

#[tokio::test]
async fn a_requests_timeout_cuts_it_short_of_the_clients() {
    let (port, _heads) = plain(Duration::from_secs(30), OK).await;
    let quick = with(Policy {
        timeout: Some(Duration::from_millis(300)),
        ..Default::default()
    });
    let started = Instant::now();
    let err = fetcher()
        .send(&format!("http://127.0.0.1:{port}/"), &quick, &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Timeout, "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_requests_timeout_can_be_longer_than_the_clients() {
    let (port, _heads) = plain(Duration::from_millis(800), OK).await;
    let mut short = options();
    short.timeout = Duration::from_millis(300);
    let fetcher = Fetcher::new(short).unwrap();
    let url = format!("http://127.0.0.1:{port}/");
    let err = fetcher.get(&url, &[]).await.unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Timeout, "the client's own limit");
    let patient = with(Policy {
        timeout: Some(Duration::from_secs(10)),
        ..Default::default()
    });
    let response = fetcher.send(&url, &patient, &[]).await.unwrap();
    assert_eq!(response.status, 200);
}

/// The requests the server saw, as many as `n`, or fewer if no more come
/// within a second.
async fn captures(server: &mut Server, n: usize) -> Vec<Capture> {
    let mut out = Vec::new();
    while out.len() < n {
        match tokio::time::timeout(Duration::from_secs(1), server.next()).await {
            Ok(Some(c)) => out.push(c),
            _ => break,
        }
    }
    out
}

#[tokio::test]
async fn a_redirect_limit_of_its_own_stops_a_request_there() {
    let mut server = Server::start().await.unwrap();
    let url = format!("https://localhost:{}/loop", server.port);
    let three = with(Policy {
        max_redirects: Some(3),
        ..Default::default()
    });
    let err = fetcher().send(&url, &three, &[]).await.unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::TooManyRedirects, "{err}");
    assert!(err.message.contains("more than 3 redirects"), "{err}");
    // The first request and three redirects followed, and no more.
    assert_eq!(captures(&mut server, 10).await.len(), 4);
}

#[tokio::test]
async fn a_clients_redirect_limit_applies_to_every_request() {
    let mut server = Server::start().await.unwrap();
    let mut two = options();
    two.max_redirects = 2;
    let url = format!("https://localhost:{}/loop", server.port);
    let err = Fetcher::new(two).unwrap().get(&url, &[]).await.unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::TooManyRedirects, "{err}");
    assert_eq!(captures(&mut server, 10).await.len(), 3);
}

#[tokio::test]
async fn a_request_that_doesnt_follow_gets_the_redirect_itself() {
    let mut server = Server::start().await.unwrap();
    let url = format!("https://localhost:{}/redirect?to=/after&set=r", server.port);
    let stay = with(Policy {
        dont_redirect: true,
        ..Default::default()
    });
    let fetcher = fetcher();
    let response: Response = fetcher.send(&url, &stay, &[]).await.unwrap();
    assert_eq!(response.status, 302);
    assert_eq!(response.url, url);
    assert_eq!(response.header("location"), Some("/after"));
    assert_eq!(captures(&mut server, 2).await.len(), 1, "not followed");
    // The cookie it set is kept, as when following.
    let next = format!("https://localhost:{}/", server.port);
    fetcher.get(&next, &[]).await.unwrap();
    let seen = server.next().await.unwrap();
    assert!(
        seen.headers
            .iter()
            .any(|(k, v)| k == "cookie" && v == "r=1"),
        "{:?}",
        seen.headers
    );
}

#[tokio::test]
async fn a_login_is_sent_as_basic_to_the_requests_own_origin() {
    let mut server = Server::start().await.unwrap();
    let port = server.port;
    // Same origin after the redirect: both carry it.
    let url = format!("https://localhost:{port}/redirect?to=/after");
    fetcher().send(&url, &with(login(None)), &[]).await.unwrap();
    for c in captures(&mut server, 2).await {
        assert_eq!(authorization(&c), Some(basic().as_str()), "{}", c.path);
    }
    // Another origin after the redirect: it stays behind.
    let url = format!("https://localhost:{port}/redirect?to=https://127.0.0.1:{port}/after");
    fetcher().send(&url, &with(login(None)), &[]).await.unwrap();
    let seen = captures(&mut server, 2).await;
    assert_eq!(authorization(&seen[0]), Some(basic().as_str()));
    assert_eq!(authorization(&seen[1]), None, "leaked to another origin");
}

/// As the Fetch standard drops Authorization from a request the moment a
/// redirect takes it to another origin: for good, not until it comes back.
#[tokio::test]
async fn a_login_left_behind_by_a_redirect_stays_behind_when_it_comes_back() {
    let mut server = Server::start().await.unwrap();
    let port = server.port;
    let url = format!(
        "https://localhost:{port}/redirect?to=https://127.0.0.1:{port}/redirect?to=https://localhost:{port}/after"
    );
    fetcher().send(&url, &with(login(None)), &[]).await.unwrap();
    let seen = captures(&mut server, 3).await;
    assert_eq!(seen.len(), 3);
    assert_eq!(authorization(&seen[0]), Some(basic().as_str()));
    assert_eq!(authorization(&seen[1]), None, "leaked to 127.0.0.1");
    assert_eq!(authorization(&seen[2]), None, "came back with it");
    // The caller's own Authorization goes the same way.
    let mine = [("Authorization".to_string(), "Bearer t0ken".to_string())];
    fetcher()
        .send(&url, &Outgoing::navigate(), &mine)
        .await
        .unwrap();
    let seen = captures(&mut server, 3).await;
    assert_eq!(authorization(&seen[0]), Some("Bearer t0ken"));
    assert_eq!(authorization(&seen[1]), None);
    assert_eq!(authorization(&seen[2]), None, "came back with it");
}

#[tokio::test]
async fn a_login_for_a_domain_goes_to_that_domain_alone() {
    let mut server = Server::start().await.unwrap();
    let port = server.port;
    let away = format!("https://localhost:{port}/redirect?to=https://127.0.0.1:{port}/after");
    // For 127.0.0.1: not to localhost, which redirects there, but there.
    fetcher()
        .send(&away, &with(login(Some("127.0.0.1"))), &[])
        .await
        .unwrap();
    let seen = captures(&mut server, 2).await;
    assert_eq!(authorization(&seen[0]), None, "sent to localhost");
    assert_eq!(authorization(&seen[1]), Some(basic().as_str()));
    // For localhost: there, and not on to 127.0.0.1.
    fetcher()
        .send(&away, &with(login(Some("LOCALHOST"))), &[])
        .await
        .unwrap();
    let seen = captures(&mut server, 2).await;
    assert_eq!(authorization(&seen[0]), Some(basic().as_str()));
    assert_eq!(authorization(&seen[1]), None, "leaked to 127.0.0.1");
}

#[test]
fn a_domain_covers_its_subdomains_and_nothing_that_merely_ends_like_it() {
    let auth = Auth::new("ada", password(), Some("example.com".into()));
    let applies = |url: &str| auth.applies(&url::Url::parse(url).unwrap(), None, false);
    assert!(applies("https://example.com/"));
    assert!(applies("https://intranet.Example.com./a"));
    assert!(!applies("https://badexample.com/"));
    assert!(!applies("https://example.com.evil.net/"));
}

#[tokio::test]
async fn a_domains_login_isnt_sent_over_plain_http_after_https() {
    let mut server = Server::start().await.unwrap();
    let (http_port, mut heads) = plain(Duration::ZERO, OK).await;
    let url = format!(
        "https://localhost:{}/redirect?to=http://localhost:{http_port}/after",
        server.port
    );
    fetcher()
        .send(&url, &with(login(Some("localhost"))), &[])
        .await
        .unwrap();
    assert!(authorization(&server.next().await.unwrap()).is_some());
    let head = heads.recv().await.unwrap();
    assert!(
        !head.to_ascii_lowercase().contains("authorization"),
        "{head}"
    );
}

#[tokio::test]
async fn the_callers_own_authorization_wins_over_a_login() {
    let mut server = Server::start().await.unwrap();
    let url = format!("https://localhost:{}/", server.port);
    let mine = [("Authorization".to_string(), "Bearer t0ken".to_string())];
    fetcher()
        .send(&url, &with(login(None)), &mine)
        .await
        .unwrap();
    let seen = server.next().await.unwrap();
    let all: Vec<_> = seen
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .collect();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].1, "Bearer t0ken");
}

#[test]
fn a_login_never_shows_its_password_in_debug_output() {
    let shown = format!("{:?}", with(login(Some("example.com"))));
    assert!(!shown.contains(&password()), "{shown}");
    assert!(shown.contains("ada"), "{shown}");
}

#[test]
fn a_bad_ca_file_is_an_error_not_an_empty_trust_store() {
    let mut bad = options();
    bad.verify_certificates = true;
    bad.ca_certificates = Some(b"not a certificate".to_vec());
    let err = Fetcher::new(bad).err().unwrap();
    assert_eq!(err.kind, FetchErrorKind::Invalid, "{err}");
}

#[test]
fn a_logins_domain_is_a_bare_host_and_never_a_public_suffix() {
    use netweir_core::auth_domain;
    assert_eq!(
        auth_domain(".Intranet.Example.com.").unwrap(),
        "intranet.example.com"
    );
    assert_eq!(auth_domain("127.0.0.1").unwrap(), "127.0.0.1");
    assert_eq!(auth_domain("[::1]").unwrap(), "::1");
    assert_eq!(auth_domain("localhost").unwrap(), "localhost");
    for bad in [
        "127.0.0.1:8080",
        "example.com:443",
        "http://example.com",
        "example.com/path",
        "ada@example.com",
        "",
        ".",
        "com",
        "co.uk",
        "github.io",
    ] {
        assert!(auth_domain(bad).is_err(), "{bad:?}");
    }
}
