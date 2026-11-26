//! Opening a recorded ClientHelloOuter: the ClientHelloInner its
//! encrypted_client_hello extension carries, decrypted with HPKE, as
//! encoded and as the server reads it. Shared by `tests/ech_tcp.rs` and
//! `examples/ech_capture.rs`.

#![allow(dead_code)]

use std::io::Read;
use std::net::TcpStream;

pub const ECH: u16 = 0xfe0d;
pub const OUTER_EXTENSIONS: u16 = 0xfd00;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// The handshake message a client's first TLS records carry.
pub fn read_hello(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut handshake = Vec::new();
    loop {
        let mut header = [0; 5];
        stream.read_exact(&mut header).ok()?;
        if header[0] != 22 {
            return None;
        }
        let mut body = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
        stream.read_exact(&mut body).ok()?;
        handshake.extend(body);
        if handshake.len() >= 4 {
            let len = u32::from_be_bytes([0, handshake[1], handshake[2], handshake[3]]) as usize;
            if handshake.len() >= 4 + len {
                handshake.truncate(4 + len);
                return Some(handshake);
            }
        }
    }
}

/// A ClientHello's parts: the bytes before its extensions, and each
/// extension as its type and body.
pub struct Parts {
    pub head: Vec<u8>,
    pub extensions: Vec<(u16, Vec<u8>)>,
}

/// `body` is a ClientHello without its handshake header.
pub fn parts(body: &[u8]) -> Option<Parts> {
    let mut at = 2 + 32;
    let session = *body.get(at)? as usize;
    at += 1 + session;
    let ciphers = u16::from_be_bytes([*body.get(at)?, *body.get(at + 1)?]) as usize;
    at += 2 + ciphers;
    let compression = *body.get(at)? as usize;
    at += 1 + compression;
    let head = body[..at].to_vec();
    let total = u16::from_be_bytes([*body.get(at)?, *body.get(at + 1)?]) as usize;
    at += 2;
    let end = at + total;
    let mut extensions = Vec::new();
    while at + 4 <= end {
        let kind = u16::from_be_bytes([body[at], body[at + 1]]);
        let len = u16::from_be_bytes([body[at + 2], body[at + 3]]) as usize;
        extensions.push((kind, body.get(at + 4..at + 4 + len)?.to_vec()));
        at += 4 + len;
    }
    Some(Parts { head, extensions })
}

pub fn with_extensions(head: &[u8], extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut list = Vec::new();
    for (kind, body) in extensions {
        list.extend(kind.to_be_bytes());
        list.extend((body.len() as u16).to_be_bytes());
        list.extend(body);
    }
    let mut out = head.to_vec();
    out.extend((list.len() as u16).to_be_bytes());
    out.extend(list);
    out
}

pub fn handshake(body: Vec<u8>) -> Vec<u8> {
    let mut out = vec![1];
    out.extend(&(body.len() as u32).to_be_bytes()[1..]);
    out.extend(body);
    out
}

/// The EncodedClientHelloInner in `outer`'s ECH extension, opened with
/// HPKE, and the ClientHelloInner it stands for: its compressed
/// extensions taken from the outer one and its session ID from it too.
pub fn open_inner(private: &[u8], config: &[u8], outer: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let body = &outer[4..];
    let outer_parts = parts(body)?;
    let (index, ech) = outer_parts
        .extensions
        .iter()
        .enumerate()
        .find(|(_, (kind, _))| *kind == ECH)
        .map(|(i, (_, b))| (i, b.clone()))?;
    if ech.first() != Some(&0) || ech.get(5) != config.get(4) {
        eprintln!("ECH not for our configuration (GREASE?)");
        return None;
    }
    let (kdf, aead) = (
        u16::from_be_bytes([ech[1], ech[2]]),
        u16::from_be_bytes([ech[3], ech[4]]),
    );
    let enc_len = u16::from_be_bytes([ech[6], ech[7]]) as usize;
    let enc = &ech[8..8 + enc_len];
    let payload_at = 8 + enc_len + 2;
    let payload = &ech[payload_at..];
    // ClientHelloOuterAAD: the outer hello with the payload zeroed.
    let mut zeroed = outer_parts.extensions.clone();
    let mut zero_ech = ech.clone();
    zero_ech[payload_at..].fill(0);
    zeroed[index].1 = zero_ech;
    let aad = with_extensions(&outer_parts.head, &zeroed);
    let mut info = b"tls ech\0".to_vec();
    info.extend(config);
    let encoded = unsafe {
        use btls_sys as ffi;
        let key = ffi::EVP_HPKE_KEY_new();
        let ok = ffi::EVP_HPKE_KEY_init(
            key,
            ffi::EVP_hpke_x25519_hkdf_sha256(),
            private.as_ptr(),
            private.len(),
        );
        assert_eq!(ok, 1, "HPKE key");
        // HKDF-SHA256 with AES-128-GCM, AES-256-GCM or ChaCha20-Poly1305.
        let aead = match (kdf, aead) {
            (1, 1) => ffi::EVP_hpke_aes_128_gcm(),
            (1, 2) => ffi::EVP_hpke_aes_256_gcm(),
            (1, 3) => ffi::EVP_hpke_chacha20_poly1305(),
            other => {
                eprintln!("no HPKE suite {other:?} here");
                ffi::EVP_HPKE_KEY_free(key);
                return None;
            }
        };
        let ctx = ffi::EVP_HPKE_CTX_new();
        let ok = ffi::EVP_HPKE_CTX_setup_recipient(
            ctx,
            key,
            ffi::EVP_hpke_hkdf_sha256(),
            aead,
            enc.as_ptr(),
            enc.len(),
            info.as_ptr(),
            info.len(),
        );
        assert_eq!(ok, 1, "HPKE setup");
        let mut out = vec![0; payload.len()];
        let mut out_len = 0;
        let ok = ffi::EVP_HPKE_CTX_open(
            ctx,
            out.as_mut_ptr(),
            &mut out_len,
            out.len(),
            payload.as_ptr(),
            payload.len(),
            aad.as_ptr(),
            aad.len(),
        );
        ffi::EVP_HPKE_CTX_free(ctx);
        ffi::EVP_HPKE_KEY_free(key);
        if ok != 1 {
            eprintln!("HPKE open failed");
            return None;
        }
        out.truncate(out_len);
        out
    };
    // Its own length tells where the padding starts.
    let inner = parts(&encoded)?;
    let used = with_extensions(&inner.head, &inner.extensions).len();
    let encoded_hello = encoded[..used].to_vec();
    let padding = &encoded[used..];
    assert!(padding.iter().all(|&b| b == 0), "padding isn't zeros");
    let decoded = decompress(&inner.extensions, &outer_parts.extensions)?;
    // legacy_session_id comes from the outer hello.
    let mut head = inner.head.clone();
    let outer_session = &outer_parts.head[34..35 + outer_parts.head[34] as usize];
    head.splice(34..35, outer_session.iter().copied());
    let mut with_padding = handshake(encoded_hello);
    with_padding.extend(padding);
    Some((with_padding, handshake(with_extensions(&head, &decoded))))
}

/// A ClientHelloInner's extensions with those `ech_outer_extensions`
/// names taken from the outer hello's. RFC 9849, section 5.1: never
/// encrypted_client_hello itself, and in the outer hello's order; a
/// server rejects anything else with illegal_parameter, and so does this.
pub fn decompress(
    inner: &[(u16, Vec<u8>)],
    outer: &[(u16, Vec<u8>)],
) -> Option<Vec<(u16, Vec<u8>)>> {
    let mut decoded = Vec::new();
    let mut after = 0;
    for (kind, ext) in inner {
        if *kind == OUTER_EXTENSIONS {
            let list = ext.get(1..1 + *ext.first()? as usize)?;
            if list.is_empty() || list.len() % 2 != 0 || list.len() + 1 != ext.len() {
                eprintln!("a malformed ech_outer_extensions");
                return None;
            }
            for pair in list.chunks(2) {
                let wanted = u16::from_be_bytes([pair[0], pair[1]]);
                let found = outer[after..]
                    .iter()
                    .position(|(k, _)| *k == wanted)
                    .filter(|_| wanted != ECH);
                let Some(at) = found else {
                    eprintln!("ech_outer_extensions names {wanted:#06x} out of order");
                    return None;
                };
                decoded.push(outer[after + at].clone());
                after += at + 1;
            }
        } else {
            decoded.push((*kind, ext.clone()));
        }
    }
    Some(decoded)
}
