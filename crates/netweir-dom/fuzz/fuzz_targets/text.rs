//! Any string as a price and as a date, read both ways round.
#![no_main]

use libfuzzer_sys::fuzz_target;
use netweir_dom::date::{self, Civil};
use netweir_dom::price;

/// `-?\d+(\.\d+)?`: what Python's Decimal() is given.
fn is_decimal(amount: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let unsigned = amount.strip_prefix('-').unwrap_or(amount);
    match unsigned.split_once('.') {
        Some((whole, frac)) => digits(whole) && digits(frac),
        None => digits(unsigned),
    }
}

fn valid(c: Civil) -> bool {
    (1..=9999).contains(&c.year)
        && (1..=12).contains(&c.month)
        && (1..=31).contains(&c.day)
        && c.hour < 24
        && c.minute < 60
        && c.second < 60
        && c.micro < 1_000_000
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Some(p) = price::parse_price(&text) {
        assert!(is_decimal(&p.amount), "{:?} from {text:?}", p.amount);
    }
    let now = Civil::date(2026, 10, 12);
    for day_first in [None, Some(false), Some(true)] {
        if let Some(p) = date::parse_date(&text, day_first, now) {
            assert!(valid(p.civil), "{p:?} from {text:?}");
            // Python adds the elapsed seconds; the result must be in range.
            assert!(p.naive().is_some_and(valid), "{p:?} from {text:?}");
        }
    }
});
