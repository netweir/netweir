use crate::client_hello::{ClientHello, is_grease};
use crate::server::Capture;

const PRE_SHARED_KEY: u16 = 0x0029;

/// Everything about `actual` that would tell a server it is not the browser
/// `expected` was captured from. Empty means indistinguishable.
///
/// GREASE values and extension order are ignored, because browsers
/// randomise both on every connection. So is whether the TLS session was
/// resumed (the pre_shared_key extension, which also changes JA4), since
/// that depends on connection history, not on the browser. The `:authority`,
/// `:path` and `Host` values are ignored because they name the test server
/// and its port, and so is the port in `Origin` and `Referer`.
///
/// Where GREASE goes is not ignored: a browser that GREASEs must be matched
/// GREASE for GREASE, and one that doesn't, by none. Nor is its shape (see
/// `grease_shape`).
///
/// For a browser that sends its TLS extensions in a fixed order (Firefox,
/// Safari), whether the order matches too. `differences` compares them as a
/// set, since Chrome shuffles them.
pub fn extension_order_differs(expected: &Capture, actual: &Capture) -> Option<String> {
    let order = |c: &Capture| -> Vec<u16> {
        c.client_hello
            .extensions
            .iter()
            .copied()
            .filter(|&x| !is_grease(x) && x != PRE_SHARED_KEY && x != 0x0000)
            .collect()
    };
    let (e, a) = (order(expected), order(actual));
    (e != a).then(|| format!("extension order: expected {e:04x?}, got {a:04x?}"))
}

pub fn differences(expected: &Capture, actual: &Capture) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, a: String, b: String| {
        if a != b {
            out.push(format!("{what}: expected {a}, got {b}"));
        }
    };
    // Each GREASE value as "GREASE", in its place: which value a browser
    // picks changes per connection, but whether it sends one, and where,
    // doesn't.
    let masked = |v: &[u16]| -> Vec<String> {
        v.iter()
            .map(|&x| {
                if is_grease(x) {
                    "GREASE".to_string()
                } else {
                    format!("{x:04x}")
                }
            })
            .collect()
    };
    let (e, a) = (&expected.client_hello, &actual.client_hello);
    let resumed = |c: &Vec<u16>| c.contains(&PRE_SHARED_KEY);
    if resumed(&e.extensions) == resumed(&a.extensions) {
        check("ja4", expected.ja4.clone(), actual.ja4.clone());
    }
    let fresh =
        |set: Vec<u16>| -> Vec<u16> { set.into_iter().filter(|&x| x != PRE_SHARED_KEY).collect() };
    check(
        "ciphers",
        format!("{:?}", masked(&e.ciphers)),
        format!("{:?}", masked(&a.ciphers)),
    );
    check(
        "extensions",
        format!("{:04x?}", fresh(e.extension_set())),
        format!("{:04x?}", fresh(a.extension_set())),
    );
    check(
        "GREASE extensions",
        format!("{:?}", grease_places(&e.extensions)),
        format!("{:?}", grease_places(&a.extensions)),
    );
    check(
        "groups",
        format!("{:?}", masked(&e.supported_groups)),
        format!("{:?}", masked(&a.supported_groups)),
    );
    check(
        "key shares",
        format!("{:?}", masked(&e.key_share_groups)),
        format!("{:?}", masked(&a.key_share_groups)),
    );
    check(
        "signature algorithms",
        format!("{:?}", masked(&e.signature_algorithms)),
        format!("{:?}", masked(&a.signature_algorithms)),
    );
    check(
        "supported versions",
        format!("{:?}", masked(&e.supported_versions)),
        format!("{:?}", masked(&a.supported_versions)),
    );
    check("alpn", format!("{:?}", e.alpn), format!("{:?}", a.alpn));
    check("alps", format!("{:?}", e.alps), format!("{:?}", a.alps));
    check(
        "certificate compression",
        format!("{:?}", e.cert_compression),
        format!("{:?}", a.cert_compression),
    );
    check(
        "psk modes",
        format!("{:?}", e.psk_modes),
        format!("{:?}", a.psk_modes),
    );
    check(
        "ec point formats",
        format!("{:?}", e.ec_point_formats),
        format!("{:?}", a.ec_point_formats),
    );
    check(
        "record size limit",
        format!("{:?}", e.record_size_limit),
        format!("{:?}", a.record_size_limit),
    );
    check(
        "delegated credentials",
        format!("{:04x?}", e.delegated_credentials),
        format!("{:04x?}", a.delegated_credentials),
    );
    check(
        "trust anchors",
        format!("{:?}", e.trust_anchors),
        format!("{:?}", a.trust_anchors),
    );
    check(
        "protocol",
        expected.protocol.clone(),
        actual.protocol.clone(),
    );
    match (&expected.http2, &actual.http2) {
        (Some(e2), Some(a2)) => {
            check("akamai", e2.akamai.clone(), a2.akamai.clone());
            check(
                "headers priority",
                format!("{:?}", e2.headers_priority),
                format!("{:?}", a2.headers_priority),
            );
        }
        (e2, a2) => check(
            "http2",
            format!("{}", e2.is_some()),
            format!("{}", a2.is_some()),
        ),
    }
    let names = |c: &Capture| c.headers.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    check(
        "header order",
        format!("{:?}", names(expected)),
        format!("{:?}", names(actual)),
    );
    for ((k, ev), (_, av)) in expected.headers.iter().zip(&actual.headers) {
        if matches!(k.as_str(), ":authority" | ":path") || k.eq_ignore_ascii_case("host") {
            continue;
        }
        if k.eq_ignore_ascii_case("origin") || k.eq_ignore_ascii_case("referer") {
            check(&format!("header {k}"), portless(ev), portless(av));
        } else {
            check(&format!("header {k}"), ev.clone(), av.clone());
        }
    }
    // Captures from before the method and body were kept have neither.
    if !expected.method.is_empty() {
        check("method", expected.method.clone(), actual.method.clone());
        check("body", expected.body.clone(), actual.body.clone());
    }
    out.extend(grease_shape(a));
    out
}

/// Where the GREASE extensions go, counted from the start for the first
/// half and from the end for the second, since browsers that shuffle their
/// extensions keep GREASE first and last and the count varies with the
/// connection (no server_name for an IP address, pre_shared_key when
/// resuming, which is left out here).
fn grease_places(extensions: &[u16]) -> Vec<isize> {
    let fresh: Vec<u16> = extensions
        .iter()
        .copied()
        .filter(|&x| x != PRE_SHARED_KEY)
        .collect();
    let len = fresh.len() as isize;
    (0..len)
        .filter(|&i| is_grease(fresh[i as usize]))
        .map(|i| if i < len / 2 { i } else { i - len })
        .collect()
}

/// What is wrong with the GREASE `hello` sends, going by BoringSSL, whose
/// shapes Chrome's and Safari's hellos both show: the first GREASE
/// extension is empty and the second carries one byte, the two never share
/// a value, and the GREASE key share has a one-byte key. These are rules
/// rather than comparisons with the capture because the committed captures
/// predate the lengths, which are left unchecked when missing.
fn grease_shape(hello: &ClientHello) -> Vec<String> {
    let mut out = Vec::new();
    let values: Vec<u16> = hello
        .extensions
        .iter()
        .copied()
        .filter(|&x| is_grease(x))
        .collect();
    if (1..values.len()).any(|i| values[..i].contains(&values[i])) {
        out.push(format!(
            "GREASE extensions: one value sent twice, {values:04x?}"
        ));
    }
    let lengths = &hello.grease_extension_lengths;
    if !lengths.is_empty() && lengths[..] != [0, 1] {
        out.push(format!(
            "GREASE extension lengths: expected [0, 1], got {lengths:?}"
        ));
    }
    let keys = &hello.grease_key_share_lengths;
    if keys.iter().any(|&n| n != 1) {
        out.push(format!(
            "GREASE key share lengths: expected 1, got {keys:?}"
        ));
    }
    out
}

/// `value` with the port after a host removed: the test server's port
/// changes with every run.
fn portless(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == ':' && chars.peek().is_some_and(char::is_ascii_digit) && !out.ends_with("://") {
            out.pop();
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::client_hello::{ClientHello, is_grease};
    use crate::server::Capture;

    fn chrome() -> Capture {
        let all: Vec<Capture> = serde_json::from_str(include_str!(
            "../../../profiles/chrome-154-macos.capture.json"
        ))
        .unwrap();
        all.into_iter().next().unwrap()
    }

    fn without_grease(v: &mut Vec<u16>) {
        v.retain(|&x| !is_grease(x));
    }

    #[test]
    fn other_grease_values_in_the_same_places_are_the_same_browser() {
        let expected = chrome();
        let mut actual = expected.clone();
        let hello = &mut actual.client_hello;
        for v in [
            &mut hello.ciphers,
            &mut hello.extensions,
            &mut hello.supported_groups,
            &mut hello.key_share_groups,
            &mut hello.signature_algorithms,
            &mut hello.supported_versions,
        ] {
            // Another connection's values: every one changed, and the two
            // GREASE extensions still different from each other.
            for x in v.iter_mut().filter(|x| is_grease(**x)) {
                *x ^= 0x1010;
            }
        }
        let grease: Vec<u16> = (hello.extensions.iter().copied())
            .filter(|&x| is_grease(x))
            .collect();
        assert!(grease.len() == 2 && grease[0] != grease[1]);
        assert_eq!(super::differences(&expected, &actual), Vec::<String>::new());
    }

    fn grease_differences(expected: &Capture, actual: &Capture) -> Vec<String> {
        let mut out = super::differences(expected, actual);
        out.retain(|d| d.starts_with("GREASE"));
        out
    }

    #[test]
    fn grease_is_placed_from_the_nearer_end() {
        // A hello for a name has server_name, one for an IP address
        // doesn't: 19 extensions against 18, GREASE first and last in both.
        let mut with_name = chrome();
        with_name.client_hello.extensions.retain(|&x| x != 0x0029);
        let extensions = &with_name.client_hello.extensions;
        assert_eq!(extensions.len(), 19);
        assert!(is_grease(extensions[0]) && is_grease(extensions[18]));
        let mut without = with_name.clone();
        without.client_hello.extensions.retain(|&x| x != 0x0000);
        assert_eq!(
            grease_differences(&with_name, &without),
            Vec::<String>::new()
        );
        // GREASE one place in from the end is somewhere else.
        let extensions = &mut without.client_hello.extensions;
        extensions.swap(16, 17);
        assert!(!grease_differences(&with_name, &without).is_empty());
    }

    #[test]
    fn grease_must_look_like_boringssls() {
        let expected = chrome();
        let check = |change: fn(&mut ClientHello)| {
            let mut actual = expected.clone();
            change(&mut actual.client_hello);
            grease_differences(&expected, &actual)
        };
        // Lengths a capture didn't keep aren't held against it.
        assert_eq!(check(|_| ()), Vec::<String>::new());
        assert_eq!(
            check(|h| {
                h.grease_extension_lengths = vec![0, 1];
                h.grease_key_share_lengths = vec![1];
            }),
            Vec::<String>::new()
        );
        for lengths in [[1, 0], [0, 0], [1, 1], [0, 2]] {
            let mut actual = expected.clone();
            actual.client_hello.grease_extension_lengths = lengths.to_vec();
            assert_eq!(
                grease_differences(&expected, &actual).len(),
                1,
                "{lengths:?}"
            );
        }
        assert_eq!(check(|h| h.grease_key_share_lengths = vec![32]).len(), 1);
        assert_eq!(check(|h| h.grease_key_share_lengths = vec![0]).len(), 1);
        // Both GREASE extensions with one value.
        assert_eq!(
            check(|h| {
                let first = h.extensions[0];
                let last = h.extensions.iter_mut().rfind(|x| is_grease(**x)).unwrap();
                *last = first;
            })
            .len(),
            1
        );
    }

    #[test]
    fn grease_left_out_anywhere_is_a_difference() {
        let expected = chrome();
        let lists: [fn(&mut Capture) -> &mut Vec<u16>; 6] = [
            |c| &mut c.client_hello.ciphers,
            |c| &mut c.client_hello.extensions,
            |c| &mut c.client_hello.supported_groups,
            |c| &mut c.client_hello.key_share_groups,
            |c| &mut c.client_hello.signature_algorithms,
            |c| &mut c.client_hello.supported_versions,
        ];
        for list in lists {
            let mut actual = expected.clone();
            without_grease(list(&mut actual));
            assert!(!super::differences(&expected, &actual).is_empty());
            // And a browser that sends none mustn't be matched with some.
            let mut plain = expected.clone();
            without_grease(list(&mut plain));
            assert!(!super::differences(&plain, &expected).is_empty());
        }
    }

    #[test]
    fn grease_moved_elsewhere_is_a_difference() {
        let expected = chrome();
        let mut actual = expected.clone();
        let ciphers = &mut actual.client_hello.ciphers;
        let grease = ciphers.remove(0);
        ciphers.push(grease);
        assert!(!super::differences(&expected, &actual).is_empty());
        let mut actual = expected.clone();
        let extensions = &mut actual.client_hello.extensions;
        let last = extensions.pop().unwrap();
        extensions.insert(3, last);
        assert!(!super::differences(&expected, &actual).is_empty());
    }

    #[test]
    fn ports_are_dropped_from_origins_and_referers() {
        assert_eq!(
            super::portless("https://localhost:8443"),
            "https://localhost"
        );
        assert_eq!(
            super::portless("https://127.0.0.1:50211/form?x=1"),
            "https://127.0.0.1/form?x=1"
        );
        assert_eq!(
            super::portless("https://example.com/a:b"),
            "https://example.com/a:b"
        );
    }
}
