//! Parses a TLS ClientHello far enough to fingerprint it.

use serde::{Deserialize, Serialize};

/// GREASE values (RFC 8701) are 0x?a?a with both bytes equal.
pub fn is_grease(v: u16) -> bool {
    v & 0x0f0f == 0x0a0a && v >> 8 == v & 0xff
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub legacy_version: u16,
    pub ciphers: Vec<u16>,
    /// Extension ids in the order sent, GREASE included.
    pub extensions: Vec<u16>,
    pub server_name: Option<String>,
    pub alpn: Vec<String>,
    /// The first ALPN protocol's raw bytes, which JA4 hashes. Not kept in
    /// saved captures; `alpn` covers every protocol a browser sends.
    #[serde(skip)]
    pub alpn_first: Vec<u8>,
    pub supported_versions: Vec<u16>,
    pub supported_groups: Vec<u16>,
    pub key_share_groups: Vec<u16>,
    pub signature_algorithms: Vec<u16>,
    pub cert_compression: Vec<u16>,
    pub alps: Vec<String>,
    pub psk_modes: Vec<u8>,
    pub ec_point_formats: Vec<u8>,
    pub record_size_limit: Option<u16>,
    /// Signature schemes offered for delegated credentials (0x0022), which
    /// Firefox sends and Chrome doesn't.
    #[serde(default)]
    pub delegated_credentials: Option<Vec<u16>>,
    /// Raw body of the trust_anchors extension (0xca34), hex-encoded.
    pub trust_anchors: Option<String>,
    /// Payload length of each GREASE extension, in the order sent. Empty in
    /// captures from before it was kept.
    #[serde(default)]
    pub grease_extension_lengths: Vec<usize>,
    /// Key length of each GREASE key share, in the order sent. Empty in
    /// captures from before it was kept.
    #[serde(default)]
    pub grease_key_share_lengths: Vec<usize>,
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn u24(&mut self) -> Option<usize> {
        self.take(3)
            .map(|b| (b[0] as usize) << 16 | (b[1] as usize) << 8 | b[2] as usize)
    }
    fn vec8(&mut self) -> Option<&'a [u8]> {
        let n = self.u8()? as usize;
        self.take(n)
    }
    fn vec16(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()? as usize;
        self.take(n)
    }
    fn u16s(bytes: &[u8]) -> Vec<u16> {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_be_bytes(c))
            .collect()
    }
    fn names(mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(name) = self.vec8() {
            out.push(String::from_utf8_lossy(name).into_owned());
        }
        out
    }
}

impl ClientHello {
    /// Parses the handshake message carried in one or more TLS records.
    /// `records` is the raw bytes read from the socket, starting at the
    /// first record header.
    pub fn parse(records: &[u8]) -> Option<ClientHello> {
        let handshake = handshake_bytes(records)?;
        let mut r = Reader(&handshake);
        if r.u8()? != 1 {
            return None; // not a ClientHello
        }
        let body_len = r.u24()?;
        let mut r = Reader(r.take(body_len)?);
        let legacy_version = r.u16()?;
        r.take(32)?; // random
        r.vec8()?; // session id
        let ciphers = Reader::u16s(r.vec16()?);
        r.vec8()?; // compression methods
        let mut hello = ClientHello {
            legacy_version,
            ciphers,
            extensions: Vec::new(),
            server_name: None,
            alpn: Vec::new(),
            alpn_first: Vec::new(),
            supported_versions: Vec::new(),
            supported_groups: Vec::new(),
            key_share_groups: Vec::new(),
            signature_algorithms: Vec::new(),
            cert_compression: Vec::new(),
            alps: Vec::new(),
            psk_modes: Vec::new(),
            ec_point_formats: Vec::new(),
            record_size_limit: None,
            delegated_credentials: None,
            trust_anchors: None,
            grease_extension_lengths: Vec::new(),
            grease_key_share_lengths: Vec::new(),
        };
        let mut exts = Reader(r.vec16().unwrap_or(&[]));
        while let Some(id) = exts.u16() {
            let data = exts.vec16()?;
            hello.extensions.push(id);
            if is_grease(id) {
                hello.grease_extension_lengths.push(data.len());
            }
            let mut d = Reader(data);
            match id {
                0x0000 => {
                    let mut list = Reader(d.vec16()?);
                    if list.u8()? == 0 {
                        hello.server_name =
                            Some(String::from_utf8_lossy(list.vec16()?).into_owned());
                    }
                }
                0x000a => hello.supported_groups = Reader::u16s(d.vec16()?),
                0x000b => hello.ec_point_formats = d.vec8()?.to_vec(),
                0x000d => hello.signature_algorithms = Reader::u16s(d.vec16()?),
                0x0010 => {
                    let list = d.vec16()?;
                    hello.alpn_first = Reader(list).vec8().unwrap_or_default().to_vec();
                    hello.alpn = Reader(list).names();
                }
                0x001b => hello.cert_compression = Reader::u16s(d.vec8()?),
                0x001c => hello.record_size_limit = d.u16(),
                0x0022 => hello.delegated_credentials = Some(Reader::u16s(d.vec16()?)),
                0x002b => hello.supported_versions = Reader::u16s(d.vec8()?),
                0x002d => hello.psk_modes = d.vec8()?.to_vec(),
                0x0033 => {
                    let mut shares = Reader(d.vec16()?);
                    while let Some(group) = shares.u16() {
                        let key = shares.vec16()?;
                        if is_grease(group) {
                            hello.grease_key_share_lengths.push(key.len());
                        }
                        hello.key_share_groups.push(group);
                    }
                }
                0x4469 | 0x44cd => hello.alps = Reader(d.vec16()?).names(),
                0xca34 => {
                    hello.trust_anchors = Some(data.iter().map(|b| format!("{b:02x}")).collect())
                }
                _ => {}
            }
        }
        Some(hello)
    }

    /// The extension ids sorted and without GREASE. Chrome shuffles its
    /// extension order on every connection, so this is what a profile can
    /// be compared on.
    pub fn extension_set(&self) -> Vec<u16> {
        let mut set: Vec<u16> = self
            .extensions
            .iter()
            .copied()
            .filter(|&e| !is_grease(e))
            .collect();
        set.sort_unstable();
        set
    }
}

/// Joins the handshake fragments carried by consecutive handshake records.
fn handshake_bytes(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    while data.len() >= 5 && data[0] == 0x16 {
        let len = u16::from_be_bytes([data[3], data[4]]) as usize;
        let record = data.get(5..5 + len)?;
        out.extend_from_slice(record);
        data = &data[5 + len..];
        if out.len() >= 4 {
            let want = 4 + ((out[1] as usize) << 16 | (out[2] as usize) << 8 | out[3] as usize);
            if out.len() >= want {
                return Some(out);
            }
        }
    }
    None
}

/// How many bytes of `data` (from the first record header) hold the whole
/// ClientHello: `Ok(None)` until enough has arrived to tell, `Err` as soon
/// as the bytes are not a TLS handshake.
pub fn bytes_needed(data: &[u8]) -> Result<Option<usize>, ()> {
    let mut offset = 0;
    let mut handshake = 0usize;
    let mut want = None;
    while offset < data.len() {
        // Every record of a ClientHello is a handshake record (0x16).
        if data[offset] != 0x16 {
            return Err(());
        }
        if offset + 5 > data.len() {
            break;
        }
        let len = u16::from_be_bytes([data[offset + 3], data[offset + 4]]) as usize;
        if want.is_none() && offset + 9 <= data.len() {
            let h = &data[offset + 5..];
            if h[0] != 1 {
                return Err(()); // not a ClientHello
            }
            want = Some(4 + ((h[1] as usize) << 16 | (h[2] as usize) << 8 | h[3] as usize));
        }
        offset += 5 + len;
        handshake += len;
        if want.is_some_and(|w| handshake >= w) {
            return Ok(Some(offset));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grease_payload_and_key_lengths_are_kept() {
        let mut exts = vec![0x1a, 0x1a, 0, 0]; // empty GREASE extension
        let mut shares = vec![0x2a, 0x2a, 0, 1, 0]; // GREASE, 1-byte key
        shares.extend_from_slice(&[0, 0x1d, 0, 32]); // x25519
        shares.extend_from_slice(&[7; 32]);
        exts.extend_from_slice(&[0, 0x33]);
        exts.extend_from_slice(&(shares.len() as u16 + 2).to_be_bytes());
        exts.extend_from_slice(&(shares.len() as u16).to_be_bytes());
        exts.extend_from_slice(&shares);
        exts.extend_from_slice(&[0x3a, 0x3a, 0, 1, 0]); // 1-byte GREASE extension
        let mut body = vec![3, 3];
        body.extend_from_slice(&[0; 32]); // random
        body.extend_from_slice(&[0, 0, 2, 0x13, 0x01, 1, 0]); // session id, ciphers, compression
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);
        let mut handshake = vec![1, 0];
        handshake.extend_from_slice(&(body.len() as u16).to_be_bytes());
        handshake.extend_from_slice(&body);
        let mut record = vec![0x16, 3, 1];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        let hello = ClientHello::parse(&record).unwrap();
        assert_eq!(hello.extensions, [0x1a1a, 0x0033, 0x3a3a]);
        assert_eq!(hello.key_share_groups, [0x2a2a, 0x001d]);
        assert_eq!(hello.grease_extension_lengths, [0, 1]);
        assert_eq!(hello.grease_key_share_lengths, [1]);
    }

    #[test]
    fn plain_http_is_rejected_at_the_first_byte() {
        assert_eq!(bytes_needed(b"G"), Err(()));
        assert_eq!(bytes_needed(b"GET / HTTP/1.1\r\n"), Err(()));
    }

    #[test]
    fn a_hello_split_over_two_records_is_measured_whole() {
        // Handshake header says 6 bytes of body: 10 bytes in all, sent as
        // 7 + 3 across two records.
        let mut data = vec![0x16, 3, 1, 0, 7, 1, 0, 0, 6, 0xaa, 0xbb, 0xcc];
        assert_eq!(bytes_needed(&data), Ok(None));
        data.extend_from_slice(&[0x16, 3, 1, 0, 3, 0xdd, 0xee, 0xff]);
        assert_eq!(bytes_needed(&data), Ok(Some(data.len())));
        assert_eq!(
            bytes_needed(&[0x16, 3, 1, 0, 4, 2, 0, 0, 0]),
            Err(()),
            "a ServerHello"
        );
    }
}
