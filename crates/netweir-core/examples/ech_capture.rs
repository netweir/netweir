//! Records the ClientHellos a client sends over TCP with ECH: the outer
//! one as it arrives, and the inner one decrypted with the key this
//! program holds, before and after its compressed extensions are put back.
//!
//!     cargo run -p netweir-core --example ech_capture -- DIR [TCP_PORT] [DOH_PORT] [MAX_NAME_LENGTH] [AEADS]
//!
//! It serves DNS over HTTPS on 127.0.0.1:DOH_PORT (default 8053), which
//! answers every name with 127.0.0.1 and an HTTPS record that offers h2
//! and http/1.1 with an ECH configuration, and listens for TLS on
//! 127.0.0.1:TCP_PORT (default 8443). The ECH key, the configuration
//! (`ech_config_list.hex`) and the DoH server's certificate
//! (`doh-cert.pem`) are kept in DIR and used again on the next run, so
//! Firefox and Netweir see the same configuration. Each connection's
//! hellos go to DIR as `N.outer.hex`, `N.inner.hex` (as encoded, with
//! `ech_outer_extensions` and padding) and `N.decoded.hex`.
//!
//! Firefox uses ECH only with DNS over HTTPS. Point a profile at the DoH
//! server with `network.trr.mode` 3, `network.trr.uri`
//! `https://127.0.0.1:DOH_PORT/dns-query`, `network.trr.allow-rfc1918`
//! true and `network.trr.confirmationNS` `skip`, trust `doh-cert.pem` in
//! the profile's certificate database, and open
//! `https://origin.test:TCP_PORT/`.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CONFIG_ID: u8 = 0x2a;
const PUBLIC_NAME: &str = "public.test";
#[path = "../tests/ech_open/mod.rs"]
mod ech_open;

use ech_open::{ECH, hex, open_inner, read_hello, unhex};

/// The X25519 key, kept in DIR, and the ECHConfig for its public half:
/// DHKEM(X25519, HKDF-SHA256) with HKDF-SHA256 and AES-128-GCM, as
/// draft-ietf-tls-esni lays it out.
fn key_and_config(dir: &Path, max_name_length: u8, suites: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let key_file = dir.join("ech_key.hex");
    let private = match std::fs::read_to_string(&key_file) {
        Ok(s) => unhex(&s),
        Err(_) => {
            let key = btls::pkey::PKey::generate(btls::pkey::Id::X25519).unwrap();
            let mut out = [0; 32];
            let private = key.raw_private_key(&mut out).unwrap().to_vec();
            std::fs::write(&key_file, hex(&private)).unwrap();
            private
        }
    };
    let mut public = [0u8; 32];
    unsafe {
        let key = btls_sys::EVP_HPKE_KEY_new();
        let ok = btls_sys::EVP_HPKE_KEY_init(
            key,
            btls_sys::EVP_hpke_x25519_hkdf_sha256(),
            private.as_ptr(),
            private.len(),
        );
        assert_eq!(ok, 1, "HPKE key");
        let mut len = 0;
        let ok = btls_sys::EVP_HPKE_KEY_public_key(key, public.as_mut_ptr(), &mut len, 32);
        assert!(ok == 1 && len == 32, "public key");
        btls_sys::EVP_HPKE_KEY_free(key);
    }
    let public = &public[..];
    let mut contents = vec![CONFIG_ID];
    contents.extend(0x0020u16.to_be_bytes());
    contents.extend((public.len() as u16).to_be_bytes());
    contents.extend(public);
    contents.extend((4 * suites.len() as u16).to_be_bytes());
    for aead in suites {
        contents.extend([0, 1, 0, *aead]);
    }
    contents.push(max_name_length);
    contents.push(PUBLIC_NAME.len() as u8);
    contents.extend(PUBLIC_NAME.as_bytes());
    contents.extend(0u16.to_be_bytes());
    let mut config = ECH.to_be_bytes().to_vec();
    config.extend((contents.len() as u16).to_be_bytes());
    config.extend(contents);
    (private, config)
}

fn capture(dir: PathBuf, port: u16, private: Vec<u8>, config: Vec<u8>) {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind TLS port");
    let count = AtomicUsize::new(0);
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let Some(outer) = read_hello(&mut stream) else {
            continue;
        };
        let n = count.fetch_add(1, Ordering::Relaxed);
        std::fs::write(dir.join(format!("{n}.outer.hex")), hex(&outer)).unwrap();
        eprintln!("connection {n}: outer hello, {} bytes", outer.len());
        if let Some((inner, decoded)) = open_inner(&private, &config, &outer) {
            std::fs::write(dir.join(format!("{n}.inner.hex")), hex(&inner)).unwrap();
            std::fs::write(dir.join(format!("{n}.decoded.hex")), hex(&decoded)).unwrap();
            eprintln!("connection {n}: inner hello, {} bytes encoded", inner.len());
        }
        // A handshake_failure alert, so the client doesn't wait.
        let _ = stream.write_all(&[21, 3, 3, 0, 2, 2, 40]);
    }
}

fn name_at(packet: &[u8], mut at: usize) -> Option<(Vec<u8>, usize)> {
    let start = at;
    loop {
        let len = *packet.get(at)? as usize;
        at += 1 + len;
        if len == 0 {
            return Some((packet[start..at].to_vec(), at));
        }
    }
}

/// The answer to one DNS query: 127.0.0.1 for A, the HTTPS record for
/// HTTPS, nothing for the rest.
fn answer(query: &[u8], ech: &[u8]) -> Option<Vec<u8>> {
    let (qname, end) = name_at(query, 12)?;
    let qtype = u16::from_be_bytes([*query.get(end)?, *query.get(end + 1)?]);
    let mut out = query[..2].to_vec();
    out.extend([0x81, 0x80, 0, 1]);
    let rdata = match qtype {
        1 => Some(vec![127, 0, 0, 1]),
        65 => {
            let mut d = 1u16.to_be_bytes().to_vec();
            d.push(0);
            let alpn = b"\x02h2\x08http/1.1";
            d.extend(1u16.to_be_bytes());
            d.extend((alpn.len() as u16).to_be_bytes());
            d.extend(alpn);
            d.extend(5u16.to_be_bytes());
            d.extend((ech.len() as u16).to_be_bytes());
            d.extend(ech);
            Some(d)
        }
        _ => None,
    };
    out.extend(u16::from(rdata.is_some()).to_be_bytes());
    out.extend([0, 0, 0, 0]);
    out.extend(&qname);
    out.extend(&query[end..end + 4]);
    if let Some(rdata) = rdata {
        out.extend([0xc0, 12]);
        out.extend(qtype.to_be_bytes());
        out.extend(1u16.to_be_bytes());
        out.extend(60u32.to_be_bytes());
        out.extend((rdata.len() as u16).to_be_bytes());
        out.extend(rdata);
    }
    eprintln!("DoH: type {qtype} for {}", String::from_utf8_lossy(&qname));
    Some(out)
}

async fn doh(dir: PathBuf, port: u16, ech: Vec<u8>) {
    let (cert_pem, cert_der, key_der) = {
        let cert_file = dir.join("doh-cert.pem");
        let key_file = dir.join("doh-key.der");
        if let (Ok(pem), Ok(key)) = (
            std::fs::read_to_string(&cert_file),
            std::fs::read(&key_file),
        ) {
            let der = rustls::pki_types::pem::PemObject::from_pem_slice(pem.as_bytes())
                .map(|c: rustls::pki_types::CertificateDer| c.to_vec())
                .unwrap();
            (pem, der, key)
        } else {
            let made = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
            let pem = made.cert.pem();
            std::fs::write(&cert_file, &pem).unwrap();
            let key = made.signing_key.serialize_der();
            std::fs::write(&key_file, &key).unwrap();
            (pem, made.cert.der().to_vec(), key)
        }
    };
    let _ = cert_pem;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert_der.into()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into()),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind DoH port");
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let (acceptor, ech) = (acceptor.clone(), ech.clone());
        tokio::spawn(async move {
            let Ok(mut tls) = acceptor.accept(stream).await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                // One HTTP/1.1 POST after another on the connection.
                let header_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while buf.len() < header_end + length {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend(&chunk[..n]),
                    }
                }
                let query = buf[header_end..header_end + length].to_vec();
                buf.drain(..header_end + length);
                let Some(body) = answer(&query, &ech) else {
                    return;
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/dns-message\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                if tls.write_all(response.as_bytes()).await.is_err()
                    || tls.write_all(&body).await.is_err()
                {
                    return;
                }
            }
        });
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("a directory"));
    let tcp: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8443);
    let doh_port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8053);
    let max_name_length: u8 = args.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    // HKDF-SHA256 with these AEADs, in order: 1 AES-128-GCM, 2 AES-256-GCM,
    // 3 ChaCha20-Poly1305.
    let suites: Vec<u8> = args
        .next()
        .map(|s| {
            s.split(',')
                .map(|a| a.parse().expect("an AEAD id"))
                .collect()
        })
        .unwrap_or_else(|| vec![1]);
    std::fs::create_dir_all(&dir).unwrap();
    let (private, config) = key_and_config(&dir, max_name_length, &suites);
    let mut list = (config.len() as u16).to_be_bytes().to_vec();
    list.extend(&config);
    std::fs::write(dir.join("ech_config_list.hex"), hex(&list)).unwrap();
    eprintln!("ECHConfigList: {}", hex(&list));
    let capturing = dir.clone();
    std::thread::spawn(move || capture(capturing, tcp, private, config));
    eprintln!("TLS on 127.0.0.1:{tcp}, DoH on https://127.0.0.1:{doh_port}/dns-query");
    doh(dir, doh_port, list).await;
}
