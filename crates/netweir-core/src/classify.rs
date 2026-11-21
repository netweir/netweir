//! What a response means for a crawl: a page, a block, a request to slow
//! down, a price, or an error.

use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::fetch::Response;

/// How a bot-protection vendor turned the request away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// A JavaScript challenge a real browser would pass.
    Challenge,
    Captcha,
    Block,
    RateLimit,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Challenge => "challenge",
            BlockKind::Captcha => "captcha",
            BlockKind::Block => "block",
            BlockKind::RateLimit => "rate_limit",
        }
    }
}

/// The meaning of one response.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok,
    Blocked {
        vendor: String,
        kind: BlockKind,
    },
    /// 429, or 503 with Retry-After: the site asked for a pause.
    Throttled {
        retry_after: Option<Duration>,
    },
    /// 402: the site charges for access. `price` is its crawler-price.
    PaymentRequired {
        price: Option<String>,
    },
    HttpError(u16),
}

impl Outcome {
    /// The outcome's name, as Python sees it.
    pub fn name(&self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Blocked { .. } => "blocked",
            Outcome::Throttled { .. } => "throttled",
            Outcome::PaymentRequired { .. } => "payment_required",
            Outcome::HttpError(_) => "http_error",
        }
    }
}

#[derive(Debug, Deserialize)]
struct File {
    signature: Vec<Signature>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signature {
    vendor: String,
    kind: BlockKind,
    #[serde(default)]
    status: Vec<u16>,
    header: Option<HeaderRule>,
    server: Option<String>,
    #[serde(default)]
    cookies: Vec<String>,
    #[serde(default)]
    body: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderRule {
    name: String,
    contains: Option<String>,
}

impl Signature {
    /// Lower is more reliable: header, cookie, body, status.
    fn rank(&self) -> u8 {
        if self.header.is_some() {
            0
        } else if !self.cookies.is_empty() {
            1
        } else if !self.body.is_empty() {
            2
        } else {
            3
        }
    }

    fn matches(&self, r: &Response, body: &str) -> bool {
        if !self.status.is_empty() && !self.status.contains(&r.status) {
            return false;
        }
        if let Some(rule) = &self.header {
            let found = r.headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case(&rule.name)
                    && rule
                        .contains
                        .as_ref()
                        .is_none_or(|c| v.to_ascii_lowercase().contains(&c.to_ascii_lowercase()))
            });
            if !found {
                return false;
            }
        }
        if let Some(server) = &self.server {
            let server = server.to_ascii_lowercase();
            // Any of them: a proxy in front may add its own.
            if !r.headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("server") && v.to_ascii_lowercase().contains(&server)
            }) {
                return false;
            }
        }
        if !self.cookies.is_empty() {
            let set = r.headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("set-cookie")
                    && self
                        .cookies
                        .iter()
                        .any(|c| v.trim_start().starts_with(c.as_str()))
            });
            if !set {
                return false;
            }
        }
        self.body.is_empty() || self.body.iter().any(|m| body.contains(&m.to_lowercase()))
    }
}

static SIGNATURES: LazyLock<Vec<Signature>> = LazyLock::new(|| {
    let file: File = toml::from_str(include_str!("../../../signatures/blocks.toml"))
        .expect("signatures/blocks.toml is valid; its test checks it");
    let mut signatures = file.signature;
    // Stable: within a rank, the file's order.
    signatures.sort_by_key(Signature::rank);
    signatures
});

/// How much of the body block markers are looked for in.
const BODY_SCAN: usize = 64 * 1024;

/// The vendors with signatures, each once.
pub fn vendors() -> Vec<&'static str> {
    let mut out: Vec<&str> = SIGNATURES.iter().map(|s| s.vendor.as_str()).collect();
    out.sort_unstable();
    out.dedup();
    out
}

pub fn classify(r: &Response) -> Outcome {
    let end = r.body.len().min(BODY_SCAN);
    let body = String::from_utf8_lossy(&r.body[..end]).to_lowercase();
    if let Some(s) = SIGNATURES.iter().find(|s| s.matches(r, &body)) {
        return Outcome::Blocked {
            vendor: s.vendor.clone(),
            kind: s.kind,
        };
    }
    let retry_after = r.header("retry-after").and_then(retry_after);
    match r.status {
        402 => Outcome::PaymentRequired {
            price: r.header("crawler-price").map(str::to_string),
        },
        429 => Outcome::Throttled { retry_after },
        503 if retry_after.is_some() => Outcome::Throttled { retry_after },
        s if s >= 400 => Outcome::HttpError(s),
        _ => Outcome::Ok,
    }
}

/// Retry-After as a wait: seconds, or an HTTP date (IMF-fixdate, which is
/// what servers send), with a date in the past meaning no wait.
fn retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = http_date(value)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(at.saturating_sub(now)))
}

/// `Wed, 21 Oct 2015 07:28:00 GMT` as seconds since 1970.
fn http_date(s: &str) -> Option<u64> {
    let mut parts = s.split_ascii_whitespace();
    let _weekday = parts.next()?;
    let day: u64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == month_name)? as u64
        + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, sec) = (clock.next()??, clock.next()??, clock.next()??);
    if parts.next()? != "GMT" || !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    // Days since 1970 from a civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + (h * 3600 + m * 60 + sec) as i64).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_file_loads() {
        assert!(SIGNATURES.len() > 10);
        assert!(SIGNATURES.windows(2).all(|w| w[0].rank() <= w[1].rank()));
    }

    #[test]
    fn http_dates() {
        assert_eq!(
            http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            http_date("Tue, 29 Feb 2028 12:00:00 GMT"),
            Some(1_835_438_400)
        );
        assert_eq!(http_date("soon"), None);
        assert_eq!(http_date("Wed, 21 Oct 2015 07:28:00 PST"), None);
        assert_eq!(retry_after(" 12 "), Some(Duration::from_secs(12)));
    }
}
