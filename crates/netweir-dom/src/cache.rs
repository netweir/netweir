//! Compiled queries, kept per thread so a query written inside a loop
//! (`card.css("h3 a::attr(title)")` once per item) is compiled once, not
//! once per item. Compiling costs more than running a query on one small
//! element, so this is most of the price of the per-item pattern.
//!
//! Per thread rather than shared: a lookup takes no lock, so threads on
//! free-threaded Python don't queue on one. A compiled [`crate::Query`] or
//! [`crate::XPath`] is safe to share, so this is a speed choice, not a
//! safety one.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread::LocalKey;

/// How many distinct queries of each kind (CSS, XPath) a thread keeps.
pub const QUERY_CACHE_SIZE: usize = 64;

/// How many bytes of query text each of a thread's caches keeps. A compiled
/// query's memory grows with its text: a short CSS query holds about 6 KB, a
/// 60-selector list 45 KB, and the densest queries (`a,a,a,...`, a
/// thousand-branch `:is()`, a long XPath of predicates) about 130 times
/// their text. So this, not the count, bounds a cache fed long queries.
/// With both limits a thread's two caches hold about 0.5 MB of ordinary
/// queries and at most about 2.2 MB of pathological ones: that, times the
/// threads that query.
pub const QUERY_CACHE_TEXT: usize = 8 * 1024;

pub(crate) type Cache<T> = RefCell<Entries<T>>;

pub(crate) struct Entries<T> {
    map: HashMap<Box<str>, Arc<T>>,
    /// The total length of the keys in `map`.
    text: usize,
}

impl<T> Default for Entries<T> {
    fn default() -> Self {
        Entries {
            map: HashMap::new(),
            text: 0,
        }
    }
}

/// The compiled query for `key`, from `cache` or made with `make` and kept.
/// Failures aren't kept: they raise, and nobody loops over a bad query.
pub(crate) fn lookup<T, E>(
    cache: &'static LocalKey<Cache<T>>,
    key: &str,
    make: impl FnOnce(&str) -> Result<T, E>,
) -> Result<Arc<T>, E> {
    let hit = cache
        .try_with(|c| c.borrow().map.get(key).cloned())
        .ok()
        .flatten();
    if let Some(hit) = hit {
        return Ok(hit);
    }
    let made = Arc::new(make(key)?);
    // A query longer than the whole budget would empty the cache and then
    // hold a compiled size of about 130 times its text on its own: it's
    // compiled for this call and not kept.
    if key.len() > QUERY_CACHE_TEXT {
        return Ok(made);
    }
    // Fails only while the thread is exiting; then the query isn't kept.
    let _ = cache.try_with(|c| {
        let mut c = c.borrow_mut();
        // Known limit: forgets everything when full rather than the least
        // recently used. A loop of unique queries thrashes either way, and
        // hot queries come back after one compile each; LRU if a workload
        // with more than 64 hot queries shows up.
        if c.map.len() >= QUERY_CACHE_SIZE || c.text + key.len() > QUERY_CACHE_TEXT {
            c.map.clear();
            c.text = 0;
        }
        c.text += key.len();
        c.map.insert(key.into(), made.clone());
    });
    Ok(made)
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local!(static CACHE: Cache<String> = Default::default());

    #[test]
    fn keeps_a_query_and_stays_bounded() {
        let make = |k: &str| Ok::<_, ()>(k.to_string());
        let a = lookup(&CACHE, "a", make).unwrap();
        assert!(Arc::ptr_eq(&a, &lookup(&CACHE, "a", make).unwrap()));
        assert_eq!(lookup(&CACHE, "bad", |_| Err::<String, _>("no")), Err("no"));
        for i in 0..QUERY_CACHE_SIZE * 3 {
            lookup(&CACHE, &i.to_string(), make).unwrap();
        }
        CACHE.with_borrow(|c| {
            assert!(c.map.len() <= QUERY_CACHE_SIZE);
            assert!(!c.map.contains_key("bad"));
        });
        // Long queries are bounded by their text, not their count.
        let long = "x".repeat(QUERY_CACHE_TEXT / 4);
        for i in 0..10 {
            lookup(&CACHE, &format!("{long}{i}"), make).unwrap();
        }
        CACHE.with_borrow(|c| {
            assert!(c.text <= QUERY_CACHE_TEXT);
            assert_eq!(c.text, c.map.keys().map(|k| k.len()).sum::<usize>());
        });
        // One longer than the budget is answered but not kept, and the
        // cache keeps what it had.
        let before = CACHE.with_borrow(|c| c.map.len());
        let huge = "y".repeat(QUERY_CACHE_TEXT + 1);
        lookup(&CACHE, &huge, make).unwrap();
        CACHE.with_borrow(|c| {
            assert!(!c.map.contains_key(huge.as_str()));
            assert_eq!(c.map.len(), before);
        });
    }
}
