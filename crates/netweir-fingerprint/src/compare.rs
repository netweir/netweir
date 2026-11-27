use crate::client_hello::{ClientHello, is_grease};
use crate::h3::{Http3, is_h3_grease};
use crate::qpack::EncoderInstruction;
use crate::quic::{Frame, Quic};
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
/// and its port, and so is the port in `Origin`, `Referer` and `Alt-Used`.
///
/// Over HTTP/3, the first QUIC flight and the HTTP/3 connection are
/// compared too (see `quic_differences` and `http3_differences`), leaving
/// out what a browser picks at random for each connection: the connection
/// IDs, the first packet number, GREASE, and the transport parameters'
/// order, which `transport_parameter_order_differs` compares for a browser
/// that keeps one.
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
    let mut transport = Vec::new();
    match (&expected.quic, &actual.quic) {
        (Some(eq), Some(aq)) => transport.extend(quic_differences(eq, aq)),
        (None, None) => {}
        (eq, aq) => transport.push(format!(
            "quic: expected {}, got {}",
            eq.is_some(),
            aq.is_some()
        )),
    }
    match (&expected.http3, &actual.http3) {
        (Some(e3), Some(a3)) => transport.extend(http3_differences(e3, a3)),
        (None, None) => {}
        (e3, a3) => transport.push(format!(
            "http3: expected {}, got {}",
            e3.is_some(),
            a3.is_some()
        )),
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
        if k.eq_ignore_ascii_case("origin")
            || k.eq_ignore_ascii_case("referer")
            || k.eq_ignore_ascii_case("alt-used")
        {
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
    out.extend(transport);
    out.extend(grease_shape(a));
    out
}

/// How `actual`'s first QUIC flight differs from `expected`'s. Compared:
/// the version, the Source Connection ID's length, the token, every
/// datagram's size, the packets in each and their frames, how the CRYPTO
/// frames cut the ClientHello (which pieces go where, and how long each
/// is), that each packet's number follows the last, and the transport parameters as a set, with their values.
///
/// Left out, as a browser draws them per connection: the Destination
/// Connection ID and its length, the first packet number, the Source
/// Connection ID's bytes (in initial_source_connection_id), GREASE versions
/// in version_information, and a GREASE transport parameter's id and
/// value. A reserved id that is the same in both (Firefox's min_ack_delay,
/// 0xff02de1a) is compared like any other. Where the CRYPTO frames cut the
/// ClientHello is left out too: Firefox cuts it inside the server name at
/// a random point (neqo's SNI slicing).
pub fn quic_differences(expected: &Quic, actual: &Quic) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, a: String, b: String| {
        if a != b {
            out.push(format!("quic {what}: expected {a}, got {b}"));
        }
    };
    check(
        "version",
        format!("{:#x}", expected.version),
        format!("{:#x}", actual.version),
    );
    check(
        "source connection id length",
        expected.scid_len.to_string(),
        actual.scid_len.to_string(),
    );
    check("token", expected.token.clone(), actual.token.clone());
    check(
        "datagrams",
        format!("{:?}", datagram_shapes(expected)),
        format!("{:?}", datagram_shapes(actual)),
    );
    check(
        "packet numbers",
        format!("{:?}", numbering(expected)),
        format!("{:?}", numbering(actual)),
    );
    check(
        "transport parameters",
        format!("{:?}", parameter_set(expected)),
        format!("{:?}", parameter_set(actual)),
    );
    out
}

/// For a browser that sends its transport parameters in a fixed order
/// (Firefox: neqo encodes them in the order of its enum), whether the
/// order matches too. GREASE ids count by place.
pub fn transport_parameter_order_differs(expected: &Quic, actual: &Quic) -> Option<String> {
    let order = |q: &Quic, other: &Quic| -> Vec<String> {
        q.transport_parameters
            .iter()
            .map(|p| {
                if p.grease && !other.transport_parameters.iter().any(|o| o.id == p.id) {
                    "GREASE".to_string()
                } else {
                    format!("{:#x}", p.id)
                }
            })
            .collect()
    };
    let (e, a) = (order(expected, actual), order(actual, expected));
    (e != a).then(|| format!("transport parameter order: expected {e:?}, got {a:?}"))
}

/// Each datagram as its size and its packets: an Initial as its frames,
/// a CRYPTO frame as which piece of the ClientHello it holds ("start" at
/// offset 0, "end" reaching the last byte, "middle" otherwise) and its
/// length unless it holds the cut piece around the server name; padding
/// after the packets as "zeros".
fn datagram_shapes(q: &Quic) -> Vec<(usize, Vec<String>)> {
    let total = q
        .datagrams
        .iter()
        .flat_map(|d| &d.packets)
        .flat_map(|p| &p.frames)
        .filter_map(|f| match f {
            Frame::Crypto { offset, length } => Some(offset + *length as u64),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    q.datagrams
        .iter()
        .map(|d| {
            let packets = d
                .packets
                .iter()
                .map(|p| {
                    let frames: Vec<String> = p
                        .frames
                        .iter()
                        .map(|f| match f {
                            Frame::Crypto { offset, length } => {
                                let place = if *offset == 0 {
                                    "start"
                                } else if offset + *length as u64 == total {
                                    "end"
                                } else {
                                    "middle"
                                };
                                if place == "middle" {
                                    format!("crypto {place} {length}")
                                } else {
                                    format!("crypto {place}")
                                }
                            }
                            Frame::Padding { length } => format!("padding {length}"),
                            other => format!("{other:?}"),
                        })
                        .collect();
                    if frames.is_empty() {
                        p.kind.clone()
                    } else {
                        format!("{} [{}]", p.kind, frames.join(", "))
                    }
                })
                .collect();
            (d.size, packets)
        })
        .chain(std::iter::once((
            total as usize,
            vec!["ClientHello bytes".to_string()],
        )))
        .collect()
}

/// Whether each Initial's packet number is the one after the last. (How
/// many bytes encode it follows from the number, which is random.)
fn numbering(q: &Quic) -> Vec<&'static str> {
    let mut last: Option<u64> = None;
    let mut out = Vec::new();
    for p in q.datagrams.iter().flat_map(|d| &d.packets) {
        if let Some(n) = p.packet_number {
            let follows = last.is_none_or(|l| n == l + 1);
            out.push(if follows { "next" } else { "out of order" });
            last = Some(n);
        }
    }
    out
}

/// The transport parameters sorted by id, each with its value, less what
/// changes per connection.
fn parameter_set(q: &Quic) -> Vec<(String, String)> {
    const INITIAL_SCID: u64 = 0x0f;
    const VERSION_INFORMATION: u64 = 0x11;
    let mut set: Vec<(String, String)> = q
        .transport_parameters
        .iter()
        .map(|p| match p.id {
            INITIAL_SCID => (
                format!("{:#x}", p.id),
                format!("{} bytes", p.value.len() / 2),
            ),
            VERSION_INFORMATION => (format!("{:#x}", p.id), versions(&p.value)),
            // Which reserved id, and what it holds, is drawn per
            // connection; Firefox's min_ack_delay is the one fixed id.
            id if p.grease && id != 0xff02_de1a => ("GREASE".to_string(), String::new()),
            id => (format!("{id:#x}"), p.value.clone()),
        })
        .collect();
    set.sort();
    set
}

/// version_information's versions in order (chosen first), a reserved
/// one (RFC 9000 §15: 0x?a?a?a?a) as "GREASE".
fn versions(hex: &str) -> String {
    let words: Vec<String> = (0..hex.len() / 8)
        .map(|i| {
            let v = u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).unwrap_or(0);
            if v & 0x0f0f_0f0f == 0x0a0a_0a0a {
                "GREASE".to_string()
            } else {
                format!("{v:#x}")
            }
        })
        .collect();
    words.join(" ")
}

/// How `actual`'s HTTP/3 connection differs from `expected`'s: the
/// unidirectional streams it opened, SETTINGS in order, and the frames on
/// its control stream and on the request's stream. A GREASE setting, frame
/// or stream type counts by place; which reserved value it is, and a
/// GREASE frame's payload, are drawn per connection.
pub fn http3_differences(expected: &Http3, actual: &Http3) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, a: String, b: String| {
        if a != b {
            out.push(format!("http3 {what}: expected {a}, got {b}"));
        }
    };
    let grease = |v: u64| {
        if is_h3_grease(v) {
            "GREASE".to_string()
        } else {
            format!("{v:#x}")
        }
    };
    let streams = |h: &Http3| -> Vec<(u64, String)> {
        h.streams
            .iter()
            .map(|&(id, kind)| (id, grease(kind)))
            .collect()
    };
    check(
        "streams",
        format!("{:?}", streams(expected)),
        format!("{:?}", streams(actual)),
    );
    let settings = |h: &Http3| -> Vec<String> {
        h.settings
            .iter()
            .map(|s| {
                if s.grease {
                    "GREASE".to_string()
                } else {
                    format!("{:#x}={}", s.id, s.value)
                }
            })
            .collect()
    };
    check(
        "settings",
        format!("{:?}", settings(expected)),
        format!("{:?}", settings(actual)),
    );
    let frames = |f: &[(u64, String)]| -> Vec<String> {
        f.iter()
            .map(|(kind, payload)| {
                if is_h3_grease(*kind) {
                    "GREASE".to_string()
                } else {
                    format!("{kind:#x} {payload}")
                }
            })
            .collect()
    };
    check(
        "control stream frames",
        format!("{:?}", frames(&expected.control_frames)),
        format!("{:?}", frames(&actual.control_frames)),
    );
    check(
        "request stream frames",
        format!("{:?}", frames(&expected.request_frames)),
        format!("{:?}", frames(&actual.request_frames)),
    );
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

/// How a request used the QPACK dynamic table its server offered, with
/// the values left out, as they name the server and its port: the
/// entries from absolute index `from` up to its Required Insert Count, as
/// the encoder stream inserted them, then its Required Insert Count and
/// Base and each field line's representation, name and Huffman flags.
/// Those entries are the ones the server waited for before it read the
/// section, so they're in whatever the timing; inserts nothing referred
/// to yet show up with the request that first does. Dynamic indices,
/// Required Insert Count and Base go in only with `indices`: a comparison
/// leaves them out after a request it couldn't make, as one that filled
/// the table would have moved them.
pub fn qpack_shape(http3: &Http3, from: u64, indices: bool) -> Vec<String> {
    let mut out = Vec::new();
    let section = http3.field_section.as_ref();
    let to = section.map_or(0, |s| s.required_insert_count);
    let inserts = http3
        .encoder
        .iter()
        .filter(|i| !matches!(i, EncoderInstruction::SetCapacity { .. }));
    for instruction in inserts
        .skip(from as usize)
        .take(to.saturating_sub(from) as usize)
    {
        out.push(match instruction {
            EncoderInstruction::InsertWithNameReference {
                static_table,
                index,
                name,
                huffman,
                ..
            } => {
                let table = if *static_table { "static" } else { "dynamic" };
                let index = if *static_table || indices {
                    index.to_string()
                } else {
                    "_".into()
                };
                format!("insert {name} by {table} name {index}, huffman {huffman}")
            }
            EncoderInstruction::InsertWithLiteralName {
                name,
                huffman_name,
                huffman_value,
                ..
            } => format!("insert {name} literally, huffman {huffman_name}/{huffman_value}"),
            EncoderInstruction::Duplicate { index, name, .. } => {
                let index = if indices {
                    index.to_string()
                } else {
                    "_".into()
                };
                format!("duplicate {name} {index}")
            }
            EncoderInstruction::SetCapacity { .. } => unreachable!("filtered out"),
        });
    }
    let Some(section) = section else {
        out.push("no field section".into());
        return out;
    };
    let (ric, base) = (section.required_insert_count, section.base);
    out.push(if indices {
        format!("required insert count {ric}, base {base}")
    } else {
        format!(
            "refers to the table {}, past its base {}",
            ric > 0,
            base < ric
        )
    });
    for line in &section.lines {
        let dynamic = matches!(
            line.kind.as_str(),
            "dynamic" | "post_base" | "dynamic_name" | "post_base_name"
        );
        let index = match line.index {
            Some(i) if indices || !dynamic => format!(" {i}"),
            Some(_) => " _".into(),
            None => String::new(),
        };
        out.push(format!(
            "{}{index} {}, huffman {:?}/{:?}{}",
            line.kind,
            line.name,
            line.huffman_name,
            line.huffman_value,
            if line.never_indexed {
                ", never indexed"
            } else {
                ""
            }
        ));
    }
    out
}

/// How the requests at `paths` in `actual` used the server's QPACK
/// table differently from those in `recorded` (see [`qpack_shape`]),
/// each list in the order it was sent. The entries a request inserted
/// are counted from the most any earlier request on its connection
/// referred to. The first `exact` paths are compared with their indices.
pub fn qpack_differences(
    recorded: &[Capture],
    actual: &[Capture],
    paths: &[&str],
    exact: usize,
) -> Vec<String> {
    /// Each path's capture over HTTP/3 and where its inserts start.
    fn shapes<'a>(captures: &'a [Capture], paths: &[&str]) -> Vec<Option<(u64, &'a Http3)>> {
        let mut high = std::collections::HashMap::<u64, u64>::new();
        let mut out = vec![None; paths.len()];
        for c in captures {
            let Some(http3) = c.http3.as_ref().filter(|_| c.protocol == "h3") else {
                continue;
            };
            let from = high.entry(c.connection).or_default();
            if let Some(i) = paths.iter().position(|p| *p == c.path) {
                out[i] = Some((*from, http3));
            }
            let ric = http3
                .field_section
                .as_ref()
                .map_or(0, |s| s.required_insert_count);
            *from = (*from).max(ric);
        }
        out
    }
    let (want, got) = (shapes(recorded, paths), shapes(actual, paths));
    let mut out = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        match (want[i], got[i]) {
            (Some((wf, w)), Some((gf, g))) => {
                let (w, g) = (qpack_shape(w, wf, i < exact), qpack_shape(g, gf, i < exact));
                if w != g {
                    out.push(format!("{path}:\n  expected {w:#?}\n  got {g:#?}"));
                }
            }
            (None, _) => out.push(format!("{path}: not over HTTP/3 in the recording")),
            (_, None) => out.push(format!("{path}: not sent over HTTP/3")),
        }
    }
    out
}

/// The capacity a QPACK encoder set the server's table to, if its
/// instructions so far set one.
pub fn qpack_capacity(http3: &Http3) -> Option<u64> {
    http3.encoder.iter().find_map(|i| match i {
        EncoderInstruction::SetCapacity { capacity } => Some(*capacity),
        _ => None,
    })
}

/// How many entries a QPACK encoder's instructions so far inserted.
pub fn qpack_inserts(http3: &Http3) -> usize {
    http3
        .encoder
        .iter()
        .filter(|i| !matches!(i, EncoderInstruction::SetCapacity { .. }))
        .count()
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

    fn firefox_over_http3(connection: u64, path: &str) -> Capture {
        let all: Vec<Capture> = serde_json::from_str(include_str!(
            "../../../profiles/firefox-160-macos.h3.capture.json"
        ))
        .unwrap();
        all.into_iter()
            .find(|c| c.connection == connection && c.path == path)
            .unwrap()
    }

    #[test]
    fn two_firefox_connections_differ_only_in_what_is_random() {
        // Connections 1 and 7 of the recording: other connection IDs (13
        // and 8 bytes), packet numbers, CRYPTO cut points, extension order,
        // version_information GREASE and control stream GREASE frame.
        let (one, other) = (
            firefox_over_http3(1, "/image"),
            firefox_over_http3(7, "/image"),
        );
        assert_ne!(one.quic, other.quic);
        assert_ne!(one.http3, other.http3);
        assert_eq!(super::differences(&one, &other), Vec::<String>::new());
        let (q1, q7) = (one.quic.as_ref().unwrap(), other.quic.as_ref().unwrap());
        assert_eq!(super::transport_parameter_order_differs(q1, q7), None);
    }

    #[test]
    fn what_a_firefox_connection_keeps_is_compared() {
        let expected = firefox_over_http3(1, "/image");
        let changed = |change: fn(&mut Capture)| {
            let mut actual = expected.clone();
            change(&mut actual);
            super::differences(&expected, &actual)
        };
        // A transport parameter's value; one left out.
        assert_eq!(
            changed(|c| c.quic.as_mut().unwrap().transport_parameters[0].value = "80003a98".into())
                .len(),
            1
        );
        assert_eq!(
            changed(|c| {
                c.quic.as_mut().unwrap().transport_parameters.remove(11);
            })
            .len(),
            1
        );
        // One datagram, not two; no padding after the Initial.
        assert!(
            !changed(|c| {
                c.quic.as_mut().unwrap().datagrams.pop();
            })
            .is_empty()
        );
        assert!(
            !changed(|c| {
                c.quic.as_mut().unwrap().datagrams[0].packets.pop();
            })
            .is_empty()
        );
        // The ClientHello in one CRYPTO frame.
        assert!(
            !changed(|c| {
                let frames = &mut c.quic.as_mut().unwrap().datagrams[0].packets[0].frames;
                frames.swap(0, 1);
            })
            .is_empty()
        );
        // An empty Source Connection ID, as Chrome's.
        assert_eq!(changed(|c| c.quic.as_mut().unwrap().scid_len = 0).len(), 1);
        // SETTINGS in another order, or a stream not opened.
        assert_eq!(
            changed(|c| c.http3.as_mut().unwrap().settings.swap(0, 1)).len(),
            1
        );
        assert_eq!(
            changed(|c| {
                c.http3.as_mut().unwrap().streams.pop();
            })
            .len(),
            1
        );
        // No GREASE frame on the control stream.
        assert_eq!(
            changed(|c| c.http3.as_mut().unwrap().control_frames.clear()).len(),
            1
        );
        // The parameters in another order: the same set, but not Firefox's.
        let mut actual = expected.clone();
        actual
            .quic
            .as_mut()
            .unwrap()
            .transport_parameters
            .swap(0, 1);
        assert_eq!(super::differences(&expected, &actual), Vec::<String>::new());
        let (e, a) = (
            expected.quic.as_ref().unwrap(),
            actual.quic.as_ref().unwrap(),
        );
        assert!(super::transport_parameter_order_differs(e, a).is_some());
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
