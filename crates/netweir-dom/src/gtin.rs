//! Product codes as pages write them: GTIN-8, UPC (GTIN-12), EAN
//! (GTIN-13), GTIN-14, ISBN-10 and -13, and ISSN, each checked against
//! its check digit, so a typo or a stray number isn't taken for a code.

/// A product code: its kind (`gtin8`, `upc`, `gtin13`, `gtin14`,
/// `isbn10`, `isbn13` or `issn`) and its digits, without spaces or
/// dashes (an ISBN-10 or ISSN may end in `X`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gtin {
    pub kind: &'static str,
    pub value: String,
}

/// The code in `text`, or None when it isn't one or its check digit is
/// wrong.
pub fn parse(text: &str) -> Option<Gtin> {
    let text = text.trim();
    // A label before the code: "EAN: 4006381333931", "ISBN-13 978...",
    // "gtin13: ...".
    static LABEL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)^(?:ean|upc|gtin|isbn|issn|jan|barcode)(?:-?(?:8|10|12|13|14))?\s*:?\s*",
        )
        .unwrap()
    });
    let text = match LABEL.find(text) {
        Some(m) => &text[m.end()..],
        None => text,
    };
    // Only digits, a final X, and the spaces and dashes codes are written with.
    if text.is_empty()
        || !text
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ' ' | '-' | 'X' | 'x'))
    {
        return None;
    }
    let value: String = text
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let x_at_end = value.find('X').is_none_or(|i| i == value.len() - 1);
    if !x_at_end {
        return None;
    }
    // An ISSN is written NNNN-NNNC; eight bare digits are a GTIN-8.
    let issn_shape = text.len() == 9 && text.as_bytes()[4] == b'-';
    let kind = match value.len() {
        8 if issn_shape => mod11(&value).then_some("issn")?,
        8 => gs1(&value).then_some("gtin8")?,
        10 => mod11(&value).then_some("isbn10")?,
        12 => gs1(&value).then_some("upc")?,
        13 if gs1(&value) && (value.starts_with("978") || value.starts_with("979")) => "isbn13",
        13 => gs1(&value).then_some("gtin13")?,
        14 => gs1(&value).then_some("gtin14")?,
        _ => return None,
    };
    Some(Gtin { kind, value })
}

/// An ISBN-10 as the ISBN-13 for the same book: 978, its first nine
/// digits, and a new check digit.
pub fn isbn13_of(isbn10: &str) -> Option<String> {
    if isbn10.len() != 10 || !mod11(isbn10) {
        return None;
    }
    let mut digits = format!("978{}", &isbn10[..9]);
    let sum: u32 = digits
        .bytes()
        .enumerate()
        .map(|(i, b)| u32::from(b - b'0') * if i % 2 == 1 { 3 } else { 1 })
        .sum();
    digits.push(char::from(b'0' + ((10 - sum % 10) % 10) as u8));
    Some(digits)
}

/// The GS1 check: digits weighted 3 and 1 from the right, check digit
/// included, sum to a multiple of 10.
fn gs1(digits: &str) -> bool {
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let sum: u32 = digits
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, b)| u32::from(b - b'0') * if i % 2 == 1 { 3 } else { 1 })
        .sum();
    sum.is_multiple_of(10)
}

/// The ISBN-10 and ISSN check: digits weighted n down to 1, X as 10 in
/// the last place, sum to a multiple of 11.
fn mod11(code: &str) -> bool {
    let n = code.len();
    let mut sum = 0;
    for (i, c) in code.chars().enumerate() {
        let v = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'X' if i == n - 1 => 10,
            _ => return false,
        };
        sum += v * (n - i) as u32;
    }
    sum.is_multiple_of(11)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(text: &str) -> Option<&'static str> {
        parse(text).map(|g| g.kind)
    }

    #[test]
    fn gs1_codes_are_checked() {
        assert_eq!(kind("4006381333931"), Some("gtin13"));
        assert_eq!(kind("4006381333932"), None, "wrong check digit");
        assert_eq!(kind("036000291452"), Some("upc"));
        assert_eq!(kind("96385074"), Some("gtin8"));
        assert_eq!(kind("10012345678902"), Some("gtin14"));
        assert_eq!(
            parse(" 4006 3813 3393-1 "),
            Some(Gtin {
                kind: "gtin13",
                value: "4006381333931".into()
            })
        );
    }

    #[test]
    fn isbns_and_issns() {
        assert_eq!(kind("0-306-40615-2"), Some("isbn10"));
        assert_eq!(kind("080442957X"), Some("isbn10"));
        assert_eq!(kind("0-306-40615-3"), None);
        assert_eq!(kind("978-0-306-40615-7"), Some("isbn13"));
        assert_eq!(kind("ISBN 978-0-306-40615-7"), Some("isbn13"));
        assert_eq!(kind("EAN: 4006381333931"), Some("gtin13"));
        assert_eq!(kind("UPC 036000291452"), Some("upc"));
        assert_eq!(kind("gtin13: 4006381333931"), Some("gtin13"));
        assert_eq!(kind("0378-5955"), Some("issn"));
        assert_eq!(kind("2049-3630"), Some("issn"));
        assert_eq!(kind("0378-5956"), None);
    }

    #[test]
    fn other_numbers_are_not_codes() {
        for text in [
            "",
            "12345",
            "123456789012345",
            "abc",
            "40063813339310",
            "19.99",
        ] {
            assert_eq!(kind(text), None, "{text}");
        }
    }
}
