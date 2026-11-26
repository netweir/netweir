//! Reads a client's first QUIC flight: the Initial packets that carry its
//! ClientHello. Initial packets are protected only with keys anyone can
//! derive from the client's Destination Connection ID (RFC 9001 §5.2), so
//! every frame in them is as visible to a server or an on-path observer as
//! a TCP ClientHello is.

use rustls::Side;
use rustls::quic::{Keys, Version};
use serde::{Deserialize, Serialize};

use crate::client_hello::ClientHello;

pub const V1: u32 = 0x0000_0001;
pub const V2: u32 = 0x6b33_43cf;

/// What the client's first flight showed, up to and including the datagram
/// that completed its ClientHello.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quic {
    /// The version of the first Initial: 1 for QUIC v1, 0x6b3343cf for v2.
    pub version: u32,
    /// Length of the Destination Connection ID the client chose.
    pub dcid_len: usize,
    /// Length of the client's own (Source) Connection ID.
    pub scid_len: usize,
    /// The first Initial's token, hex. Empty without one.
    pub token: String,
    pub datagrams: Vec<Datagram>,
    /// From the ClientHello's quic_transport_parameters extension, in the
    /// order sent.
    pub transport_parameters: Vec<TransportParameter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Datagram {
    /// UDP payload size.
    pub size: usize,
    /// The QUIC packets coalesced in it, in order.
    pub packets: Vec<Packet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Packet {
    /// "initial", "0-rtt", "handshake", "retry", "short" or "unknown"; or
    /// "zeros" for zero bytes after the last packet, which is how Firefox
    /// pads a datagram to size.
    pub kind: String,
    /// Bytes of the datagram this packet takes.
    pub size: usize,
    /// For a decrypted Initial: its packet number and how many bytes
    /// encoded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_number_len: Option<usize>,
    /// For a decrypted Initial: every frame, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<Frame>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// A run of consecutive PADDING bytes.
    Padding {
        length: usize,
    },
    Ping,
    Ack {
        largest: u64,
        delay: u64,
        /// ACK ranges after the first.
        ranges: u64,
        ecn: bool,
    },
    Crypto {
        offset: u64,
        length: usize,
    },
    ConnectionClose {
        error: u64,
    },
    /// A frame not allowed in an Initial; nothing after it is read.
    Other {
        id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportParameter {
    pub id: u64,
    /// The raw value, hex.
    pub value: String,
    /// The id is a reserved one (31 * N + 27, RFC 9000 §18.1), which is
    /// how a client GREASEs. Not every such id is random: 0xff02de1a, the
    /// ack-frequency draft's min_ack_delay that Firefox sends, is one too.
    pub grease: bool,
}

/// Reads a QUIC variable-length integer (RFC 9000 §16).
pub fn varint(data: &mut &[u8]) -> Option<u64> {
    let first = *data.first()?;
    let len = 1usize << (first >> 6);
    let bytes = data.get(..len)?;
    let mut v = u64::from(first & 0x3f);
    for &b in &bytes[1..] {
        v = v << 8 | u64::from(b);
    }
    *data = &data[len..];
    Some(v)
}

fn take<'a>(data: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let head = data.get(..n)?;
    *data = &data[n..];
    Some(head)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The transport parameters in a quic_transport_parameters extension.
pub fn transport_parameters(mut data: &[u8]) -> Option<Vec<TransportParameter>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let id = varint(&mut data)?;
        let len = varint(&mut data)? as usize;
        let value = take(&mut data, len)?;
        out.push(TransportParameter {
            id,
            value: hex(value),
            grease: id % 31 == 27,
        });
    }
    Some(out)
}

/// The packet type a long header's two type bits name, which v2 (RFC 9369
/// §3.2) numbers differently from v1.
fn long_kind(version: u32, bits: u8) -> &'static str {
    match (version, bits) {
        (V1, 0) | (V2, 1) => "initial",
        (V1, 1) | (V2, 2) => "0-rtt",
        (V1, 2) | (V2, 3) => "handshake",
        (V1, 3) | (V2, 0) => "retry",
        _ => "unknown",
    }
}

/// The full packet number from its truncated encoding (RFC 9000 §A.3).
fn packet_number(largest: Option<u64>, truncated: u64, len: usize) -> u64 {
    let Some(largest) = largest else {
        return truncated;
    };
    let expected = largest + 1;
    let win = 1u64 << (len * 8);
    let candidate = (expected & !(win - 1)) | truncated;
    if candidate + win / 2 <= expected && candidate < (1 << 62) - win {
        candidate + win
    } else if candidate > expected + win / 2 && candidate >= win {
        candidate - win
    } else {
        candidate
    }
}

fn frames(mut p: &[u8], crypto: &mut Vec<(u64, Vec<u8>)>) -> Option<Vec<Frame>> {
    let mut out = Vec::new();
    while !p.is_empty() {
        let id = varint(&mut p)?;
        let frame = match id {
            0x00 => {
                let run = 1 + p.iter().take_while(|&&b| b == 0).count();
                p = &p[run - 1..];
                if let Some(Frame::Padding { length }) = out.last_mut() {
                    *length += run;
                    continue;
                }
                Frame::Padding { length: run }
            }
            0x01 => Frame::Ping,
            0x02 | 0x03 => {
                let largest = varint(&mut p)?;
                let delay = varint(&mut p)?;
                let ranges = varint(&mut p)?;
                varint(&mut p)?; // first range
                for _ in 0..ranges {
                    varint(&mut p)?;
                    varint(&mut p)?;
                }
                if id == 0x03 {
                    for _ in 0..3 {
                        varint(&mut p)?;
                    }
                }
                Frame::Ack {
                    largest,
                    delay,
                    ranges,
                    ecn: id == 0x03,
                }
            }
            0x06 => {
                let offset = varint(&mut p)?;
                let len = varint(&mut p)? as usize;
                crypto.push((offset, take(&mut p, len)?.to_vec()));
                Frame::Crypto {
                    offset,
                    length: len,
                }
            }
            0x1c => {
                let error = varint(&mut p)?;
                varint(&mut p)?; // frame type
                let len = varint(&mut p)? as usize;
                take(&mut p, len)?;
                Frame::ConnectionClose { error }
            }
            _ => {
                out.push(Frame::Other { id });
                break;
            }
        };
        out.push(frame);
    }
    Some(out)
}

/// The CRYPTO stream's bytes from offset 0, as far as they are contiguous.
fn contiguous(chunks: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut sorted: Vec<&(u64, Vec<u8>)> = chunks.iter().collect();
    sorted.sort_by_key(|(offset, _)| *offset);
    let mut out = Vec::new();
    for (offset, data) in sorted {
        let (offset, end) = (*offset as usize, *offset as usize + data.len());
        if offset > out.len() {
            break;
        }
        if end > out.len() {
            out.extend_from_slice(&data[out.len() - offset..]);
        }
    }
    out
}

/// The whole ClientHello handshake message, once the CRYPTO data holds it.
fn complete_hello(chunks: &[(u64, Vec<u8>)]) -> Option<Vec<u8>> {
    let data = contiguous(chunks);
    let want = 4 + ((*data.get(1)? as usize) << 16 | (data[2] as usize) << 8 | data[3] as usize);
    (data.len() >= want).then(|| data[..want].to_vec())
}

fn initial_keys(version: u32, dcid: &[u8]) -> Option<Keys> {
    let version = match version {
        V1 => Version::V1,
        V2 => Version::V2,
        _ => return None,
    };
    let rustls::SupportedCipherSuite::Tls13(suite) =
        rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256
    else {
        return None;
    };
    Some(suite.quic_suite()?.keys(dcid, Side::Server, version))
}

/// Removes header and packet protection from the Initial at the start of
/// `packet`, whose packet number starts at `pn_offset`. Returns the packet
/// number, its length and the plaintext frames.
fn open(keys: &Keys, packet: &[u8], pn_offset: usize) -> Option<(u64, usize, Vec<u8>)> {
    let mut packet = packet.to_vec();
    let sample_len = keys.remote.header.sample_len();
    let sample = packet
        .get(pn_offset + 4..pn_offset + 4 + sample_len)?
        .to_vec();
    let (first, rest) = packet.split_first_mut()?;
    keys.remote
        .header
        .decrypt_in_place(&sample, first, rest.get_mut(pn_offset - 1..pn_offset + 3)?)
        .ok()?;
    let pn_len = (packet[0] & 0x03) as usize + 1;
    let truncated = packet[pn_offset..pn_offset + pn_len]
        .iter()
        .fold(0u64, |n, &b| n << 8 | u64::from(b));
    let (header, payload) = packet.split_at_mut(pn_offset + pn_len);
    let plain = keys
        .remote
        .packet
        .decrypt_in_place(truncated, header, payload)
        .ok()?;
    Some((truncated, pn_len, plain.to_vec()))
}

/// The streams the client's 0-RTT packets in `datagrams` carried data on,
/// read with its early traffic secret. QUIC v1 only; packets that don't
/// open are skipped.
pub fn zero_rtt_streams(datagrams: &[Vec<u8>], secret: &[u8]) -> std::collections::HashSet<u64> {
    let mut streams = std::collections::HashSet::new();
    let Some(keys) = ZeroRttKeys::new(secret) else {
        return streams;
    };
    let mut largest = None;
    for datagram in datagrams {
        let mut rest = &datagram[..];
        while let Some(&first) = rest.first() {
            if first & 0x80 == 0 {
                break;
            }
            let start = rest;
            let mut r = &rest[1..];
            let Some(version) = take(&mut r, 4).map(|v| u32::from_be_bytes(v.try_into().unwrap()))
            else {
                break;
            };
            let Some(dcid_len) = take(&mut r, 1).map(|l| l[0] as usize) else {
                break;
            };
            if take(&mut r, dcid_len).is_none() {
                break;
            }
            let Some(scid_len) = take(&mut r, 1).map(|l| l[0] as usize) else {
                break;
            };
            if take(&mut r, scid_len).is_none() {
                break;
            }
            let kind = long_kind(version, (first >> 4) & 0x03);
            if kind == "initial" {
                let Some(len) = varint(&mut r) else { break };
                if take(&mut r, len as usize).is_none() {
                    break;
                }
            }
            let Some(length) = varint(&mut r).map(|l| l as usize) else {
                break;
            };
            let pn_offset = start.len() - r.len();
            let end = pn_offset + length;
            if end > start.len() || matches!(kind, "retry" | "unknown") {
                break;
            }
            if kind == "0-rtt"
                && version == V1
                && let Some((truncated, len, plain)) = keys.open(&start[..end], pn_offset)
            {
                let pn = packet_number(largest, truncated, len);
                largest = largest.max(Some(pn));
                stream_frames(&plain, &mut streams);
            }
            rest = &start[end..];
        }
    }
    streams
}

/// QUIC v1 packet protection for 0-RTT, from the client's early traffic
/// secret (RFC 9001, Section 5.1). The suite is told from the secret's
/// length: TLS_AES_256_GCM_SHA384 (Safari's) for 48 bytes, else
/// TLS_AES_128_GCM_SHA256 (Chrome's and Firefox's).
/// shortcut: a session resumed with TLS_CHACHA20_POLY1305_SHA256 doesn't
/// open, so reads as not early; add it if a profile ever resumes with it.
struct ZeroRttKeys {
    key: ring::aead::LessSafeKey,
    iv: [u8; 12],
    header: ring::aead::quic::HeaderProtectionKey,
}

impl ZeroRttKeys {
    fn new(secret: &[u8]) -> Option<ZeroRttKeys> {
        use ring::aead::{AES_128_GCM, AES_256_GCM, quic};
        let (hkdf, aead, hp_alg, len) = if secret.len() == 48 {
            (ring::hkdf::HKDF_SHA384, &AES_256_GCM, &quic::AES_256, 32)
        } else {
            (ring::hkdf::HKDF_SHA256, &AES_128_GCM, &quic::AES_128, 16)
        };
        let prk = ring::hkdf::Prk::new_less_safe(hkdf, secret);
        let (mut key, mut iv, mut hp) = ([0u8; 32], [0u8; 12], [0u8; 32]);
        expand_label(&prk, b"quic key", &mut key[..len])?;
        expand_label(&prk, b"quic iv", &mut iv)?;
        expand_label(&prk, b"quic hp", &mut hp[..len])?;
        Some(ZeroRttKeys {
            key: ring::aead::LessSafeKey::new(ring::aead::UnboundKey::new(aead, &key[..len]).ok()?),
            iv,
            header: quic::HeaderProtectionKey::new(hp_alg, &hp[..len]).ok()?,
        })
    }

    /// Removes header and packet protection from the long-header packet
    /// `packet`, whose packet number starts at `pn_offset`. Returns the
    /// truncated packet number, its length and the plaintext.
    fn open(&self, packet: &[u8], pn_offset: usize) -> Option<(u64, usize, Vec<u8>)> {
        let mut packet = packet.to_vec();
        let sample = packet.get(pn_offset + 4..pn_offset + 20)?;
        let mask = self.header.new_mask(sample).ok()?;
        packet[0] ^= mask[0] & 0x0f;
        let pn_len = (packet[0] & 0x03) as usize + 1;
        for i in 0..pn_len {
            *packet.get_mut(pn_offset + i)? ^= mask[1 + i];
        }
        let truncated = packet[pn_offset..pn_offset + pn_len]
            .iter()
            .fold(0u64, |n, &b| n << 8 | u64::from(b));
        let mut nonce = self.iv;
        for (i, b) in truncated.to_be_bytes().iter().enumerate() {
            nonce[4 + i] ^= b;
        }
        let (header, payload) = packet.split_at_mut(pn_offset + pn_len);
        let plain = self
            .key
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                ring::aead::Aad::from(&*header),
                payload,
            )
            .ok()?;
        Some((truncated, pn_len, plain.to_vec()))
    }
}

/// HKDF-Expand-Label with an empty context (RFC 8446, Section 7.1).
fn expand_label(prk: &ring::hkdf::Prk, label: &[u8], out: &mut [u8]) -> Option<()> {
    struct Len(usize);
    impl ring::hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let length = (out.len() as u16).to_be_bytes();
    let full = [b"tls13 ".as_slice(), label].concat();
    let label_len = [full.len() as u8];
    let info: [&[u8]; 4] = [&length, &label_len, &full, &[0]];
    prk.expand(&info, Len(out.len())).ok()?.fill(out).ok()
}

/// The streams the STREAM frames in `plain` carry data on. Stops at a frame
/// it doesn't know the length of.
fn stream_frames(mut p: &[u8], streams: &mut std::collections::HashSet<u64>) {
    while let Some(id) = varint(&mut p) {
        let ok = match id {
            0x00 | 0x01 => Some(()),
            0x02 | 0x03 => (|| {
                varint(&mut p)?;
                varint(&mut p)?;
                let ranges = varint(&mut p)?;
                varint(&mut p)?;
                for _ in 0..ranges * 2 + if id == 0x03 { 3 } else { 0 } {
                    varint(&mut p)?;
                }
                Some(())
            })(),
            0x04 => (|| {
                for _ in 0..3 {
                    varint(&mut p)?;
                }
                Some(())
            })(),
            0x05 | 0x11 => (|| {
                varint(&mut p)?;
                varint(&mut p)?;
                Some(())
            })(),
            0x08..=0x0f => (|| {
                let stream = varint(&mut p)?;
                streams.insert(stream);
                if id & 0x04 != 0 {
                    varint(&mut p)?;
                }
                let len = if id & 0x02 != 0 {
                    varint(&mut p)? as usize
                } else {
                    p.len()
                };
                take(&mut p, len)?;
                Some(())
            })(),
            0x10 | 0x12 | 0x13 | 0x14 | 0x16 | 0x17 => varint(&mut p).map(|_| ()),
            0x15 => (|| {
                varint(&mut p)?;
                varint(&mut p)?;
                Some(())
            })(),
            0x18 => (|| {
                varint(&mut p)?;
                varint(&mut p)?;
                let len = *take(&mut p, 1)?.first()? as usize;
                take(&mut p, len + 16)?;
                Some(())
            })(),
            0x19 => varint(&mut p).map(|_| ()),
            0x1a => take(&mut p, 8).map(|_| ()),
            0x30 => {
                p = &[];
                Some(())
            }
            0x31 => (|| {
                let len = varint(&mut p)? as usize;
                take(&mut p, len)?;
                Some(())
            })(),
            _ => None,
        };
        if ok.is_none() {
            return;
        }
    }
}

/// Reads the client's first flight from the datagrams it sent, in arrival
/// order, and returns it with the ClientHello its CRYPTO frames carry.
/// `odcid` is the Destination Connection ID of the client's first Initial;
/// datagrams before the first that starts with an Initial to it are
/// skipped, Initials after it are read whatever connection ID they carry,
/// and reading stops at the datagram that completes the hello.
/// `None` if the hello never completes or isn't one.
pub fn first_flight(datagrams: &[Vec<u8>], odcid: &[u8]) -> Option<(Quic, ClientHello)> {
    let mut quic: Option<Quic> = None;
    let mut keys = None;
    let mut crypto = Vec::new();
    let mut largest = None;
    for datagram in datagrams {
        let mut packets = Vec::new();
        let mut rest = &datagram[..];
        while let Some(&first) = rest.first() {
            if first & 0x80 == 0 {
                let zeros = rest.iter().all(|&b| b == 0);
                packets.push(Packet {
                    kind: if zeros { "zeros" } else { "short" }.into(),
                    size: rest.len(),
                    packet_number: None,
                    packet_number_len: None,
                    frames: Vec::new(),
                });
                break;
            }
            let start = rest;
            let mut r = &rest[1..];
            let version = u32::from_be_bytes(take(&mut r, 4)?.try_into().ok()?);
            let dcid_len = *take(&mut r, 1)?.first()? as usize;
            let dcid = take(&mut r, dcid_len)?;
            let scid_len = *take(&mut r, 1)?.first()? as usize;
            take(&mut r, scid_len)?;
            let kind = long_kind(version, (first >> 4) & 0x03);
            let mut packet = Packet {
                kind: kind.into(),
                size: start.len(),
                packet_number: None,
                packet_number_len: None,
                frames: Vec::new(),
            };
            if matches!(kind, "retry" | "unknown") {
                packets.push(packet);
                break;
            }
            let token = if kind == "initial" {
                let len = varint(&mut r)? as usize;
                take(&mut r, len)?
            } else {
                &[]
            };
            let length = varint(&mut r)? as usize;
            let pn_offset = start.len() - r.len();
            let end = pn_offset + length;
            if end > start.len() {
                return None;
            }
            packet.size = end;
            if quic.is_none() && kind == "initial" && dcid == odcid {
                keys = initial_keys(version, dcid);
                quic = Some(Quic {
                    version,
                    dcid_len,
                    scid_len,
                    token: hex(token),
                    datagrams: Vec::new(),
                    transport_parameters: Vec::new(),
                });
            }
            // A resent Initial goes to the server's connection ID once the
            // client has its reply, still under the first one's keys (RFC
            // 9001, Section 5.2).
            if let (Some(keys), "initial") = (&keys, kind)
                && let Some((truncated, len, plain)) = open(keys, &start[..end], pn_offset)
            {
                let pn = packet_number(largest, truncated, len);
                largest = largest.max(Some(pn));
                packet.packet_number = Some(pn);
                packet.packet_number_len = Some(len);
                packet.frames = frames(&plain, &mut crypto)?;
            }
            packets.push(packet);
            rest = &start[end..];
        }
        let Some(q) = quic.as_mut() else { continue };
        q.datagrams.push(Datagram {
            size: datagram.len(),
            packets,
        });
        if let Some(handshake) = complete_hello(&crypto) {
            let hello = ClientHello::parse_handshake(&handshake)?;
            let mut quic = quic?;
            quic.transport_parameters = transport_parameters(&hello.quic_transport_parameters)?;
            return Some((quic, hello));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 9001 Appendix A.2's client Initial, and RFC 9369 Appendix A.2's
    /// v2 version of it: the same CRYPTO frame and PADDING, protected with
    /// each version's keys.
    const RFC9001_INITIAL: &str = include_str!("../tests/data/rfc9001-client-initial.hex");
    const RFC9369_INITIAL: &str = include_str!("../tests/data/rfc9369-client-initial.hex");
    const RFC_DCID: &str = "8394c8f03e515708";

    fn check_rfc_initial(datagram: &str, version: u32) {
        let (quic, hello) = first_flight(&[unhex(datagram)], &unhex(RFC_DCID)).unwrap();
        assert_eq!(quic.version, version);
        assert_eq!((quic.dcid_len, quic.scid_len), (8, 0));
        assert_eq!(quic.token, "");
        assert_eq!(quic.datagrams.len(), 1);
        let packet = &quic.datagrams[0].packets[0];
        assert_eq!((packet.kind.as_str(), packet.size), ("initial", 1200));
        assert_eq!(
            (packet.packet_number, packet.packet_number_len),
            (Some(2), Some(4))
        );
        // The CRYPTO frame is 245 bytes of hello (0x40f1) and its 4-byte
        // header; PADDING makes up the 1162-byte payload.
        assert_eq!(
            packet.frames,
            [
                Frame::Crypto {
                    offset: 0,
                    length: 241
                },
                Frame::Padding { length: 1162 - 245 },
            ]
        );
        assert_eq!(hello.server_name.as_deref(), Some("example.com"));
        assert_eq!(hello.alpn, ["alpn"]);
        assert_eq!(hello.ciphers, [0x1301, 0x1302]);
        assert_eq!(
            hello.extensions,
            [
                0x0000, 0xff01, 0x000a, 0x0010, 0x0005, 0x0033, 0x002b, 0x000d, 0x002d, 0x001c,
                0x0039
            ]
        );
        let params: Vec<(u64, &str)> = quic
            .transport_parameters
            .iter()
            .map(|p| (p.id, p.value.as_str()))
            .collect();
        assert_eq!(
            params,
            [
                (0x04, "ffffffffffffffff"),
                (0x05, "8000ffff"),
                (0x07, "8000ffff"),
                (0x08, "10"),
                (0x01, "80007530"),
                (0x09, "10"),
                (0x0f, "8394c8f03e515708"),
                (0x06, "8000ffff"),
            ]
        );
        assert!(quic.transport_parameters.iter().all(|p| !p.grease));
    }

    #[test]
    fn the_rfc_9001_client_initial_decrypts_to_its_frames_and_hello() {
        check_rfc_initial(RFC9001_INITIAL, V1);
    }

    #[test]
    fn the_rfc_9369_v2_client_initial_decrypts_to_the_same() {
        check_rfc_initial(RFC9369_INITIAL, V2);
    }

    #[test]
    fn a_chrome_initial_gives_the_quic_ja4_foxio_publishes() {
        // Frame 3 of pcap/tls-handshake.pcapng in FoxIO's ja4 repository,
        // Chrome connecting to Google over QUIC. FoxIO's Zeek test baseline
        // (zeek/testing/Baseline/tests.ja4-tls) and Wireshark README give
        // its JA4 as below.
        let datagram = unhex(include_str!("../tests/data/foxio-chrome-initial.hex"));
        let (quic, hello) = first_flight(&[datagram], &unhex("2fadc20ded383f8c")).unwrap();
        assert_eq!(
            crate::ja4::ja4_quic(&hello),
            "q13d0310h3_55b375c5d22e_cd85d2d88918"
        );
        assert_eq!(quic.datagrams[0].size, 1250);
        assert!(quic.transport_parameters.iter().any(|p| p.grease));
    }

    #[test]
    fn zero_bytes_after_the_initial_are_padding_not_a_packet() {
        let mut datagram = unhex(RFC9001_INITIAL);
        datagram.extend_from_slice(&[0; 52]);
        let (quic, _) = first_flight(&[datagram], &unhex(RFC_DCID)).unwrap();
        let packets: Vec<(&str, usize)> = quic.datagrams[0]
            .packets
            .iter()
            .map(|p| (p.kind.as_str(), p.size))
            .collect();
        assert_eq!(packets, [("initial", 1200), ("zeros", 52)]);
        assert_eq!(quic.datagrams[0].size, 1252);
    }

    #[test]
    fn a_flight_to_another_connection_id_is_not_read() {
        assert!(first_flight(&[unhex(RFC9001_INITIAL)], &[0; 8]).is_none());
    }

    #[test]
    fn a_tampered_initial_does_not_decrypt() {
        let mut datagram = unhex(RFC9001_INITIAL);
        datagram[100] ^= 1;
        let dcid = unhex(RFC_DCID);
        assert!(first_flight(&[datagram], &dcid).is_none());
    }

    #[test]
    fn grease_transport_parameters_are_flagged() {
        // max_idle_timeout 30000, then reserved id 31 * 1 + 27 = 58 with
        // three bytes, then 31 * 500 + 27 = 15527 (two-byte varint 0x7ca7)
        // with none.
        let params = transport_parameters(&[
            0x01, 0x04, 0x80, 0, 0x75, 0x30, 0x3a, 3, 1, 2, 3, 0x7c, 0xa7, 0,
        ])
        .unwrap();
        assert_eq!(
            params,
            [
                TransportParameter {
                    id: 1,
                    value: "80007530".into(),
                    grease: false
                },
                TransportParameter {
                    id: 58,
                    value: "010203".into(),
                    grease: true
                },
                TransportParameter {
                    id: 15527,
                    value: "".into(),
                    grease: true
                },
            ]
        );
        assert!(transport_parameters(&[0x01, 0x04, 0x80]).is_none(), "short");
    }

    #[test]
    fn crypto_frames_out_of_order_are_put_back_together() {
        let chunks = vec![
            (3, b"def".to_vec()),
            (0, b"abc".to_vec()),
            (8, b"x".to_vec()),
        ];
        assert_eq!(contiguous(&chunks), b"abcdef");
    }

    #[test]
    fn packet_numbers_decode_like_rfc_9000_a_3() {
        assert_eq!(packet_number(Some(0xa82f30ea), 0x9b32, 2), 0xa82f9b32);
        assert_eq!(packet_number(None, 7, 1), 7);
    }

    #[test]
    fn varints_decode_like_rfc_9000_a_1() {
        for (bytes, value) in [
            (
                &[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c][..],
                151_288_809_941_952_652,
            ),
            (&[0x9d, 0x7f, 0x3e, 0x7d], 494_878_333),
            (&[0x7b, 0xbd], 15_293),
            (&[0x25], 37),
        ] {
            let mut b = bytes;
            assert_eq!(varint(&mut b), Some(value));
            assert!(b.is_empty());
        }
        assert_eq!(varint(&mut &[0x7b][..]), None);
    }
}
