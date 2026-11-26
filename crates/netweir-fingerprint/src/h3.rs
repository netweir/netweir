//! Just enough HTTP/3 (RFC 9114) to record what a client sends on a
//! connection and answer each request. The server advertises a QPACK
//! dynamic table of 0 (RFC 9204), so every request's headers decode from
//! the static table and literals alone.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::h2::{hpack_int, hpack_string};
use crate::quic::varint;

const DATA: u64 = 0x00;
const HEADERS: u64 = 0x01;
const SETTINGS: u64 = 0x04;
const CONTROL_STREAM: u64 = 0x00;

/// What the client said on the connection, plus the frames it put on one
/// request's stream around HEADERS and DATA.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Http3 {
    /// The unidirectional streams the client opened, as (stream id, stream
    /// type), in stream id order, which is the order it opened them: 0 is
    /// the control stream, 2 the QPACK encoder, 3 the QPACK decoder; a
    /// reserved type is GREASE (see [`is_h3_grease`]).
    pub streams: Vec<(u64, u64)>,
    /// SETTINGS on the client's control stream, in the order sent.
    /// QPACK's are 0x01 (QPACK_MAX_TABLE_CAPACITY) and 0x07
    /// (QPACK_BLOCKED_STREAMS).
    pub settings: Vec<Setting>,
    /// Frames on the control stream after SETTINGS by the time this
    /// request was answered, as (type, payload hex): PRIORITY_UPDATE is
    /// 0xf0700, and a reserved type is GREASE.
    pub control_frames: Vec<(u64, String)>,
    /// Frames on this request's stream other than HEADERS and DATA, in
    /// order, as (type, payload hex).
    pub request_frames: Vec<(u64, String)>,
    /// Whether this request's stream came in 0-RTT: its data was in the
    /// client's 0-RTT packets, on a server that offers early data.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub early: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    pub id: u64,
    pub value: u64,
    pub grease: bool,
}

/// HTTP/3's reserved values for setting ids, frame types and stream types,
/// 0x1f * N + 0x21 (RFC 9114 §7.2.4.1, §7.2.8, §6.2.3).
pub fn is_h3_grease(v: u64) -> bool {
    v >= 0x21 && (v - 0x21).is_multiple_of(0x1f)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One frame from the front of `data`, as (type, payload), or `None` until
/// the whole frame is there.
fn frame<'a>(data: &mut &'a [u8]) -> Option<(u64, &'a [u8])> {
    let mut d = *data;
    let kind = varint(&mut d)?;
    let len = varint(&mut d)? as usize;
    let payload = d.get(..len)?;
    *data = &d[len..];
    Some((kind, payload))
}

/// A SETTINGS frame's payload, in order.
pub fn settings(mut payload: &[u8]) -> Option<Vec<Setting>> {
    let mut out = Vec::new();
    while !payload.is_empty() {
        let id = varint(&mut payload)?;
        let value = varint(&mut payload)?;
        out.push(Setting {
            id,
            value,
            grease: is_h3_grease(id),
        });
    }
    Some(out)
}

/// The QPACK static table (RFC 9204 Appendix A).
const STATIC: [(&str, &str); 99] = [
    (":authority", ""),
    (":path", "/"),
    ("age", "0"),
    ("content-disposition", ""),
    ("content-length", "0"),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("referer", ""),
    ("set-cookie", ""),
    (":method", "CONNECT"),
    (":method", "DELETE"),
    (":method", "GET"),
    (":method", "HEAD"),
    (":method", "OPTIONS"),
    (":method", "POST"),
    (":method", "PUT"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "103"),
    (":status", "200"),
    (":status", "304"),
    (":status", "404"),
    (":status", "503"),
    ("accept", "*/*"),
    ("accept", "application/dns-message"),
    ("accept-encoding", "gzip, deflate, br"),
    ("accept-ranges", "bytes"),
    ("access-control-allow-headers", "cache-control"),
    ("access-control-allow-headers", "content-type"),
    ("access-control-allow-origin", "*"),
    ("cache-control", "max-age=0"),
    ("cache-control", "max-age=2592000"),
    ("cache-control", "max-age=604800"),
    ("cache-control", "no-cache"),
    ("cache-control", "no-store"),
    ("cache-control", "public, max-age=31536000"),
    ("content-encoding", "br"),
    ("content-encoding", "gzip"),
    ("content-type", "application/dns-message"),
    ("content-type", "application/javascript"),
    ("content-type", "application/json"),
    ("content-type", "application/x-www-form-urlencoded"),
    ("content-type", "image/gif"),
    ("content-type", "image/jpeg"),
    ("content-type", "image/png"),
    ("content-type", "text/css"),
    ("content-type", "text/html; charset=utf-8"),
    ("content-type", "text/plain"),
    ("content-type", "text/plain;charset=utf-8"),
    ("range", "bytes=0-"),
    ("strict-transport-security", "max-age=31536000"),
    (
        "strict-transport-security",
        "max-age=31536000; includesubdomains",
    ),
    (
        "strict-transport-security",
        "max-age=31536000; includesubdomains; preload",
    ),
    ("vary", "accept-encoding"),
    ("vary", "origin"),
    ("x-content-type-options", "nosniff"),
    ("x-xss-protection", "1; mode=block"),
    (":status", "100"),
    (":status", "204"),
    (":status", "206"),
    (":status", "302"),
    (":status", "400"),
    (":status", "403"),
    (":status", "421"),
    (":status", "425"),
    (":status", "500"),
    ("accept-language", ""),
    ("access-control-allow-credentials", "FALSE"),
    ("access-control-allow-credentials", "TRUE"),
    ("access-control-allow-headers", "*"),
    ("access-control-allow-methods", "get"),
    ("access-control-allow-methods", "get, post, options"),
    ("access-control-allow-methods", "options"),
    ("access-control-expose-headers", "content-length"),
    ("access-control-request-headers", "content-type"),
    ("access-control-request-method", "get"),
    ("access-control-request-method", "post"),
    ("alt-svc", "clear"),
    ("authorization", ""),
    (
        "content-security-policy",
        "script-src 'none'; object-src 'none'; base-uri 'none'",
    ),
    ("early-data", "1"),
    ("expect-ct", ""),
    ("forwarded", ""),
    ("if-range", ""),
    ("origin", ""),
    ("purpose", "prefetch"),
    ("server", ""),
    ("timing-allow-origin", "*"),
    ("upgrade-insecure-requests", "1"),
    ("user-agent", ""),
    ("x-forwarded-for", ""),
    ("x-frame-options", "deny"),
    ("x-frame-options", "sameorigin"),
];

/// A QPACK (HPACK) integer with an `n`-bit prefix (RFC 7541 §5.1).
fn prefix_int(data: &mut &[u8], n: u32) -> Option<u64> {
    let (&first, rest) = data.split_first()?;
    *data = rest;
    let max = (1u64 << n) - 1;
    let mut v = u64::from(first) & max;
    if v < max {
        return Some(v);
    }
    for shift in (0..63).step_by(7) {
        let (&b, rest) = data.split_first()?;
        *data = rest;
        v += u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// A string literal whose length has an `n`-bit prefix, with the Huffman
/// flag in the bit above it.
fn string(data: &mut &[u8], n: u32) -> Option<String> {
    let huffman = data.first()? & (1 << n) != 0;
    let len = prefix_int(data, n)? as usize;
    let raw = data.get(..len)?;
    *data = &data[len..];
    let bytes = if huffman {
        fluke_hpack::huffman::HuffmanDecoder::new()
            .decode(raw)
            .ok()?
    } else {
        raw.to_vec()
    };
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn static_entry(index: u64) -> Option<(String, String)> {
    let (name, value) = STATIC.get(index as usize)?;
    Some((name.to_string(), value.to_string()))
}

/// Decodes a field section that refers to no dynamic table, which is all a
/// client may send a server that allows none.
pub fn decode_fields(mut block: &[u8]) -> Option<Vec<(String, String)>> {
    if prefix_int(&mut block, 8)? != 0 {
        return None; // Required Insert Count: a dynamic table reference
    }
    prefix_int(&mut block, 7)?; // Base
    let mut out = Vec::new();
    while let Some(&b) = block.first() {
        let field = if b & 0x80 != 0 {
            // Indexed field line; T (0x40) must say static.
            if b & 0x40 == 0 {
                return None;
            }
            static_entry(prefix_int(&mut block, 6)?)?
        } else if b & 0x40 != 0 {
            // Literal field line with name reference.
            if b & 0x10 == 0 {
                return None;
            }
            let (name, _) = static_entry(prefix_int(&mut block, 4)?)?;
            (name, string(&mut block, 7)?)
        } else if b & 0x20 != 0 {
            // Literal field line with literal name.
            let name = string(&mut block, 3)?;
            (name, string(&mut block, 7)?)
        } else {
            return None; // post-base: dynamic table again
        };
        out.push(field);
    }
    Some(out)
}

/// Encodes a response's field section with literals only, so it needs no
/// encoder state.
fn encode_fields(status: u16, headers: &[(&str, String)]) -> Vec<u8> {
    let mut out = vec![0, 0];
    // Literal with a static name reference: ":status" is entry 25.
    hpack_int(&mut out, 0x50, 4, 25);
    hpack_string(&mut out, status.to_string().as_bytes());
    for (name, value) in headers {
        hpack_int(&mut out, 0x20, 3, name.len());
        out.extend_from_slice(name.as_bytes());
        hpack_string(&mut out, value.as_bytes());
    }
    out
}

fn put_varint(out: &mut Vec<u8>, v: u64) {
    match v {
        0..=0x3f => out.push(v as u8),
        0x40..=0x3fff => out.extend_from_slice(&(v as u16 | 0x4000).to_be_bytes()),
        0x4000..=0x3fff_ffff => out.extend_from_slice(&(v as u32 | 0x8000_0000).to_be_bytes()),
        _ => out.extend_from_slice(&(v | 0xc000_0000_0000_0000).to_be_bytes()),
    }
}

fn put_frame(out: &mut Vec<u8>, kind: u64, payload: &[u8]) {
    put_varint(out, kind);
    put_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

/// One request read from a connection.
pub struct Request {
    /// The request stream's ID.
    pub stream: u64,
    pub http3: Http3,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

fn failed(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Reads one unidirectional stream: its type, and for the control stream
/// every frame, into `state`. `settled` is told once SETTINGS is in.
async fn read_uni(
    mut recv: quinn::RecvStream,
    state: Arc<Mutex<Http3>>,
    settled: watch::Sender<bool>,
) -> io::Result<()> {
    let id = u64::from(recv.id());
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut kind = None;
    loop {
        let Some(n) = recv.read(&mut chunk).await.map_err(failed)? else {
            return Ok(());
        };
        buf.extend_from_slice(&chunk[..n]);
        let mut data = &buf[..];
        if kind.is_none() {
            let Some(k) = varint(&mut data) else { continue };
            kind = Some(k);
            let mut state = state.lock().unwrap();
            state.streams.push((id, k));
            // Each stream is read on its own task, so they finish in any order.
            state.streams.sort_unstable();
        }
        if kind != Some(CONTROL_STREAM) {
            buf.clear(); // QPACK and GREASE streams: only the type is kept
            continue;
        }
        while let Some((kind, payload)) = frame(&mut data) {
            let mut state = state.lock().unwrap();
            if kind == SETTINGS && !*settled.borrow() {
                state.settings = settings(payload).ok_or_else(|| failed("bad SETTINGS"))?;
                settled.send_replace(true);
            } else {
                state.control_frames.push((kind, hex(payload)));
            }
        }
        buf = data.to_vec();
        if buf.len() > 1 << 16 {
            return Err(failed("control frame too large"));
        }
    }
}

/// Serves a connection until the client closes it: opens the server's
/// control stream with empty SETTINGS (no dynamic table), records the
/// client's unidirectional streams, and answers each request stream with
/// what `on_request` returns.
pub async fn serve<F>(conn: quinn::Connection, mut on_request: F) -> io::Result<()>
where
    F: FnMut(Request) -> (u16, Vec<(&'static str, String)>, Vec<u8>),
{
    // The control stream, opened beside the requests being read. A
    // connection taken before its handshake is done may not have the
    // client's stream limits yet, and quinn doesn't wake an open_uni when
    // they come with the ClientHello, so it waits for that first.
    let control = {
        let conn = conn.clone();
        tokio::spawn(async move {
            while conn.handshake_data().is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let mut control = conn.open_uni().await.map_err(failed)?;
            let mut opening = Vec::new();
            put_varint(&mut opening, CONTROL_STREAM);
            put_frame(&mut opening, SETTINGS, &[]);
            control.write_all(&opening).await.map_err(failed)?;
            Ok::<_, io::Error>(control)
        })
    };

    let state = Arc::new(Mutex::new(Http3::default()));
    let (settled, mut settings_in) = watch::channel(false);
    let uni = {
        let (conn, state) = (conn.clone(), state.clone());
        tokio::spawn(async move {
            while let Ok(recv) = conn.accept_uni().await {
                tokio::spawn(read_uni(recv, state.clone(), settled.clone()));
            }
        })
    };
    let served = async {
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let stream = recv.id().index() << 2;
            // A stream the client resets, as when it cancels a request, is
            // skipped; the connection carries on.
            let Ok(raw) = recv.read_to_end(1 << 20).await else {
                continue;
            };
            let mut data = &raw[..];
            let (mut fields, mut body, mut request_frames) = (None, Vec::new(), Vec::new());
            while let Some((kind, payload)) = frame(&mut data) {
                match kind {
                    HEADERS if fields.is_none() => {
                        fields = Some(decode_fields(payload).ok_or_else(|| failed("bad QPACK"))?)
                    }
                    DATA => body.extend_from_slice(payload),
                    _ => request_frames.push((kind, hex(payload))),
                }
            }
            let headers = fields.ok_or_else(|| failed("request without HEADERS"))?;
            // `/close?code=N`: answered, then the connection closed with
            // application error N.
            let close = headers
                .iter()
                .find(|(k, _)| k == ":path")
                .and_then(|(_, p)| p.strip_prefix("/close?code="))
                .and_then(|code| code.parse::<u32>().ok());
            // A request can beat the control stream in; wait a moment for
            // its SETTINGS, as they belong in every capture.
            let _ = tokio::time::timeout(Duration::from_secs(2), settings_in.wait_for(|&in_| in_))
                .await;
            let mut http3 = state.lock().unwrap().clone();
            http3.request_frames = request_frames;
            let (status, reply_headers, reply) = on_request(Request {
                stream,
                http3,
                headers,
                body,
            });
            let mut out = Vec::new();
            put_frame(&mut out, HEADERS, &encode_fields(status, &reply_headers));
            put_frame(&mut out, DATA, &reply);
            if send.write_all(&out).await.is_ok() {
                let _ = send.finish();
            }
            if let Some(code) = close {
                // Once the client has the answer.
                let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
                conn.close(code.into(), b"");
                break;
            }
        }
        Ok(())
    };
    let result = served.await;
    uni.abort();
    // Held until here: closing the control stream would be a connection
    // error for the client.
    let control = if control.is_finished() {
        control.await.ok().and_then(Result::ok)
    } else {
        control.abort();
        None
    };
    drop(control);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_keep_their_order_and_flag_grease() {
        // QPACK_MAX_TABLE_CAPACITY 65536, MAX_FIELD_SECTION_SIZE 262144,
        // QPACK_BLOCKED_STREAMS 100, H3_DATAGRAM 1, then a reserved id
        // (0x1f * 3 + 0x21 = 0x7e, two-byte varint 0x407e) set to 0.
        let payload = [
            0x01, 0x80, 0x01, 0x00, 0x00, 0x06, 0x80, 0x04, 0x00, 0x00, 0x07, 0x40, 0x64, 0x33,
            0x01, 0x40, 0x7e, 0x00,
        ];
        let got: Vec<(u64, u64, bool)> = settings(&payload)
            .unwrap()
            .into_iter()
            .map(|s| (s.id, s.value, s.grease))
            .collect();
        assert_eq!(
            got,
            [
                (0x01, 65536, false),
                (0x06, 262_144, false),
                (0x07, 100, false),
                (0x33, 1, false),
                (0x7e, 0, true),
            ]
        );
        assert!(settings(&[0x01]).is_none(), "a setting without a value");
    }

    #[test]
    fn grease_values_are_0x1f_n_plus_0x21() {
        assert!(is_h3_grease(0x21) && is_h3_grease(0x40) && is_h3_grease(0x1f * 1000 + 0x21));
        assert!(!is_h3_grease(0x00) && !is_h3_grease(0x33) && !is_h3_grease(0x22));
    }

    #[test]
    fn frames_wait_until_whole() {
        let mut data = &[0x04, 0x02, 0x01][..];
        assert_eq!(frame(&mut data), None);
        assert_eq!(data.len(), 3, "nothing consumed");
        let mut data = &[0x04, 0x02, 0x01, 0x00, 0x00][..];
        assert_eq!(frame(&mut data), Some((0x04, &[0x01, 0x00][..])));
        assert_eq!(data, [0x00]);
    }

    #[test]
    fn a_field_section_decodes_in_order() {
        let mut block = vec![0x00, 0x00];
        // ":path: /index.html", a literal with static name reference 1.
        block.push(0x51);
        block.push(11);
        block.extend_from_slice(b"/index.html");
        // Static entries 17 (":method GET") and 23 (":scheme https").
        block.extend_from_slice(&[0xd1, 0xd7]);
        // "custom-key: custom-value", Huffman literal name and value (the
        // codes are RFC 7541 §C.4.3's). An 8-byte name overflows the
        // 3-bit prefix: 7, then 1.
        block.extend_from_slice(&[0x2f, 0x01, 0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xa9, 0x7d, 0x7f]);
        block.extend_from_slice(&[0x89, 0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xb8, 0xe8, 0xb4, 0xbf]);
        // "priority: u=0, i", plain literal name and value.
        block.extend_from_slice(&[0x27, 0x01]);
        block.extend_from_slice(b"priority");
        block.push(6);
        block.extend_from_slice(b"u=0, i");
        let fields = decode_fields(&block).unwrap();
        assert_eq!(
            fields,
            [
                (":path".to_string(), "/index.html".to_string()),
                (":method".to_string(), "GET".to_string()),
                (":scheme".to_string(), "https".to_string()),
                ("custom-key".to_string(), "custom-value".to_string()),
                ("priority".to_string(), "u=0, i".to_string()),
            ]
        );
        assert!(
            decode_fields(&block[..block.len() - 1]).is_none(),
            "cut short"
        );
    }

    #[test]
    fn a_dynamic_table_reference_is_refused() {
        assert!(decode_fields(&[0x02, 0x00, 0x80]).is_none());
        assert!(
            decode_fields(&[0x00, 0x00, 0x80]).is_none(),
            "indexed, dynamic"
        );
        assert!(decode_fields(&[0x00, 0x00, 0x10]).is_none(), "post-base");
    }

    #[test]
    fn encoded_responses_decode_back() {
        let block = encode_fields(302, &[("location", "/after".to_string())]);
        assert_eq!(
            decode_fields(&block).unwrap(),
            [
                (":status".to_string(), "302".to_string()),
                ("location".to_string(), "/after".to_string()),
            ]
        );
    }

    #[test]
    fn varints_round_trip() {
        for v in [
            0,
            63,
            64,
            16383,
            16384,
            (1 << 30) - 1,
            1 << 30,
            (1 << 62) - 1,
        ] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(varint(&mut &out[..]), Some(v));
        }
    }
}
