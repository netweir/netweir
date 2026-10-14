//! JA4, the TLS client fingerprint, written from FoxIO's published
//! description of the algorithm (the JA4 TLS fingerprint is BSD-3-Clause).

use sha2::{Digest, Sha256};

use crate::client_hello::{ClientHello, is_grease};

pub fn ja4(hello: &ClientHello) -> String {
    let version = hello
        .supported_versions
        .iter()
        .copied()
        .filter(|&v| !is_grease(v))
        .max()
        .unwrap_or(hello.legacy_version);
    let version = match version {
        0x0304 => "13",
        0x0303 => "12",
        0x0302 => "11",
        0x0301 => "10",
        0x0300 => "s3",
        0x0002 => "s2",
        0xfeff => "d1",
        0xfefd => "d2",
        0xfefc => "d3",
        _ => "00",
    };
    let sni = if hello.server_name.is_some() {
        'd'
    } else {
        'i'
    };
    let ciphers: Vec<u16> = hello
        .ciphers
        .iter()
        .copied()
        .filter(|&c| !is_grease(c))
        .collect();
    let extensions: Vec<u16> = hello
        .extensions
        .iter()
        .copied()
        .filter(|&e| !is_grease(e))
        .collect();
    let first = if hello.alpn_first.is_empty() {
        hello.alpn.first().map(|a| a.as_bytes())
    } else {
        Some(hello.alpn_first.as_slice())
    };
    let alpn = alpn_chars(first);
    let a = format!(
        "t{version}{sni}{:02}{:02}{alpn}",
        ciphers.len().min(99),
        extensions.len().min(99)
    );

    let mut sorted_ciphers = ciphers.clone();
    sorted_ciphers.sort_unstable();
    let b = truncated_hash(&hex_list(&sorted_ciphers));

    let mut sorted_ext: Vec<u16> = extensions
        .into_iter()
        .filter(|&e| e != 0x0000 && e != 0x0010)
        .collect();
    sorted_ext.sort_unstable();
    let c = if sorted_ext.is_empty() {
        "000000000000".to_string()
    } else {
        let mut input = hex_list(&sorted_ext);
        // Chrome 152+ sends a GREASE signature algorithm; like every other
        // GREASE value it is random per connection and not part of the print.
        let sigalgs: Vec<u16> = hello
            .signature_algorithms
            .iter()
            .copied()
            .filter(|&s| !is_grease(s))
            .collect();
        if !sigalgs.is_empty() {
            input.push('_');
            input.push_str(&hex_list(&sigalgs));
        }
        truncated_hash(&input)
    };
    format!("{a}_{b}_{c}")
}

fn alpn_chars(first: Option<&[u8]>) -> String {
    let Some(bytes) = first.filter(|v| !v.is_empty()) else {
        return "00".to_string();
    };
    let (f, l) = (bytes[0], bytes[bytes.len() - 1]);
    if f.is_ascii_alphanumeric() && l.is_ascii_alphanumeric() {
        format!("{}{}", f as char, l as char)
    } else {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}{}", &hex[..1], &hex[hex.len() - 1..])
    }
}

fn hex_list(values: &[u16]) -> String {
    values
        .iter()
        .map(|v| format!("{v:04x}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn truncated_hash(input: &str) -> String {
    if input.is_empty() {
        return "000000000000".to_string();
    }
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cipher_hash_matches_the_published_example() {
        let list = "002f,0035,009c,009d,1301,1302,1303,c013,c014,c02b,c02c,c02f,c030,cca8,cca9";
        assert_eq!(truncated_hash(list), "8daaf6152771");
    }

    #[test]
    fn grease_signature_algorithms_do_not_change_the_print() {
        let mut hello = ClientHello {
            legacy_version: 0x0303,
            ciphers: vec![0x1301],
            extensions: vec![0x000d, 0x002b],
            server_name: None,
            alpn: vec![],
            alpn_first: vec![],
            supported_versions: vec![0x0304],
            supported_groups: vec![],
            key_share_groups: vec![],
            signature_algorithms: vec![0x0a0a, 0x0403],
            cert_compression: vec![],
            alps: vec![],
            psk_modes: vec![],
            ec_point_formats: vec![],
            record_size_limit: None,
            delegated_credentials: None,
            trust_anchors: None,
            grease_extension_lengths: vec![],
            grease_key_share_lengths: vec![],
        };
        let first = ja4(&hello);
        hello.signature_algorithms[0] = 0xdada;
        assert_eq!(ja4(&hello), first);
    }

    #[test]
    fn alpn_rules() {
        assert_eq!(alpn_chars(Some(b"h2")), "h2");
        assert_eq!(alpn_chars(Some(b"http/1.1")), "h1");
        assert_eq!(alpn_chars(None), "00");
        // Not alphanumeric: first and last hex digit of the raw bytes.
        assert_eq!(alpn_chars(Some(&[0xab])), "ab");
        assert_eq!(alpn_chars(Some(&[0xff, 0x01])), "f1");
    }
}
