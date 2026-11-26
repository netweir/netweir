//! A request over TCP with ECH from an HTTPS record must send Firefox's
//! ClientHellos: profiles/firefox-160-macos.ech.capture.json recorded
//! Firefox 160.0.1's outer and inner ones (examples/ech_capture.rs holds
//! the ECH key, so the inner one is decrypted), and its ECH GREASE.
//! Netweir's are recorded the same way here and held to it: every byte the
//! same but what's drawn for each connection. Then a server holding the
//! key must accept ECH and finish the handshake, with a HelloRetryRequest
//! too.

#![cfg(feature = "http3")]

mod ech_open;

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use ech_open::{
    ECH, OUTER_EXTENSIONS, decompress, handshake, open_inner, parts, read_hello, unhex,
    with_extensions,
};
use netweir_core::{FetchOptions, Fetcher, Profile};

const SERVER_NAME: u16 = 0x0000;
const SUPPORTED_VERSIONS: u16 = 0x002b;
const KEY_SHARE: u16 = 0x0033;
const PRE_SHARED_KEY: u16 = 0x0029;
/// What NSS leaves out of a ClientHelloInner, which offers TLS 1.3 only:
/// extended_master_secret, renegotiation_info, ec_point_formats and
/// session_ticket.
const TLS12_ONLY: [u16; 4] = [0x0017, 0xff01, 0x000b, 0x0023];
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

/// What profiles/firefox-160-macos.ech.capture.json holds.
struct Recorded {
    private: Vec<u8>,
    recordings: Vec<Recording>,
    /// Each host's first ClientHello with ECH GREASE.
    grease: Vec<(String, Vec<u8>)>,
}

fn recorded() -> Recorded {
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
    let grease = json["grease"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["host"].as_str().unwrap().to_string(), field(g, "outer")))
        .collect();
    Recorded {
        private: field(&json, "ech_private_key"),
        recordings,
        grease,
    }
}

/// A ClientHello handshake message with its random zeroed.
fn without_random(hello: &[u8]) -> Vec<u8> {
    let mut out = hello.to_vec();
    out[6..38].fill(0);
    out
}

/// A ClientHello with what's drawn for each connection taken out: its
/// random and session ID zeroed, each key share's key zeroed, and the
/// encrypted_client_hello extension's enc and payload zeroed (their
/// lengths stay). With `grease`, its AEAD and configuration ID too, which
/// ECH GREASE draws at random.
fn shape(hello: &[u8], grease: bool) -> Vec<u8> {
    let mut p = parts(&hello[4..]).unwrap();
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
            // An outer one: type, suite, configuration ID, enc, payload.
            ECH if ext.len() > 1 => {
                ext[8..].fill(0);
                if grease {
                    ext[3..6].fill(0);
                }
            }
            _ => {}
        }
    }
    handshake(with_extensions(&p.head, &p.extensions))
}

/// A TCP port on 127.0.0.1 that writes down each connection's first
/// ClientHello and answers it with handshake_failure.
fn hello_catcher() -> (u16, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, hellos) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if let Some(hello) = read_hello(&mut stream) {
                let _ = tx.send(hello);
            }
            let _ = stream.write_all(&[21, 3, 3, 0, 2, 2, 40]);
        }
    });
    (port, hellos)
}

/// A Fetcher with `profile` whose `host` is 127.0.0.1 at `port`, with an
/// HTTPS record offering h2 and http/1.1 with `config_list` if there is
/// one. The record names its target, so the port prefix of its owner isn't
/// taken for it.
fn fetcher(profile: &str, host: &str, port: u16, config_list: Option<&[u8]>) -> Fetcher {
    let mut options = FetchOptions::new(Profile::named(profile).unwrap());
    options.verify_certificates = false;
    options.resolve = vec![(host.into(), ([127, 0, 0, 1], port).into())];
    if let Some(list) = config_list {
        let owner = format!("_{port}._https.{host}");
        let record = netweir_core::https_answer(
            &owner,
            &[(
                1,
                host,
                &[(1, b"\x02h2\x08http/1.1".to_vec()), (5, list.to_vec())],
            )],
            300,
        );
        options.https_records.insert(owner, record);
    }
    Fetcher::new(options).unwrap()
}

/// Whether `hello` carries a real ECH, named for the configuration's
/// public name, rather than ECH GREASE (whose configuration ID is drawn at
/// random, so only the outer server_name tells the two apart).
fn real_ech(hello: &[u8]) -> bool {
    let p = parts(&hello[4..]).unwrap();
    let name = p
        .extensions
        .iter()
        .find(|(k, _)| *k == SERVER_NAME)
        .unwrap();
    name.1.ends_with(PUBLIC_NAME.as_bytes())
}

/// The first ClientHello with the record's ECH that a Fetcher with
/// `profile` sends. The first request may go before the record is in, with
/// ECH GREASE.
fn netweir_hello(profile: &str, config_list: &[u8]) -> Vec<u8> {
    let (port, hellos) = hello_catcher();
    let fetcher = fetcher(profile, "origin.test", port, Some(config_list));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for _ in 0..20 {
        let _ = runtime.block_on(fetcher.get(&format!("https://origin.test:{port}/"), &[]));
        while let Ok(hello) = hellos.recv_timeout(Duration::from_millis(200)) {
            if real_ech(&hello) {
                return hello;
            }
        }
    }
    panic!("no ClientHello with the record's ECH");
}

#[test]
fn the_ech_client_hellos_over_tcp_are_firefoxs() {
    let recorded = recorded();
    let private = &recorded.private;
    for firefox in recorded.recordings {
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
            shape(&ours, false),
            shape(&firefox.outer, false),
            "{what}: the outer ClientHello differs from Firefox's"
        );
        let (inner, decoded) = open_inner(private, config, &ours).expect("our ECH opens");
        let (recorded, recorded_decoded) =
            open_inner(private, config, &firefox.outer).expect("Firefox's ECH opens");
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
        // And the ClientHelloInner the server reads from it.
        assert_eq!(
            shape(&decoded, false),
            shape(&recorded_decoded, false),
            "{what}: the decoded ClientHelloInner differs from Firefox's"
        );
        let p = parts(&decoded[4..]).unwrap();
        let name = p
            .extensions
            .iter()
            .find(|(k, _)| *k == SERVER_NAME)
            .unwrap();
        assert!(
            name.1.ends_with(b"origin.test"),
            "{what}: inner server_name"
        );
    }
}

#[test]
fn ech_outer_extensions_must_follow_the_outer_order() {
    let outer: Vec<(u16, Vec<u8>)> = [0x000a, 0x0010, 0x0033, ECH]
        .iter()
        .map(|&k| (k, vec![k as u8]))
        .collect();
    let names = |kinds: &[u16]| {
        let mut ext = vec![(kinds.len() * 2) as u8];
        for k in kinds {
            ext.extend(k.to_be_bytes());
        }
        vec![(OUTER_EXTENSIONS, ext)]
    };
    let decoded = decompress(&names(&[0x000a, 0x0033]), &outer).unwrap();
    assert_eq!(decoded, [outer[0].clone(), outer[2].clone()]);
    assert_eq!(decompress(&names(&[0x0033, 0x000a]), &outer), None);
    assert_eq!(decompress(&names(&[0x000a, 0x000a]), &outer), None);
    assert_eq!(decompress(&names(&[ECH]), &outer), None);
    assert_eq!(decompress(&names(&[0x0005]), &outer), None);
}

/// Every Firefox profile that asks for an NSS ClientHelloInner.
fn nss_inner_profiles() -> Vec<String> {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            let profile = name.strip_suffix(".toml")?.to_string();
            let text = std::fs::read_to_string(dir.join(&name)).unwrap();
            text.contains("\nech_inner_like_nss = true")
                .then_some(profile)
        })
        .collect();
    names.sort();
    names
}

/// The plaintext length NSS gives the GREASE ECH payload of `outer`, a
/// ClientHelloOuter to `host` (tls13_MaybeGreaseEch, lib/ssl/tls13ech.c
/// in NSS 3.132): the EncodedClientHelloInner it would have, padded by
/// tls13_PadChInner for a maximum_name_length of 100. On a resumption,
/// NSS copies the outer hello's pre_shared_key into the inner one it
/// measures.
fn nss_grease_plaintext_len(outer: &[u8], host: &str) -> usize {
    let o = parts(&outer[4..]).unwrap();
    let suites = &o.head[35 + o.head[34] as usize..];
    let compressed = o
        .extensions
        .iter()
        .filter(|(k, _)| {
            ![SERVER_NAME, SUPPORTED_VERSIONS, ECH, PRE_SHARED_KEY].contains(k)
                && !TLS12_ONLY.contains(k)
        })
        .count();
    // Version, random, an empty session ID, the cipher suites and
    // compression methods; then the extensions: the inner
    // encrypted_client_hello, server_name, supported_versions (TLS 1.3)
    // and ech_outer_extensions.
    let len = 2
        + 32
        + 1
        + suites.len()
        + 2
        + (4 + 1)
        + (4 + 5 + host.len())
        + (4 + 3)
        + (4 + 1 + 2 * compressed);
    let psk = o
        .extensions
        .iter()
        .find(|(k, _)| *k == PRE_SHARED_KEY)
        .map_or(0, |(_, e)| 4 + e.len());
    let len = len + psk;
    let padding = 100usize.saturating_sub(host.len());
    let rounding = 31 - ((len + padding - 1) % 32);
    len + padding + rounding
}

#[test]
fn every_profile_that_asks_sends_nss_ech_grease() {
    // Only firefox-160-macos has HTTP/3 and follows HTTPS records, so the
    // older Firefox profiles never send a real ECH; what they all send is
    // ECH GREASE, and it has to be NSS's for each of them.
    let profiles = nss_inner_profiles();
    assert!(
        profiles.len() >= 4 && profiles.iter().all(|p| p.starts_with("firefox-")),
        "{profiles:?}"
    );
    let recorded = recorded();
    let (host, firefox) = &recorded.grease[0];
    let ech_payload = |hello: &[u8]| {
        let p = parts(&hello[4..]).unwrap();
        let ech = p
            .extensions
            .iter()
            .find(|(k, _)| *k == ECH)
            .unwrap()
            .1
            .clone();
        let enc = u16::from_be_bytes([ech[6], ech[7]]) as usize;
        (ech[1..5].to_vec(), ech.len() - 10 - enc)
    };
    // The formula gives Firefox's own recording.
    assert_eq!(
        ech_payload(firefox).1,
        nss_grease_plaintext_len(firefox, host) + 16
    );
    for profile in profiles {
        let text = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../profiles/{profile}.toml")),
        )
        .unwrap();
        assert!(
            text.contains("\naes_hw_override = true"),
            "{profile}: aes_hw_override"
        );
        let (port, hellos) = hello_catcher();
        let fetcher = fetcher(&profile, host, port, None);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _ = runtime.block_on(fetcher.get(&format!("https://{host}:{port}/"), &[]));
        let ours = hellos.recv_timeout(Duration::from_secs(5)).unwrap();
        let (suite, payload) = ech_payload(&ours);
        assert!(
            suite == [0, 1, 0, 1] || suite == [0, 1, 0, 3],
            "{profile}: GREASE suite {suite:?}"
        );
        assert_eq!(
            payload,
            nss_grease_plaintext_len(&ours, host) + 16,
            "{profile}: GREASE payload length"
        );
    }
}

#[test]
fn ech_grease_is_firefoxs() {
    // NSS's GREASE ECH (tls13_MaybeGreaseEch, lib/ssl/tls13ech.c in NSS
    // 3.132): HKDF-SHA256 (line 2251) with AES-128-GCM or
    // ChaCha20-Poly1305 by a random bit (line 2257), a random configuration
    // ID and enc, and a payload as long as the EncodedClientHelloInner
    // padded for a maximum_name_length of 100 (line 2194, greaseEchSize).
    let recorded = recorded();
    for (host, firefox) in &recorded.grease {
        let (port, hellos) = hello_catcher();
        let fetcher = fetcher("firefox-160-macos", host, port, None);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut aeads = BTreeSet::new();
        for _ in 0..24 {
            let _ = runtime.block_on(fetcher.get(&format!("https://{host}:{port}/"), &[]));
            let ours = hellos.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(
                shape(&ours, true),
                shape(firefox, true),
                "{host}: the ClientHello with ECH GREASE differs from Firefox's"
            );
            let p = parts(&ours[4..]).unwrap();
            let ech = &p.extensions.iter().find(|(k, _)| *k == ECH).unwrap().1;
            assert_eq!(&ech[..3], [0, 0, 1], "{host}: HKDF-SHA256");
            aeads.insert(u16::from_be_bytes([ech[3], ech[4]]));
        }
        // The chance both don't turn up in 24 draws is 2^-23.
        assert_eq!(aeads, BTreeSet::from([1, 3]), "{host}: the AEADs drawn");
    }
}

/// What a server holding the ECH key saw of a handshake that finished.
#[derive(Debug)]
struct Accepted {
    ech: bool,
    cipher: String,
    group: Option<String>,
    /// The ClientHelloInner it read, as BoringSSL hands it to the
    /// certificate callback, without the handshake header.
    hello: Vec<u8>,
}

/// A TLS server on 127.0.0.1 with the ECH key and `config`, offering only
/// `groups` if given, which answers one HTTP/1.1 request per connection
/// and tells of each handshake that finished.
fn ech_server(
    private: &[u8],
    config: &[u8],
    groups: Option<&str>,
) -> (u16, mpsc::Receiver<Accepted>) {
    use btls::ssl::{AlpnError, SslAcceptor, SslEchKeys, SslMethod};
    let made = rcgen::generate_simple_self_signed(vec!["origin.test".to_string()]).unwrap();
    let cert = btls::x509::X509::from_der(made.cert.der()).unwrap();
    let key = btls::pkey::PKey::private_key_from_der(&made.signing_key.serialize_der()).unwrap();
    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    builder.set_certificate(&cert).unwrap();
    builder.set_private_key(&key).unwrap();
    let mut keys = SslEchKeys::builder().unwrap();
    keys.add_key(
        true,
        config,
        btls::hpke::HpkeKey::dhkem_p256_sha256(private).unwrap(),
    )
    .unwrap();
    builder.set_ech_keys(&keys.build()).unwrap();
    if let Some(groups) = groups {
        builder.set_curves_list(groups).unwrap();
    }
    builder.set_alpn_select_callback(|_, client| {
        btls::ssl::select_next_proto(b"\x08http/1.1", client).ok_or(AlpnError::NOACK)
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hellos = seen.clone();
    builder.set_select_certificate_callback(move |hello| {
        *hellos.lock().unwrap() = hello.as_bytes().to_vec();
        Ok(())
    });
    let acceptor = builder.build();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, accepted) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let Ok(mut tls) = acceptor.accept(stream) else {
                continue;
            };
            let mut request: Vec<u8> = Vec::new();
            let mut buf = [0; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match tls.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend(&buf[..n]),
                }
            }
            let _ = tls
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            let ssl = tls.ssl();
            let _ = tx.send(Accepted {
                ech: ssl.ech_accepted(),
                cipher: ssl
                    .current_cipher()
                    .map(|c| c.standard_name().unwrap_or(c.name()).to_string())
                    .unwrap_or_default(),
                group: ssl.curve_name().map(str::to_string),
                hello: seen.lock().unwrap().clone(),
            });
            let _ = tls.shutdown();
        }
    });
    (port, accepted)
}

/// The first handshake with ECH accepted that a request with the
/// recorded configuration makes to `server`.
fn accepted_handshake(
    port: u16,
    config_list: &[u8],
    accepted: &mpsc::Receiver<Accepted>,
) -> Accepted {
    let fetcher = fetcher("firefox-160-macos", "origin.test", port, Some(config_list));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for _ in 0..20 {
        let answer = runtime.block_on(fetcher.get(&format!("https://origin.test:{port}/"), &[]));
        let answer = answer.expect("the request is answered");
        assert_eq!(answer.status, 200);
        while let Ok(seen) = accepted.recv_timeout(Duration::from_millis(200)) {
            if seen.ech {
                return seen;
            }
        }
    }
    panic!("ECH was never accepted");
}

#[test]
fn a_server_with_the_key_accepts_ech_and_the_handshake_finishes() {
    // The ClientHelloInner's compressed extensions, delegated_credentials
    // and record_size_limit among them, have to decode on the server.
    let recorded = recorded();
    let firefox = &recorded.recordings[0];
    let config = &firefox.config_list[2..];
    let (port, accepted) = ech_server(&recorded.private, config, None);
    let seen = accepted_handshake(port, &firefox.config_list, &accepted);
    assert_eq!(seen.cipher, "TLS_AES_128_GCM_SHA256");
    assert_eq!(seen.group.as_deref(), Some("X25519MLKEM768"));
    // The server read Firefox's ClientHelloInner.
    let (_, firefox_decoded) =
        open_inner(&recorded.private, config, &firefox.outer).expect("Firefox's ECH opens");
    assert_eq!(
        shape(&handshake(seen.hello), false),
        shape(&firefox_decoded, false),
        "the ClientHelloInner the server read differs from Firefox's"
    );
}

#[test]
fn ech_is_accepted_after_a_hello_retry_request() {
    // A server that takes only P-384, which the profile offers but sends
    // no key share for, asks for one (HelloRetryRequest), and the second
    // ClientHello has to carry ECH that it accepts too.
    let recorded = recorded();
    let firefox = &recorded.recordings[0];
    let config = &firefox.config_list[2..];
    let (port, accepted) = ech_server(&recorded.private, config, Some("P-384"));
    let seen = accepted_handshake(port, &firefox.config_list, &accepted);
    assert_eq!(seen.cipher, "TLS_AES_128_GCM_SHA256");
    // P-384 could only be agreed on in the second ClientHello, and ECH
    // was accepted in it.
    assert_eq!(seen.group.as_deref(), Some("P-384"));
}

#[test]
fn ech_grease_on_a_resumption_is_as_long_as_nss_makes_it() {
    // Without a record the hello carries ECH GREASE; once a session is
    // in, the next one resumes it, with a pre_shared_key the GREASE
    // payload has to count, as NSS's does.
    let recorded = recorded();
    let firefox = &recorded.recordings[0];
    let (port, accepted) = ech_server(&recorded.private, &firefox.config_list[2..], None);
    let fetcher = fetcher("firefox-160-macos", "origin.test", port, None);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut resumed = 0;
    for _ in 0..4 {
        let answer = runtime.block_on(fetcher.get(&format!("https://origin.test:{port}/"), &[]));
        assert_eq!(answer.expect("answered").status, 200);
        let seen = accepted.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!seen.ech);
        let hello = handshake(seen.hello);
        let p = parts(&hello[4..]).unwrap();
        resumed += usize::from(p.extensions.iter().any(|(k, _)| *k == PRE_SHARED_KEY));
        let ech = &p.extensions.iter().find(|(k, _)| *k == ECH).unwrap().1;
        let enc = u16::from_be_bytes([ech[6], ech[7]]) as usize;
        assert_eq!(
            ech.len() - 10 - enc,
            nss_grease_plaintext_len(&hello, "origin.test") + 16,
            "the GREASE payload's length"
        );
    }
    assert!(resumed > 0, "no handshake resumed");
}

/// A TCP port on 127.0.0.1 that passes each connection on to `to`, and
/// writes down the ClientHellos the client sends in the clear: the first,
/// and the second after a HelloRetryRequest.
fn hello_relay(to: u16) -> (u16, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, hellos) = mpsc::channel();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(mut client) = client else { continue };
            let Ok(mut server) = std::net::TcpStream::connect(("127.0.0.1", to)) else {
                continue;
            };
            let (mut back_from, mut back_to) =
                (server.try_clone().unwrap(), client.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut back_from, &mut back_to);
            });
            let tx = tx.clone();
            std::thread::spawn(move || {
                // Records one after another; a handshake record that starts
                // a ClientHello is one (each fits a record here).
                let mut seen = 0;
                loop {
                    let mut header = [0; 5];
                    if client.read_exact(&mut header).is_err() {
                        return;
                    }
                    let mut body = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
                    if client.read_exact(&mut body).is_err() {
                        return;
                    }
                    if seen < 2 && header[0] == 22 && body.first() == Some(&1) {
                        seen += 1;
                        let _ = tx.send(body.clone());
                    }
                    if server.write_all(&header).is_err() || server.write_all(&body).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, hellos)
}

#[test]
fn ech_grease_is_the_same_in_the_second_client_hello() {
    // RFC 9849, section 6.2: after a HelloRetryRequest, the second
    // ClientHello carries the same GREASE encrypted_client_hello as the
    // first, as NSS's does (tls13_MaybeGreaseEch sends greaseEchBuf
    // again).
    let recorded = recorded();
    let firefox = &recorded.recordings[0];
    let (server, accepted) =
        ech_server(&recorded.private, &firefox.config_list[2..], Some("P-384"));
    let (port, hellos) = hello_relay(server);
    let fetcher = fetcher("firefox-160-macos", "origin.test", port, None);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let answer = runtime.block_on(fetcher.get(&format!("https://origin.test:{port}/"), &[]));
    assert_eq!(answer.expect("answered").status, 200);
    let seen = accepted.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(seen.group.as_deref(), Some("P-384"));
    let first = hellos.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = hellos
        .recv_timeout(Duration::from_secs(5))
        .expect("a second ClientHello");
    let ech = |hello: &[u8]| {
        let p = parts(&hello[4..]).unwrap();
        p.extensions.into_iter().find(|(k, _)| *k == ECH).unwrap().1
    };
    assert_eq!(ech(&first), ech(&second));
}
