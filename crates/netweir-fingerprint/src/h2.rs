//! Just enough HTTP/2 to record what a client sends on a connection and
//! answer each request.

use std::collections::HashMap;
use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

const DATA: u8 = 0x0;
const HEADERS: u8 = 0x1;
const PRIORITY: u8 = 0x2;
const SETTINGS: u8 = 0x4;
const PING: u8 = 0x6;
const GOAWAY: u8 = 0x7;
const WINDOW_UPDATE: u8 = 0x8;
const CONTINUATION: u8 = 0x9;

const END_STREAM: u8 = 0x1;
const ACK: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const PADDED: u8 = 0x8;
const PRIORITY_FLAG: u8 = 0x20;

/// A stream dependency: (exclusive, depends on, weight as on the wire).
type Dependency = (bool, u32, u8);

/// A request whose headers have arrived and whose body is still coming.
type Pending = (Option<Dependency>, Vec<(String, String)>, Vec<u8>);

/// What the client said about the connection, plus the priority it put on
/// one request's HEADERS frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Http2 {
    /// SETTINGS as sent, in order.
    pub settings: Vec<(u16, u32)>,
    /// The connection-level WINDOW_UPDATE increment, 0 if none was sent.
    pub window_update: u32,
    /// PRIORITY frames before the first request: (stream, exclusive, depends on, weight).
    pub priority_frames: Vec<(u32, bool, u32, u8)>,
    /// Priority carried on this request's HEADERS frame, if any.
    pub headers_priority: Option<Dependency>,
    /// The Akamai HTTP/2 fingerprint string, from the connection's opening
    /// frames and this request's pseudo-header order.
    pub akamai: String,
}

/// One request read from the connection.
pub struct Request {
    pub http2: Http2,
    pub headers: Vec<(String, String)>,
    /// The DATA frames' payload, for a request that has one.
    pub body: Vec<u8>,
}

/// A response to send on a stream.
pub struct Reply<'a> {
    pub status: u16,
    pub headers: &'a [(&'a str, String)],
    pub body: &'a [u8],
}

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

async fn read_frame<S: AsyncRead + Unpin>(s: &mut S) -> io::Result<Frame> {
    let mut head = [0u8; 9];
    s.read_exact(&mut head).await?;
    let len = (head[0] as usize) << 16 | (head[1] as usize) << 8 | head[2] as usize;
    if len > 1 << 20 {
        return Err(invalid("frame too large"));
    }
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).await?;
    Ok(Frame {
        kind: head[3],
        flags: head[4],
        stream: u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff,
        payload,
    })
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut out = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// The dependency in a 5-byte priority block.
fn dependency(bytes: &[u8]) -> io::Result<Dependency> {
    let b: &[u8; 5] = bytes
        .get(..5)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid("short priority"))?;
    let raw = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    Ok((raw >> 31 == 1, raw & 0x7fff_ffff, b[4]))
}

/// The header block fragment of a HEADERS frame, with padding and priority
/// removed.
fn header_fragment(f: &Frame) -> io::Result<(Option<Dependency>, &[u8])> {
    let mut p = &f.payload[..];
    let mut pad = 0;
    if f.flags & PADDED != 0 {
        pad = *p.first().ok_or_else(|| invalid("short padded frame"))? as usize;
        p = &p[1..];
    }
    let mut priority = None;
    if f.flags & PRIORITY_FLAG != 0 {
        priority = Some(dependency(p)?);
        p = &p[5..];
    }
    let end = p
        .len()
        .checked_sub(pad)
        .ok_or_else(|| invalid("padding longer than frame"))?;
    Ok((priority, &p[..end]))
}

/// Reads one HTTP/2 connection, calling `on_request` for each request and
/// sending back what it returns, until the client closes or goes away.
pub async fn serve<S, F>(s: &mut S, mut on_request: F) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(Request) -> (u16, Vec<(&'static str, String)>, Vec<u8>),
{
    let mut preface = [0u8; 24];
    s.read_exact(&mut preface).await?;
    if preface != PREFACE {
        return Err(invalid("bad preface"));
    }
    // Our side of the connection: default settings.
    s.write_all(&frame(SETTINGS, 0, 0, &[])).await?;

    let mut settings: Option<Vec<(u16, u32)>> = None;
    let mut window_update = 0;
    let mut priority_frames = Vec::new();
    let mut seen_request = false;
    // HPACK state lives for the whole connection.
    let mut decoder = fluke_hpack::Decoder::new();
    let mut open: HashMap<u32, (Option<Dependency>, Vec<u8>, bool)> = HashMap::new();
    // Requests whose headers have arrived and whose body hasn't finished.
    let mut bodies: HashMap<u32, Pending> = HashMap::new();

    loop {
        let f = match read_frame(s).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let complete = match f.kind {
            SETTINGS if f.flags & ACK == 0 => {
                let pairs = f
                    .payload
                    .as_chunks::<6>()
                    .0
                    .iter()
                    .map(|c| {
                        (
                            u16::from_be_bytes([c[0], c[1]]),
                            u32::from_be_bytes([c[2], c[3], c[4], c[5]]),
                        )
                    })
                    .collect();
                settings.get_or_insert(pairs);
                s.write_all(&frame(SETTINGS, ACK, 0, &[])).await?;
                None
            }
            WINDOW_UPDATE if f.stream == 0 && !seen_request => {
                let b: [u8; 4] = f
                    .payload
                    .get(..4)
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| invalid("short window update"))?;
                window_update = u32::from_be_bytes(b) & 0x7fff_ffff;
                None
            }
            PRIORITY if !seen_request => {
                let (excl, dep, w) = dependency(&f.payload)?;
                priority_frames.push((f.stream, excl, dep, w));
                None
            }
            PING if f.flags & ACK == 0 => {
                s.write_all(&frame(PING, ACK, 0, &f.payload)).await?;
                None
            }
            GOAWAY => return Ok(()),
            HEADERS => {
                let (priority, fragment) = header_fragment(&f)?;
                let ends = f.flags & END_STREAM != 0;
                open.insert(f.stream, (priority, fragment.to_vec(), ends));
                (f.flags & END_HEADERS != 0).then_some(f.stream)
            }
            DATA => {
                let mut p = &f.payload[..];
                if f.flags & PADDED != 0 {
                    let pad = *p.first().ok_or_else(|| invalid("short padded frame"))? as usize;
                    p = p
                        .get(1..p.len().saturating_sub(pad))
                        .ok_or_else(|| invalid("padding longer than frame"))?;
                }
                if let Some(entry) = bodies.get_mut(&f.stream) {
                    entry.2.extend_from_slice(p);
                }
                // Keeps a long body coming: the window it used is open again.
                if !p.is_empty() {
                    let n = (p.len() as u32).to_be_bytes();
                    s.write_all(&frame(WINDOW_UPDATE, 0, 0, &n)).await?;
                    s.write_all(&frame(WINDOW_UPDATE, 0, f.stream, &n)).await?;
                }
                if f.flags & END_STREAM != 0
                    && let Some((priority, headers, body)) = bodies.remove(&f.stream)
                {
                    let r = request_of(
                        &settings,
                        window_update,
                        &priority_frames,
                        priority,
                        headers,
                        body,
                    );
                    let (status, reply_headers, body) = on_request(r);
                    respond(
                        s,
                        f.stream,
                        &Reply {
                            status,
                            headers: &reply_headers,
                            body: &body,
                        },
                    )
                    .await?;
                }
                None
            }
            CONTINUATION => {
                let entry = open
                    .get_mut(&f.stream)
                    .ok_or_else(|| invalid("CONTINUATION without HEADERS"))?;
                entry.1.extend_from_slice(&f.payload);
                (f.flags & END_HEADERS != 0).then_some(f.stream)
            }
            _ => None,
        };
        let Some(stream) = complete else { continue };
        let (headers_priority, block, ends) = open.remove(&stream).unwrap_or_default();
        let headers: Vec<(String, String)> = decoder
            .decode(&block)
            .map_err(|_| invalid("bad HPACK block"))?
            .into_iter()
            .map(|(k, v)| {
                (
                    String::from_utf8_lossy(&k).into_owned(),
                    String::from_utf8_lossy(&v).into_owned(),
                )
            })
            .collect();
        seen_request = true;
        if !ends {
            // Answered once the body has all arrived.
            bodies.insert(stream, (headers_priority, headers, Vec::new()));
            continue;
        }
        let request = request_of(
            &settings,
            window_update,
            &priority_frames,
            headers_priority,
            headers,
            Vec::new(),
        );
        let (status, reply_headers, body) = on_request(request);
        respond(
            s,
            stream,
            &Reply {
                status,
                headers: &reply_headers,
                body: &body,
            },
        )
        .await?;
    }
}

fn request_of(
    settings: &Option<Vec<(u16, u32)>>,
    window_update: u32,
    priority_frames: &[(u32, bool, u32, u8)],
    headers_priority: Option<Dependency>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Request {
    let settings_now = settings.clone().unwrap_or_default();
    let akamai = akamai(&settings_now, window_update, priority_frames, &headers);
    Request {
        http2: Http2 {
            settings: settings_now,
            window_update,
            priority_frames: priority_frames.to_vec(),
            headers_priority,
            akamai,
        },
        headers,
        body,
    }
}

fn akamai(
    settings: &[(u16, u32)],
    window_update: u32,
    priority: &[(u32, bool, u32, u8)],
    headers: &[(String, String)],
) -> String {
    let settings = settings
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join(";");
    let priority = if priority.is_empty() {
        "0".to_string()
    } else {
        // The wire carries weight - 1; the Akamai format prints the weight.
        priority
            .iter()
            .map(|(s, e, d, w)| format!("{s}:{}:{d}:{}", *e as u8, *w as u32 + 1))
            .collect::<Vec<_>>()
            .join(",")
    };
    let pseudo = headers
        .iter()
        .filter_map(|(k, _)| k.strip_prefix(':').and_then(|n| n.chars().next()))
        .map(String::from)
        .collect::<Vec<_>>()
        .join(",");
    format!("{settings}|{window_update}|{priority}|{pseudo}")
}

/// HPACK integer with an `n`-bit prefix (RFC 7541 §5.1); `flags` fills the
/// bits above the prefix in the first byte.
pub(crate) fn hpack_int(out: &mut Vec<u8>, flags: u8, n: u32, mut value: usize) {
    let max = (1usize << n) - 1;
    if value < max {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | max as u8);
    value -= max;
    while value >= 128 {
        out.push((value % 128) as u8 | 0x80);
        value /= 128;
    }
    out.push(value as u8);
}

pub(crate) fn hpack_string(out: &mut Vec<u8>, s: &[u8]) {
    hpack_int(out, 0, 7, s.len());
    out.extend_from_slice(s);
}

/// Encodes a response header block using only literals without indexing,
/// so it needs no encoder state.
fn encode_headers(status: u16, headers: &[(&str, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    // ":status" is static table entry 8; literal value, indexed name.
    hpack_int(&mut out, 0x00, 4, 8);
    hpack_string(&mut out, status.to_string().as_bytes());
    for (name, value) in headers {
        out.push(0x00);
        hpack_string(&mut out, name.as_bytes());
        hpack_string(&mut out, value.as_bytes());
    }
    out
}

async fn respond<S: AsyncWrite + Unpin>(
    s: &mut S,
    stream: u32,
    reply: &Reply<'_>,
) -> io::Result<()> {
    let block = encode_headers(reply.status, reply.headers);
    let end = if reply.body.is_empty() { END_STREAM } else { 0 };
    let mut out = frame(HEADERS, END_HEADERS | end, stream, &block);
    let chunks: Vec<&[u8]> = reply.body.chunks(16_384).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let last = i + 1 == chunks.len();
        out.extend(frame(
            DATA,
            if last { END_STREAM } else { 0 },
            stream,
            chunk,
        ));
    }
    s.write_all(&out).await?;
    s.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hpack_integers_match_rfc_7541_examples() {
        // C.1.1: 10 with a 5-bit prefix; C.1.2: 1337 with a 5-bit prefix.
        let mut out = Vec::new();
        hpack_int(&mut out, 0, 5, 10);
        assert_eq!(out, [0x0a]);
        out.clear();
        hpack_int(&mut out, 0, 5, 1337);
        assert_eq!(out, [0x1f, 0x9a, 0x0a]);
    }

    #[test]
    fn encoded_headers_decode_back() {
        let block = encode_headers(
            302,
            &[
                ("location", "https://x/".into()),
                ("set-cookie", "a=1".into()),
            ],
        );
        let decoded = fluke_hpack::Decoder::new().decode(&block).unwrap();
        let decoded: Vec<(String, String)> = decoded
            .into_iter()
            .map(|(k, v)| (String::from_utf8(k).unwrap(), String::from_utf8(v).unwrap()))
            .collect();
        assert_eq!(
            decoded,
            [
                (":status".to_string(), "302".to_string()),
                ("location".to_string(), "https://x/".to_string()),
                ("set-cookie".to_string(), "a=1".to_string())
            ]
        );
    }

    #[test]
    fn malformed_headers_frames_are_errors_not_panics() {
        for (flags, payload) in [
            (PADDED, vec![]),
            (PADDED, vec![9, 1, 2]),
            (PRIORITY_FLAG, vec![0, 0]),
            (PADDED | PRIORITY_FLAG, vec![0, 1, 2]),
        ] {
            let f = Frame {
                kind: HEADERS,
                flags,
                stream: 1,
                payload,
            };
            assert!(header_fragment(&f).is_err());
        }
    }
}
