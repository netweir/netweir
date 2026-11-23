//! The HTTP/3 recordings in profiles/ parse, and show what the browsers
//! were recorded doing.

use netweir_fingerprint::{Capture, Frame};

fn recording(name: &str) -> Vec<Capture> {
    let path = format!(
        "{}/../../profiles/{name}.h3.capture.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn h3(captures: &[Capture]) -> Vec<&Capture> {
    captures.iter().filter(|c| c.protocol == "h3").collect()
}

fn pseudo(c: &Capture) -> Vec<&str> {
    c.headers
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| k.starts_with(':'))
        .collect()
}

/// Setting ids in order, a GREASE one as 0.
fn setting_ids(c: &Capture) -> Vec<u64> {
    let http3 = c.http3.as_ref().unwrap();
    http3
        .settings
        .iter()
        .map(|s| if s.grease { 0 } else { s.id })
        .collect()
}

fn resumed(c: &Capture) -> bool {
    c.client_hello.extensions.contains(&0x0029)
}

#[test]
fn chrome_157_over_http3() {
    let all = recording("chrome-157-macos");
    let captures = h3(&all);
    assert!(captures.len() >= 10);
    for c in &captures {
        let quic = c.quic.as_ref().unwrap();
        assert_eq!((quic.version, quic.dcid_len, quic.scid_len), (1, 8, 0));
        let sizes: Vec<usize> = quic.datagrams.iter().map(|d| d.size).collect();
        assert_eq!(sizes, [1250, 1250], "two padded Initials");
        // The chaos protector: the hello in many CRYPTO frames, with PING
        // and PADDING among them.
        let frames: Vec<&Frame> = quic
            .datagrams
            .iter()
            .flat_map(|d| &d.packets)
            .flat_map(|p| &p.frames)
            .collect();
        let crypto = frames
            .iter()
            .filter(|f| matches!(f, Frame::Crypto { .. }))
            .count();
        assert!(crypto > 5, "{crypto} CRYPTO frames");
        assert!(frames.iter().any(|f| matches!(f, Frame::Ping)));
        let grease: Vec<_> = quic
            .transport_parameters
            .iter()
            .filter(|p| p.grease)
            .collect();
        assert_eq!(grease.len(), 1);
        assert!(grease[0].value.len() <= 30, "0-15 bytes");
        if !resumed(c) {
            // The value FoxIO's README gives for Chrome over QUIC.
            assert_eq!(c.ja4, "q13d0312h3_55b375c5d22e_178839b6cec1");
        }
        if !c.path.is_empty() {
            assert_eq!(setting_ids(c), [0x01, 0x06, 0x07, 0x33, 0]);
            assert_eq!(pseudo(c), [":method", ":authority", ":scheme", ":path"]);
        }
    }
    // Chrome shuffles its transport parameters on every connection.
    let order = |c: &Capture| -> Vec<u64> {
        let quic = c.quic.as_ref().unwrap();
        quic.transport_parameters.iter().map(|p| p.id).collect()
    };
    assert!(captures.iter().any(|c| order(c) != order(captures[0])));
}

#[test]
fn firefox_160_over_http3() {
    let all = recording("firefox-160-macos");
    // It came over by Alt-Svc: the first navigation went over HTTP/2.
    assert_eq!(all[0].protocol, "h2");
    let captures = h3(&all);
    assert!(captures.len() >= 10);
    let order = |c: &Capture| -> Vec<u64> {
        let quic = c.quic.as_ref().unwrap();
        quic.transport_parameters.iter().map(|p| p.id).collect()
    };
    for c in &captures {
        let quic = c.quic.as_ref().unwrap();
        assert_eq!((quic.version, quic.scid_len), (1, 3));
        // Padded with zeros after the Initial: to 1252 bytes over IPv4,
        // 1232 over IPv6.
        for d in &quic.datagrams {
            assert!(matches!(d.size, 1232 | 1252), "{}", d.size);
            assert_eq!(d.packets.last().unwrap().kind, "zeros");
        }
        // The same parameters in the same order, every connection.
        assert_eq!(order(c), order(captures[0]));
        if !c.path.is_empty() {
            assert_eq!(
                setting_ids(c),
                [0x01, 0x07, 0x2b60_3742, 0xff_d277, 0x33, 0x08]
            );
            assert_eq!(pseudo(c), [":method", ":scheme", ":authority", ":path"]);
        }
    }
}
