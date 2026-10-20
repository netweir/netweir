//! Every fixture in tests/fixtures/blocks classified, and checked against
//! the outcome it stands for.

use std::time::Duration;

use bytes::Bytes;
use netweir_core::Response;
use netweir_core::classify::{BlockKind, Outcome, classify};

fn load(name: &str) -> Response {
    let path = format!(
        "{}/tests/fixtures/blocks/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a blank line after the headers");
    let head = String::from_utf8(raw[..split].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|l| {
            let (k, v) = l.split_once(": ").unwrap();
            (k.to_ascii_lowercase(), v.to_string())
        })
        .collect();
    Response {
        url: "https://example.com/".into(),
        status,
        version: "HTTP/1.1",
        headers,
        body: Bytes::copy_from_slice(&raw[split + 4..]),
        steps: Default::default(),
    }
}

fn blocked(vendor: &str, kind: BlockKind) -> Outcome {
    Outcome::Blocked {
        vendor: vendor.into(),
        kind,
    }
}

#[test]
fn every_fixture_is_classified_as_it_should_be() {
    let expected = [
        (
            "cloudflare-challenge-header.http",
            blocked("cloudflare", BlockKind::Challenge),
        ),
        (
            "cloudflare-challenge-page.http",
            blocked("cloudflare", BlockKind::Challenge),
        ),
        (
            "cloudflare-block.http",
            blocked("cloudflare", BlockKind::Block),
        ),
        (
            "cloudflare-rate-limit.http",
            blocked("cloudflare", BlockKind::RateLimit),
        ),
        ("akamai-block.http", blocked("akamai", BlockKind::Block)),
        (
            "datadome-captcha.http",
            blocked("datadome", BlockKind::Captcha),
        ),
        ("human-captcha.http", blocked("human", BlockKind::Captcha)),
        (
            "kasada-challenge.http",
            blocked("kasada", BlockKind::Challenge),
        ),
        ("imperva-block.http", blocked("imperva", BlockKind::Block)),
        (
            "aws-waf-challenge.http",
            blocked("aws-waf", BlockKind::Challenge),
        ),
        (
            "aws-waf-captcha.http",
            blocked("aws-waf", BlockKind::Captcha),
        ),
        ("ok-behind-cloudflare.http", Outcome::Ok),
        ("ok-behind-imperva.http", Outcome::Ok),
        ("ok-mentions-vendors.http", Outcome::Ok),
        ("not-found.http", Outcome::HttpError(404)),
        ("forbidden-plain.http", Outcome::HttpError(403)),
        (
            "throttled.http",
            Outcome::Throttled {
                retry_after: Some(Duration::from_secs(30)),
            },
        ),
        ("unavailable.http", Outcome::HttpError(503)),
        (
            "payment.http",
            Outcome::PaymentRequired {
                price: Some("USD 0.01".into()),
            },
        ),
    ];
    let dir = format!("{}/tests/fixtures/blocks", env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".http") && n != "throttled-date.http")
        .collect();
    files.sort();
    let mut listed: Vec<String> = expected.iter().map(|(n, _)| n.to_string()).collect();
    listed.sort();
    assert_eq!(files, listed, "every fixture has an expected outcome");
    for (name, want) in expected {
        assert_eq!(classify(&load(name)), want, "{name}");
    }
}

#[test]
fn retry_after_can_be_a_date() {
    // A date in the past means "now": throttled, with no wait.
    assert_eq!(
        classify(&load("throttled-date.http")),
        Outcome::Throttled {
            retry_after: Some(Duration::ZERO)
        }
    );
}

#[test]
fn every_signature_has_a_fixture() {
    let vendors = netweir_core::classify::vendors();
    for vendor in vendors {
        let dir = format!("{}/tests/fixtures/blocks", env!("CARGO_MANIFEST_DIR"));
        let has = std::fs::read_dir(&dir).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{vendor}-"))
        });
        assert!(has, "no fixture for {vendor}");
    }
}

#[test]
fn a_proxy_adding_its_own_server_header_hides_nothing() {
    let mut r = load("cloudflare-block.http");
    r.headers.insert(0, ("server".into(), "envoy".into()));
    assert_eq!(classify(&r), blocked("cloudflare", BlockKind::Block));
}

#[test]
fn header_values_match_in_any_case() {
    let mut r = load("cloudflare-challenge-header.http");
    for (k, v) in r.headers.iter_mut() {
        if k == "cf-mitigated" {
            *v = "Challenge".into();
        }
    }
    assert_eq!(classify(&r), blocked("cloudflare", BlockKind::Challenge));
}
