//! Bytes to text, finding the encoding the way a browser does: byte order
//! mark, then the Content-Type header, then a `<meta>` charset near the
//! top of the page, then a guess from the bytes themselves.

use encoding_rs::{Encoding, UTF_8};

pub fn decode(body: &[u8], content_type: Option<&str>) -> String {
    match encoding_of(body, content_type) {
        Found::Utf32 { big_endian } => utf32(body, big_endian),
        // `decode` strips a BOM and replaces malformed sequences with U+FFFD.
        Found::Encoding(encoding) => encoding.decode(body).0.into_owned(),
    }
}

/// The name of the encoding `decode` reads `body` with, as Python's
/// codecs know most of them: "UTF-8", "windows-1252", "utf-32-le".
pub fn encoding_name(body: &[u8], content_type: Option<&str>) -> &'static str {
    match encoding_of(body, content_type) {
        Found::Utf32 { big_endian: true } => "utf-32-be",
        Found::Utf32 { big_endian: false } => "utf-32-le",
        Found::Encoding(e) => e.name(),
    }
}

/// `text` in the encoding named `name`, as a browser would write it (the
/// WHATWG encoders, which Python's codecs don't always agree with, for
/// Big5 and EUC-KR among others), so that `decode` with that encoding
/// gives it back. UTF-16 and UTF-32 get a byte order mark, which is how
/// they're found again. None for an encoding that can't hold the text, or
/// that isn't one.
pub fn encode(text: &str, name: &str) -> Option<Vec<u8>> {
    let utf32 = |big_endian: bool| {
        let mut out = Vec::with_capacity(4 + text.len() * 4);
        for c in std::iter::once('\u{feff}').chain(text.chars()) {
            let n = c as u32;
            out.extend_from_slice(&if big_endian {
                n.to_be_bytes()
            } else {
                n.to_le_bytes()
            });
        }
        out
    };
    match name.to_ascii_lowercase().as_str() {
        "utf-32-be" | "utf-32be" => return Some(utf32(true)),
        "utf-32-le" | "utf-32le" | "utf-32" => return Some(utf32(false)),
        _ => {}
    }
    let encoding = Encoding::for_label(name.as_bytes())?;
    if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE {
        let big_endian = encoding == encoding_rs::UTF_16BE;
        let mut out = Vec::with_capacity(2 + text.len() * 2);
        for unit in std::iter::once(0xFEFF).chain(text.encode_utf16()) {
            out.extend_from_slice(&if big_endian {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            });
        }
        return Some(out);
    }
    let (bytes, used, unmappable) = encoding.encode(text);
    (used == encoding && !unmappable).then(|| bytes.into_owned())
}

enum Found {
    Encoding(&'static Encoding),
    /// encoding_rs has no UTF-32, which XML allows.
    Utf32 {
        big_endian: bool,
    },
}

fn encoding_of(body: &[u8], content_type: Option<&str>) -> Found {
    // UTF-32 first: its little-endian BOM starts like UTF-16's.
    if let Some(big_endian) = utf32_marks(body) {
        return Found::Utf32 { big_endian };
    }
    Found::Encoding(
        Encoding::for_bom(body)
            .map(|(e, _)| e)
            .or_else(|| utf16_marks(body))
            .or_else(|| content_type.and_then(charset_param))
            .or_else(|| meta_charset(body))
            .unwrap_or_else(|| guess(body)),
    )
}

/// UTF-32 by its byte order mark, or by `<` as the first character, as
/// appendix F of the XML spec detects it.
fn utf32_marks(body: &[u8]) -> Option<bool> {
    match body.get(..4)? {
        [0, 0, 0xFE, 0xFF] | [0, 0, 0, b'<'] => Some(true),
        [0xFF, 0xFE, 0, 0] | [b'<', 0, 0, 0] => Some(false),
        _ => None,
    }
}

/// UTF-16 without a byte order mark, by `<?` as appendix F of the XML
/// spec detects it: an XML declaration, in the only bytes it can start
/// with in UTF-16.
fn utf16_marks(body: &[u8]) -> Option<&'static Encoding> {
    match body.get(..4)? {
        [b'<', 0, b'?', 0] => Some(encoding_rs::UTF_16LE),
        [0, b'<', 0, b'?'] => Some(encoding_rs::UTF_16BE),
        _ => None,
    }
}

fn utf32(body: &[u8], big_endian: bool) -> String {
    let mut out = String::with_capacity(body.len() / 4);
    let mut chunks = body.chunks(4);
    // A byte order mark isn't text.
    if matches!(body.get(..4), Some([0, 0, 0xFE, 0xFF] | [0xFF, 0xFE, 0, 0])) {
        chunks.next();
    }
    for c in chunks {
        let n = match (c, big_endian) {
            (&[a, b, c, d], true) => u32::from_be_bytes([a, b, c, d]),
            (&[a, b, c, d], false) => u32::from_le_bytes([a, b, c, d]),
            _ => 0xFFFD,
        };
        out.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
    }
    out
}

fn charset_param(content_type: &str) -> Option<&'static Encoding> {
    content_type.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| Encoding::for_label(v.trim().trim_matches(['"', '\'']).as_bytes()))
            .flatten()
    })
}

/// The encoding a page declares near its top: an XML declaration's
/// `encoding`, or a `<meta charset>` / `<meta content="...; charset=...">`
/// in the first 1024 bytes, as the HTML spec's prescan reads them.
fn meta_charset(body: &[u8]) -> Option<&'static Encoding> {
    let head = &body[..body.len().min(1024)];
    let lower = head.to_ascii_lowercase();
    if lower.starts_with(b"<?xml") {
        let end = find(&lower, b"?>").unwrap_or(lower.len());
        if let Some(label) = attribute(&lower[..end], b"encoding") {
            return Encoding::for_label(label).map(not_utf16);
        }
    }
    let mut from = 0;
    while let Some(i) = find(&lower[from..], b"<meta") {
        let tag_start = from + i + b"<meta".len();
        let tag_end = lower[tag_start..]
            .iter()
            .position(|&b| b == b'>')
            .map_or(lower.len(), |p| tag_start + p);
        let tag = &lower[tag_start..tag_end];
        let label = attribute(tag, b"charset").or_else(|| {
            let content = attribute(tag, b"content")?;
            let at = find(content, b"charset")?;
            let rest = &content[at + b"charset".len()..];
            let rest = rest
                .iter()
                .position(|&b| b == b'=')
                .map(|p| &rest[p + 1..])?;
            let start = rest
                .iter()
                .position(|b| !b.is_ascii_whitespace() && *b != b'"' && *b != b'\'')?;
            let rest = &rest[start..];
            let end = rest
                .iter()
                .position(|b| b.is_ascii_whitespace() || matches!(b, b';' | b'"' | b'\''))
                .unwrap_or(rest.len());
            Some(&rest[..end])
        });
        if let Some(e) = label.and_then(Encoding::for_label) {
            return Some(not_utf16(e));
        }
        from = tag_end;
    }
    None
}

/// A page read as ASCII here can't really be UTF-16, whatever it says.
fn not_utf16(e: &'static Encoding) -> &'static Encoding {
    if e == encoding_rs::UTF_16LE || e == encoding_rs::UTF_16BE {
        UTF_8
    } else {
        e
    }
}

/// The value of `name=` inside one tag (already lowercased), quoted or not.
fn attribute<'a>(tag: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    attribute_range(tag, name).map(|r| &tag[r])
}

/// Where `name=`'s value sits inside `tag` (lowercased), as a byte range,
/// so the value can be read from the original, case intact.
fn attribute_range(tag: &[u8], name: &[u8]) -> Option<std::ops::Range<usize>> {
    let mut from = 0;
    while let Some(i) = find(&tag[from..], name) {
        let at = from + i;
        let before_ok = at == 0 || tag[at - 1].is_ascii_whitespace() || tag[at - 1] == b'/';
        let mut j = at + name.len();
        while tag.get(j).is_some_and(|b| b.is_ascii_whitespace()) {
            j += 1;
        }
        if before_ok && tag.get(j) == Some(&b'=') {
            j += 1;
            while tag.get(j).is_some_and(|b| b.is_ascii_whitespace()) {
                j += 1;
            }
            return Some(match tag.get(j) {
                Some(&q @ (b'"' | b'\'')) => {
                    let start = j + 1;
                    start
                        ..tag[start..]
                            .iter()
                            .position(|&b| b == q)
                            .map_or(tag.len(), |p| start + p)
                }
                _ => {
                    j..tag[j..]
                        .iter()
                        .position(|b| b.is_ascii_whitespace() || *b == b'/')
                        .map_or(tag.len(), |p| j + p)
                }
            });
        }
        from = at + 1;
    }
    None
}

/// The `content` of the first `<meta name="...">` with this name (compared
/// without case) in the first 64 KiB of a page.
pub fn meta_content(body: &[u8], name: &str) -> Option<String> {
    let head = &body[..body.len().min(64 * 1024)];
    let lower = head.to_ascii_lowercase();
    let want = name.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = find(&lower[from..], b"<meta") {
        let start = from + i + b"<meta".len();
        let end = lower[start..]
            .iter()
            .position(|&b| b == b'>')
            .map_or(lower.len(), |p| start + p);
        let tag = &lower[start..end];
        if attribute(tag, b"name").is_some_and(|n| n == want.as_bytes())
            && let Some(r) = attribute_range(tag, b"content")
        {
            return Some(
                String::from_utf8_lossy(&head[start + r.start..start + r.end]).into_owned(),
            );
        }
        from = end;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn guess(body: &[u8]) -> &'static Encoding {
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(body, true);
    detector.guess(None, chardetng::Utf8Detection::Allow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_and_utf32_without_a_byte_order_mark_are_found_by_the_declaration() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><r>Zoë €</r>";
        let le: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let be: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(decode(&le, Some("application/xml")), text);
        assert_eq!(decode(&be, None), text);
        assert_eq!(encoding_name(&le, None), "UTF-16LE");
        let le32: Vec<u8> = text
            .chars()
            .flat_map(|c| (c as u32).to_le_bytes())
            .collect();
        let be32: Vec<u8> = text
            .chars()
            .flat_map(|c| (c as u32).to_be_bytes())
            .collect();
        assert_eq!(decode(&le32, None), text);
        assert_eq!(decode(&be32, None), text);
        assert_eq!(encoding_name(&be32, None), "utf-32-be");
        let mut bom = vec![0xFF, 0xFE, 0, 0];
        bom.extend_from_slice(&le32);
        assert_eq!(decode(&bom, None), text);
    }

    #[test]
    fn encoding_gives_what_decoding_reads_back() {
        for (text, name) in [
            ("繁體中文 測試", "big5"),
            ("한국어 텍스트", "euc-kr"),
            ("日本語", "shift_jis"),
            ("Zoë €", "windows-1252"),
            ("Zoë", "UTF-16LE"),
            ("Zoë", "utf-32-be"),
        ] {
            let bytes = encode(text, name).unwrap_or_else(|| panic!("{name}"));
            let content_type = format!("text/plain; charset={name}");
            let content_type = (!name.starts_with("utf-32")).then_some(content_type.as_str());
            assert_eq!(decode(&bytes, content_type), text, "{name}");
        }
        assert_eq!(encode("€ 日本", "iso-8859-2"), None);
        assert_eq!(encode("x", "no-such-encoding"), None);
    }

    #[test]
    fn utf8_by_default() {
        assert_eq!(decode("café".as_bytes(), None), "café");
    }

    #[test]
    fn header_charset_wins_over_meta() {
        let body = b"<meta charset=utf-8>caf\xe9";
        assert_eq!(
            decode(body, Some("text/html; charset=ISO-8859-1")),
            "<meta charset=utf-8>café"
        );
    }

    #[test]
    fn meta_charset_is_found() {
        let body = b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1251\"></head>\xcf\xf0\xe8\xe2\xe5\xf2";
        assert!(decode(body, Some("text/html")).ends_with("Привет"));
        assert_eq!(
            decode(b"<meta charset='shift_jis'>\x93\xfa\x96\x7b", None),
            "<meta charset='shift_jis'>日本"
        );
    }

    #[test]
    fn charset_outside_a_meta_tag_is_ignored() {
        // A comment, a script and a <link charset> all mention charset=;
        // only <meta> declares the page's encoding.
        let body = "<!-- charset=windows-1251 --><script>var s='charset=koi8-r'</script>\
                    <link rel=stylesheet charset=shift_jis href=a.css><p>café</p>";
        assert_eq!(decode(body.as_bytes(), None), body);
    }

    #[test]
    fn xml_declaration_names_the_encoding() {
        // 0xe1 is alpha in ISO-8859-7 but a-acute in Latin-1, which is what
        // a guess from the bytes alone would pick.
        let mut body = b"<?xml version=\"1.0\" encoding=\"ISO-8859-7\"?><feed>".to_vec();
        body.push(0xe1);
        body.extend_from_slice(b"</feed>");
        assert!(decode(&body, Some("application/xml")).contains("<feed>\u{3b1}</feed>"));
    }

    #[test]
    fn meta_content_type_without_charset_is_skipped() {
        let mut body = b"<meta http-equiv=refresh content=\"5\"><meta charset=latin1>".to_vec();
        body.push(0xe9);
        assert!(decode(&body, None).ends_with('é'));
    }

    #[test]
    fn meta_content_keeps_the_value_case() {
        let page = br#"<head><meta charset=utf-8><meta NAME="tdm-policy" content="https://Example.com/P"><meta name=x content=y>"#;
        assert_eq!(
            meta_content(page, "TDM-Policy").as_deref(),
            Some("https://Example.com/P")
        );
        assert_eq!(meta_content(page, "x").as_deref(), Some("y"));
        assert_eq!(meta_content(page, "missing"), None);
    }

    #[test]
    fn bom_wins_over_everything() {
        let body = b"\xef\xbb\xbf<meta charset=latin1>\xc3\xa9";
        assert_eq!(
            decode(body, Some("text/html; charset=latin1")),
            "<meta charset=latin1>é"
        );
    }

    #[test]
    fn unlabelled_legacy_text_is_guessed() {
        let (bytes, _, _) = encoding_rs::WINDOWS_1251
            .encode("Это обычная русская страница без указания кодировки.");
        assert_eq!(
            decode(&bytes, None),
            "Это обычная русская страница без указания кодировки."
        );
    }

    #[test]
    fn bad_bytes_become_replacement_characters() {
        assert_eq!(
            decode(b"<meta charset=utf-8>a\xffb", None),
            "<meta charset=utf-8>a\u{fffd}b"
        );
    }

    #[test]
    fn quoted_and_unknown_header_charsets() {
        assert_eq!(
            decode(b"caf\xe9", Some("text/html; charset=\"latin1\"")),
            "café"
        );
        assert_eq!(
            decode("café".as_bytes(), Some("text/html; charset=klingon")),
            "café"
        );
    }
}
