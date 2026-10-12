//! Prices as shops write them: `£1,234.50`, `1.234,50 €`, `From USD 1,099`.

/// A price: the amount as exact decimal digits (`"1234.50"`, never a float)
/// and the currency as written next to it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Price {
    pub amount: String,
    pub currency: Option<String>,
}

/// Currency symbols and local abbreviations, with the ISO 4217 code each
/// one stands for when it stands for only one. Longer forms come first so
/// `US$` wins over `$`.
const SYMBOLS: &[(&str, Option<&str>)] = &[
    ("US$", Some("USD")),
    ("CA$", Some("CAD")),
    ("AU$", Some("AUD")),
    ("NZ$", Some("NZD")),
    ("HK$", Some("HKD")),
    ("MX$", Some("MXN")),
    ("NT$", Some("TWD")),
    ("C$", Some("CAD")),
    ("A$", Some("AUD")),
    ("S$", Some("SGD")),
    ("R$", Some("BRL")),
    ("руб", Some("RUB")),
    ("лв", Some("BGN")),
    ("lei", Some("RON")),
    ("zł", Some("PLN")),
    ("Kč", Some("CZK")),
    ("Ft", Some("HUF")),
    ("Rp", Some("IDR")),
    ("RM", Some("MYR")),
    // Krona, krone and króna: Swedish, Norwegian, Danish or Icelandic.
    ("kr", None),
    ("Kr", None),
    // Rupees: Indian, Pakistani, Sri Lankan or Nepalese.
    ("Rs.", None),
    ("Rs", None),
    ("₨", None),
    ("$", None),
    ("£", Some("GBP")),
    ("€", Some("EUR")),
    // Yen or yuan.
    ("¥", None),
    ("￥", None),
    ("元", Some("CNY")),
    ("円", Some("JPY")),
    ("₹", Some("INR")),
    ("₽", Some("RUB")),
    ("₩", Some("KRW")),
    ("원", Some("KRW")),
    ("₪", Some("ILS")),
    ("₺", Some("TRY")),
    ("₫", Some("VND")),
    ("₱", Some("PHP")),
    ("₴", Some("UAH")),
    ("₦", Some("NGN")),
    ("฿", Some("THB")),
    ("₸", Some("KZT")),
    ("₾", Some("GEL")),
    ("₼", Some("AZN")),
    ("₵", Some("GHS")),
    ("₡", Some("CRC")),
    ("₲", Some("PYG")),
];

/// ISO 4217 codes recognised next to a number, written in capitals.
const CODES: &[&str] = &[
    "AED", "ARS", "AUD", "BDT", "BGN", "BHD", "BRL", "CAD", "CHF", "CLP", "CNY", "COP", "CZK",
    "DKK", "EGP", "EUR", "GBP", "GEL", "HKD", "HUF", "IDR", "ILS", "INR", "ISK", "JOD", "JPY",
    "KES", "KRW", "KWD", "KZT", "LKR", "MAD", "MXN", "MYR", "NGN", "NOK", "NZD", "OMR", "PEN",
    "PHP", "PKR", "PLN", "QAR", "RON", "RSD", "RUB", "SAR", "SEK", "SGD", "THB", "TRY", "TWD",
    "UAH", "USD", "UYU", "VND", "ZAR",
];

/// The ISO 4217 code for a currency as written: a code is itself, a
/// symbol is its code when it belongs to one currency only (`£` is GBP;
/// `$`, `¥` and `kr` are None).
pub fn currency_code(currency: &str) -> Option<&'static str> {
    let currency = currency.trim();
    if let Some(code) = CODES.iter().find(|c| c.eq_ignore_ascii_case(currency)) {
        return Some(code);
    }
    SYMBOLS
        .iter()
        .find(|(s, _)| *s == currency)
        .and_then(|(_, code)| *code)
}

/// The first price in `text`, or None when there's no number in it.
///
/// The decimal separator is worked out from the text: the last of `.` and
/// `,` is the decimal point when one or two digits follow it, or when the
/// other separator appears before it. Three digits after a lone separator
/// make it a thousands separator (`1,234` and `1.234` are both 1234), as
/// long as the groups are well formed: `0.500` and `12345.678` have no
/// other reading than a decimal one, so they keep it. Spaces, thin spaces
/// and apostrophes group thousands too, and so do Indian lakhs and crores
/// (`1,23,456`).
///
/// A `-` or `−` straight before the number, or before a currency straight
/// before it (`-£12.00`), makes the amount negative.
pub fn parse_price(text: &str) -> Option<Price> {
    let text = narrow(text);
    let text = text.as_str();
    let mut start = text.find(|c: char| c.is_ascii_digit())?;
    let end = number_end(text, start);
    // ".99" is 0.99, but in "Rs.499" the point belongs to the currency.
    if text[..start].ends_with('.') && !is_letter(text[..start - 1].chars().next_back()) {
        start -= 1;
    }
    let digits: String = text[start..end]
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    let before = &text[..start];
    let mut currency = currency_before(before);
    let mut minus = false;
    if let Some(c) = currency {
        // "-£12": the sign before the currency (which ends `before`, give
        // or take spaces).
        minus = negative(&before[..before.trim_end().len() - c.len()]);
    } else if let Some(rest) = before.strip_suffix(['-', '\u{2212}']) {
        // "£-5": a currency, then the sign.
        currency = currency_before(rest);
        minus = currency.is_some() || negative(before);
    }
    if currency.is_none() {
        currency = currency_after(skip_dash_cents(&text[end..]));
    }
    let mut amount = amount(&digits);
    if minus {
        amount.insert(0, '-');
    }
    Some(Price {
        amount,
        currency: currency.map(str::to_string),
    })
}

/// Full-width digits and separators (`１，２３４`) as ASCII.
fn narrow(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '０'..='９' => char::from(b'0' + (c as u32 - '０' as u32) as u8),
            '，' => ',',
            '．' => '.',
            '－' => '-',
            other => other,
        })
        .collect()
}

/// Whether the text ends with a minus sign standing on its own: at the
/// start, after a space or after an opening bracket. Not the hyphen in
/// "2-for-", nor the ":-" that labels a price in "MRP:-₹499".
fn negative(before: &str) -> bool {
    let Some(rest) = before.strip_suffix(['-', '\u{2212}']) else {
        return false;
    };
    match rest.chars().next_back() {
        None => true,
        Some(c) => c.is_whitespace() || c == '(' || c == '[',
    }
}

/// Skips the `,-` or `.–` that some shops write for "no cents" (`299,- kr`).
fn skip_dash_cents(text: &str) -> &str {
    for sep in [',', '.'] {
        for dash in ['-', '\u{2013}', '\u{2014}'] {
            if let Some(rest) = text.strip_prefix(sep).and_then(|t| t.strip_prefix(dash)) {
                return rest;
            }
        }
    }
    text
}

/// Characters that group thousands when exactly three digits follow.
fn is_group_space(c: char) -> bool {
    matches!(
        c,
        ' ' | '\u{a0}' | '\u{2009}' | '\u{202f}' | '\u{3000}' | '\'' | '\u{2019}'
    )
}

/// Where the number starting at `start` ends.
fn number_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let digit_at = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
    let mut i = start;
    loop {
        if digit_at(i) {
            i += 1;
            continue;
        }
        let Some(c) = text[i..].chars().next() else {
            return i;
        };
        let next = i + c.len_utf8();
        let separator = (c == '.' || c == ',') && digit_at(next);
        let group = is_group_space(c) && (0..3).all(|k| digit_at(next + k)) && !digit_at(next + 3);
        if !separator && !group {
            return i;
        }
        i = next;
    }
}

/// The amount as decimal digits from a number of digits, `.` and `,`.
fn amount(number: &str) -> String {
    let Some(last) = number.rfind(['.', ',']) else {
        return number.to_string();
    };
    let sep = number.as_bytes()[last] as char;
    let other = if sep == '.' { ',' } else { '.' };
    let after = number.len() - last - 1;
    let whole: String = number[..last]
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let whole = if whole.is_empty() { "0".into() } else { whole };
    let decimal = after <= 2 || number[..last].contains(other) || !thousands(number, sep);
    if decimal {
        format!("{whole}.{}", &number[last + 1..])
    } else {
        number.chars().filter(char::is_ascii_digit).collect()
    }
}

/// Whether `sep` reads as a thousands separator throughout `number`: a
/// leading group of one to three digits that isn't zero, then groups of
/// three.
fn thousands(number: &str, sep: char) -> bool {
    let groups: Vec<&str> = number.split(sep).collect();
    let (lead, rest) = groups.split_first().unwrap_or((&"", &[]));
    if lead.trim_start_matches('0').is_empty() {
        return false;
    }
    let western = lead.len() <= 3 && rest.iter().all(|g| g.len() == 3);
    // Lakhs and crores: 1,23,456 and 1,00,00,000.
    let indian = sep == ','
        && lead.len() <= 2
        && rest
            .split_last()
            .is_some_and(|(last, middle)| last.len() == 3 && middle.iter().all(|g| g.len() == 2));
    western || indian
}

fn is_letter(c: Option<char>) -> bool {
    c.is_some_and(char::is_alphabetic)
}

/// A currency written just before the number (spaces between allowed).
fn currency_before(text: &str) -> Option<&str> {
    let text = text.trim_end_matches(|c: char| c.is_whitespace());
    let ok = |s: &str| {
        let rest = &text[..text.len() - s.len()];
        // A symbol made of letters must be a word of its own.
        !s.chars().next().is_some_and(char::is_alphabetic) || !is_letter(rest.chars().next_back())
    };
    CODES
        .iter()
        .copied()
        .chain(SYMBOLS.iter().map(|(s, _)| *s))
        .filter(|s| text.ends_with(s) && ok(s))
        .max_by_key(|s| s.len())
        .map(|s| &text[text.len() - s.len()..])
}

/// A currency written just after the number (spaces between allowed).
fn currency_after(text: &str) -> Option<&str> {
    let text = text.trim_start_matches(|c: char| c.is_whitespace());
    let ok = |s: &str| {
        !s.chars().next_back().is_some_and(char::is_alphabetic)
            || !is_letter(text[s.len()..].chars().next())
    };
    CODES
        .iter()
        .copied()
        .chain(SYMBOLS.iter().map(|(s, _)| *s))
        .filter(|s| text.starts_with(s) && ok(s))
        .max_by_key(|s| s.len())
        .map(|s| &text[..s.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Option<(String, Option<String>)> {
        parse_price(text).map(|p| (p.amount, p.currency))
    }

    #[test]
    fn prices_from_around_the_world() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("£1,234.50", "1234.50", Some("£")),
            ("1.234,50 €", "1234.50", Some("€")),
            ("$ 12", "12", Some("$")),
            ("12,99 zł", "12.99", Some("zł")),
            ("From USD 1,099", "1099", Some("USD")),
            ("1,234", "1234", None),
            ("1.234", "1234", None),
            ("12,99", "12.99", None),
            ("1.234,56", "1234.56", None),
            ("1,234.567", "1234.567", None),
            ("1'234.50 CHF", "1234.50", Some("CHF")),
            ("CHF 1’234.50", "1234.50", Some("CHF")),
            ("1 234,50 €", "1234.50", Some("€")),
            ("1\u{a0}234,50\u{a0}€", "1234.50", Some("€")),
            ("1\u{202f}234,50 €", "1234.50", Some("€")),
            ("12 345 678,90 ₽", "12345678.90", Some("₽")),
            ("1,000,000", "1000000", None),
            ("1.000.000", "1000000", None),
            ("€ 9,-", "9", Some("€")),
            ("19.99", "19.99", None),
            ("19.9", "19.9", None),
            ("US$49.00", "49.00", Some("US$")),
            ("R$ 1.299,90", "1299.90", Some("R$")),
            ("C$15", "15", Some("C$")),
            ("¥1,500", "1500", Some("¥")),
            ("1500円", "1500", Some("円")),
            ("₹ 1,23,456.00", "123456.00", Some("₹")),
            ("299 kr", "299", Some("kr")),
            ("299,00 kr.", "299.00", Some("kr")),
            ("1 299 Kč", "1299", Some("Kč")),
            ("4 990 Ft", "4990", Some("Ft")),
            ("EUR 12.50", "12.50", Some("EUR")),
            ("12.50 EUR", "12.50", Some("EUR")),
            ("Price: £10.00 inc. VAT", "10.00", Some("£")),
            ("From £10 to £20", "10", Some("£")),
            ("Now only 9.99!", "9.99", None),
            ("0.500 BHD", "0.500", Some("BHD")),
            ("12345.678", "12345.678", None),
            ("0.00012345 BTC", "0.00012345", None),
            ("$0.99", "0.99", Some("$")),
            ("₩12,000", "12000", Some("₩")),
            ("12,000원", "12000", Some("원")),
            ("Rs. 499", "499", Some("Rs.")),
            ("Rs 499", "499", Some("Rs")),
            ("599,-", "599", None),
            ("USD1,099.00", "1099.00", Some("USD")),
            ("1099USD", "1099", Some("USD")),
            ("100 lei", "100", Some("lei")),
            ("12 Selbstabholung", "12", None),
            ("2 for $5", "2", None),
        ];
        for (text, amount, currency) in cases {
            assert_eq!(
                p(text),
                Some((amount.to_string(), currency.map(str::to_string))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn leading_points_lakhs_signs_dashes_and_wide_digits() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            // A leading decimal point.
            (".99", "0.99", None),
            ("$.99", "0.99", Some("$")),
            ("€ .50", "0.50", Some("€")),
            ("only .5 left", "0.5", None),
            ("Rs.499", "499", Some("Rs.")),
            // Indian lakh and crore grouping.
            ("Rs. 1,23,456", "123456", Some("Rs.")),
            ("₹1,00,000", "100000", Some("₹")),
            ("₹ 1,00,00,000", "10000000", Some("₹")),
            ("₹12,34,567.50", "1234567.50", Some("₹")),
            // A sign before the number, or before a currency before it.
            ("-£12.00", "-12.00", Some("£")),
            ("−5 €", "-5", Some("€")),
            ("-12,50 €", "-12.50", Some("€")),
            ("Discount: -$3", "-3", Some("$")),
            ("(-4.20)", "-4.20", None),
            ("2-for-$5", "2", None),
            ("sold-out-£5", "5", Some("£")),
            // Whole amounts written with a dash for the cents.
            ("1 299,- Kč", "1299", Some("Kč")),
            ("299,- kr", "299", Some("kr")),
            ("49,– €", "49", Some("€")),
            ("49.- CHF", "49", Some("CHF")),
            // Full-width digits and separators.
            ("￥１，２３４", "1234", Some("￥")),
            ("１２．５０ドル", "12.50", None),
        ];
        for (text, amount, currency) in cases {
            assert_eq!(
                p(text),
                Some((amount.to_string(), currency.map(str::to_string))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn where_a_minus_is_a_sign() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            // A colon and a dash label a price; they don't negate it.
            ("Rate:-₹ 1,200/-", "1200", Some("₹")),
            ("MRP:-₹499", "499", Some("₹")),
            ("Price:-Rs.499", "499", Some("Rs.")),
            ("Cost:-500", "500", None),
            ("x-5", "5", None),
            // At the start, after a space or an opening bracket, it is.
            ("-5", "-5", None),
            ("Total -5", "-5", None),
            ("[-3]", "-3", None),
            ("(-£4)", "-4", Some("£")),
            // A currency, then the sign.
            ("£-5", "-5", Some("£")),
            ("US$-5", "-5", Some("US$")),
            ("€-12,50", "-12.50", Some("€")),
            ("€ -5", "-5", Some("€")),
            ("Rs.-499", "-499", Some("Rs.")),
            // Full-width minus and ideographic space.
            ("－５円", "-5", Some("円")),
            ("１\u{3000}２３４円", "1234", Some("円")),
        ];
        for (text, amount, currency) in cases {
            assert_eq!(
                p(text),
                Some((amount.to_string(), currency.map(str::to_string))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn no_number_no_price() {
        for text in [
            "",
            "Free",
            "Sold out",
            "£",
            "Price on request",
            "N/A",
            "€ ,",
        ] {
            assert_eq!(p(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_code_or_symbol_must_stand_alone() {
        // "LUSD" isn't USD, and "Fta" isn't forint.
        assert_eq!(p("LUSD 5"), Some(("5".into(), None)));
        assert_eq!(p("5 Fta"), Some(("5".into(), None)));
    }

    #[test]
    fn codes_for_what_is_written() {
        for (written, code) in [
            ("£", Some("GBP")),
            ("€", Some("EUR")),
            ("$", None),
            ("US$", Some("USD")),
            ("¥", None),
            ("zł", Some("PLN")),
            ("kr", None),
            ("USD", Some("USD")),
            ("usd", Some("USD")),
            ("CHF", Some("CHF")),
            ("R$", Some("BRL")),
            ("₹", Some("INR")),
            ("XYZ", None),
            ("", None),
        ] {
            assert_eq!(currency_code(written), code, "{written:?}");
        }
    }
}
