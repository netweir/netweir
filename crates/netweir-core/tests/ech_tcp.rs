//! A request over TCP with ECH from an HTTPS record must send Firefox's
//! ClientHellos: profiles/firefox-160-macos.ech.capture.json recorded
//! Firefox 160.0.1's outer and inner ones (examples/ech_capture.rs holds
//! the ECH key, so the inner one is decrypted). Netweir's are recorded
//! the same way here and held to it: every byte the same but the randoms,
//! the key shares and the encrypted payload.

#![cfg(feature = "http3")]

mod ech_open;

use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use ech_open::{ECH, handshake, open_inner, parts, read_hello, unhex, with_extensions};
use netweir_core::{FetchOptions, Fetcher, Profile};

const KEY_SHARE: u16 = 0x0033;
/// The recorded configurations' public name (examples/ech_capture.rs).
const PUBLIC_NAME: &str = "public.test";

/// One recorded connection: the ECHConfigList Firefox was given, and
/// the outer and (encoded) inner ClientHellos it sent with it.
struct Recording {
    maximum_name_length: u64,
    config_list: Vec<u8>,
    outer: Vec<u8>,
    inner: Vec<u8>,
}

/// The ECH private key, and each recording.
fn recordings() -> (Vec<u8>, Vec<Recording>) {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../profiles/firefox-160-macos.ech.capture.json");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let field = |v: &serde_json::Value, name: &str| unhex(v[name].as_str().unwrap());
    let recordings = json["recordings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| Recording {
            maximum_name_length: r["maximum_name_length"].as_u64().unwrap(),
            config_list: field(r, "ech_config_list"),
            outer: field(r, "outer"),
            inner: field(r, "inner"),
        })
        .collect();
    (field(&json, "ech_private_key"), recordings)
}

/// A ClientHello handshake message with its random zeroed.
fn without_random(hello: &[u8]) -> Vec<u8> {
    let mut out = hello.to_vec();
    out[6..38].fill(0);
    out
}

/// An outer ClientHello with what's drawn for each connection taken out:
/// its random and session ID zeroed, each key share's key zeroed, and the
/// encrypted_client_hello extension's enc and payload zeroed (their
/// lengths stay).
fn outer_shape(outer: &[u8]) -> Vec<u8> {
    let body = &outer[4..];
    let mut p = parts(body).unwrap();
    p.head[2..34].fill(0);
    let session = p.head[34] as usize;
    p.head[35..35 + session].fill(0);
    for (kind, ext) in &mut p.extensions {
        match *kind {
            KEY_SHARE => {
                let mut at = 2;
                while at + 4 <= ext.len() {
                    let len = u16::from_be_bytes([ext[at + 2], ext[at + 3]]) as usize;
                    ext[at + 4..at + 4 + len].fill(0);
                    at += 4 + len;
                }
            }
            ECH => ext[8..].iter_mut().for_each(|b| *b = 0),
            _ => {}
        }
    }
    handshake(with_extensions(&p.head, &p.extensions))
}

/// The first ClientHello a Fetcher with `profile` sends to a record's
/// target with the recorded ECH configuration.
fn netweir_hello(profile: &str, config_list: &[u8]) -> Vec<u8> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, hellos) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if let Some(hello) = read_hello(&mut stream) {
                let _ = tx.send(hello);
            }
            // handshake_failure, so the client doesn't wait.
            let _ = std::io::Write::write_all(&mut stream, &[21, 3, 3, 0, 2, 2, 40]);
        }
    });
    // The record names its target, so the port prefix of its owner
    // isn't taken for it.
    let record = netweir_core::https_answer(
        &format!("_{port}._https.origin.test"),
        &[(
            1,
            "origin.test",
            &[
                (1, b"\x02h2\x08http/1.1".to_vec()),
                (5, config_list.to_vec()),
            ],
        )],
        300,
    );
    let mut options = FetchOptions::new(Profile::named(profile).unwrap());
    options.verify_certificates = false;
    options.resolve = vec![("origin.test".into(), ([127, 0, 0, 1], port).into())];
    options
        .https_records
        .insert(format!("_{port}._https.origin.test"), record);
    let fetcher = Fetcher::new(options).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // The first request may go before the records are in, with ECH GREASE.
    for _ in 0..20 {
        let _ = runtime.block_on(fetcher.get(&format!("https://origin.test:{port}/"), &[]));
        while let Ok(hello) = hellos.recv_timeout(Duration::from_millis(200)) {
            // GREASE ECH draws its configuration ID at random, so it's
            // the outer server_name, the configuration's public name, that
            // tells the two apart.
            let p = parts(&hello[4..]).unwrap();
            let name = p.extensions.iter().find(|(k, _)| *k == 0).unwrap();
            if name.1.ends_with(PUBLIC_NAME.as_bytes()) {
                return hello;
            }
        }
    }
    panic!("no ClientHello with the record's ECH");
}

#[test]
fn the_ech_client_hellos_over_tcp_are_firefoxs() {
    let (private, recordings) = recordings();
    for firefox in recordings {
        // The configuration's cipher suites, after its 32-byte X25519 key.
        let list = &firefox.config_list;
        let suites = u16::from_be_bytes([list[43], list[44]]) as usize;
        let what = format!(
            "maximum_name_length {}, cipher suites {}",
            firefox.maximum_name_length,
            ech_open::hex(&list[45..45 + suites])
        );
        let config = &firefox.config_list[2..];
        let ours = netweir_hello("firefox-160-macos", &firefox.config_list);
        assert_eq!(
            outer_shape(&ours),
            outer_shape(&firefox.outer),
            "{what}: the outer ClientHello differs from Firefox's"
        );
        let (inner, _) = open_inner(&private, config, &ours).expect("our ECH opens");
        let (recorded, _) =
            open_inner(&private, config, &firefox.outer).expect("Firefox's ECH opens");
        assert_eq!(
            recorded, firefox.inner,
            "{what}: the recording opens to itself"
        );
        // The encoded ClientHelloInner, padding and all: the
        // encrypted_client_hello marker first, the outer hello's cipher
        // suites, and every extension but server_name and
        // supported_versions in ech_outer_extensions, in the outer
        // hello's order.
        assert_eq!(
            without_random(&inner),
            without_random(&firefox.inner),
            "{what}: the ClientHelloInner differs from Firefox's"
        );
    }
}
