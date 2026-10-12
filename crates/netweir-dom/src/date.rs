//! Dates as pages write them: ISO 8601, RFC 2822 and HTTP dates, numeric
//! dates, month names in seven languages, and English relative phrases.
//!
//! The rule throughout is that a string is read only when every part of it
//! is understood. A wrong date is worse than none.

/// A date and time on a calendar, without a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub micro: u32,
}

impl Civil {
    pub fn date(year: i32, month: u32, day: u32) -> Civil {
        Civil {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            second: 0,
            micro: 0,
        }
    }
}

/// What the text said about the zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// No zone: a naive date.
    Naive,
    /// A fixed offset, in seconds east of UTC.
    Offset(i32),
    /// Counted from `now` (`3 days ago`), so in whatever zone `now` is.
    AsNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parsed {
    pub civil: Civil,
    pub zone: Zone,
    /// Seconds still to add to `civil` as elapsed time rather than on the
    /// clock face: the hours, minutes and seconds of `3 hours ago`. In a
    /// zone with summer time an hour ago isn't always the clock moved back
    /// one, so the caller adds these in UTC. Days, weeks, months and
    /// years are already in `civil`, on the calendar.
    pub elapsed: i64,
}

impl Parsed {
    /// `civil` with the elapsed seconds added on the clock face, as a
    /// naive datetime would have them. Always Some for what
    /// [`parse_date`] returns.
    pub fn naive(&self) -> Option<Civil> {
        add_seconds(self.civil, self.elapsed)
    }
}

/// Dates are short; anything longer is prose, not a date.
const MAX_LEN: usize = 200;

/// The date in `text`, or None when it isn't fully understood.
///
/// `day_first` settles `03/04/2026`: given, it decides; None reads a date
/// written with dots (`03.04.2026`) day first, as everywhere that writes
/// them does, and one with slashes or dashes month first, as US English;
/// `now` is what relative phrases count from and two-digit years are read
/// near.
pub fn parse_date(text: &str, day_first: Option<bool>, now: Civil) -> Option<Parsed> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_LEN {
        return None;
    }
    let lower = text.to_lowercase();
    let tokens = tokenize(&lower)?;
    relative(&tokens, now).or_else(|| absolute(&tokens, day_first, now))
}

// --- tokens -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok<'a> {
    /// A run of ASCII digits; `ordinal` when a suffix like "th" or "er"
    /// was stuck to it.
    Num {
        digits: &'a str,
        ordinal: bool,
    },
    Word(&'a str),
    Punct(char),
}

/// Tokens with whether whitespace came before each.
#[derive(Debug)]
struct Token<'a> {
    tok: Tok<'a>,
    spaced: bool,
}

const ORDINALS: &[&str] = &[
    "st", "nd", "rd", "th", "er", "re", "e", "ème", "eme", "º", "ª",
];

fn tokenize(text: &str) -> Option<Vec<Token<'_>>> {
    let mut out: Vec<Token<'_>> = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut spaced = false;
    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            spaced = true;
            chars.next();
            continue;
        }
        let run = |chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
                   keep: fn(char) -> bool| {
            let mut end = i;
            while let Some(&(j, c)) = chars.peek() {
                if !keep(c) {
                    break;
                }
                end = j + c.len_utf8();
                chars.next();
            }
            &text[i..end]
        };
        let tok = if c.is_ascii_digit() {
            Tok::Num {
                digits: run(&mut chars, |c| c.is_ascii_digit()),
                ordinal: false,
            }
        } else if c.is_alphabetic() {
            let word = run(&mut chars, char::is_alphabetic);
            // "12th", "1er", "1º": the suffix belongs to the number.
            if let (
                false,
                Some(Token {
                    tok: Tok::Num { ordinal, .. },
                    ..
                }),
            ) = (spaced, out.last_mut())
                && ORDINALS.contains(&word)
                && !*ordinal
            {
                *ordinal = true;
                spaced = false;
                continue;
            }
            Tok::Word(word)
        } else {
            chars.next();
            Tok::Punct(c)
        };
        out.push(Token { tok, spaced });
        spaced = false;
    }
    Some(out)
}

fn num<'a>(t: Option<&Token<'a>>) -> Option<&'a str> {
    match t?.tok {
        Tok::Num { digits, .. } => Some(digits),
        _ => None,
    }
}

fn word<'a>(t: Option<&Token<'a>>) -> Option<&'a str> {
    match t?.tok {
        Tok::Word(w) => Some(w),
        _ => None,
    }
}

fn punct(t: Option<&Token<'_>>) -> Option<char> {
    match t?.tok {
        Tok::Punct(c) => Some(c),
        _ => None,
    }
}

fn value(digits: &str) -> Option<u32> {
    if digits.len() > 9 {
        return None;
    }
    digits.parse().ok()
}

// --- words ------------------------------------------------------------

/// Month names in English, French, German, Spanish, Italian, Portuguese
/// and Dutch, full and abbreviated, lowercase.
const MONTHS: [&[&str]; 12] = [
    &[
        "january", "jan", "janvier", "janv", "januar", "jänner", "jän", "enero", "ene", "gennaio",
        "gen", "janeiro", "januari",
    ],
    &[
        "february",
        "feb",
        "février",
        "fevrier",
        "févr",
        "fevr",
        "fév",
        "fev",
        "februar",
        "febrero",
        "febbraio",
        "fevereiro",
        "februari",
    ],
    &[
        "march", "mar", "mars", "märz", "maerz", "mär", "mrz", "marzo", "março", "marco", "maart",
        "mrt",
    ],
    &["april", "apr", "avril", "avr", "abril", "abr", "aprile"],
    &["may", "mai", "mayo", "maggio", "mag", "maio", "mei"],
    &[
        "june", "jun", "juin", "juni", "junio", "giugno", "giu", "junho",
    ],
    &[
        "july", "jul", "juillet", "juil", "juli", "julio", "luglio", "lug", "julho",
    ],
    &["august", "aug", "août", "aout", "agosto", "ago", "augustus"],
    &[
        "september",
        "sep",
        "sept",
        "septembre",
        "septiembre",
        "setiembre",
        "set",
        "settembre",
        "sett",
        "setembro",
    ],
    &[
        "october", "oct", "octobre", "oktober", "okt", "octubre", "ottobre", "ott", "outubro",
        "out",
    ],
    &["november", "nov", "novembre", "noviembre", "novembro"],
    &[
        "december",
        "dec",
        "décembre",
        "decembre",
        "déc",
        "dezember",
        "dez",
        "diciembre",
        "dic",
        "dicembre",
        "dezembro",
    ],
];

const WEEKDAYS: &[&str] = &[
    // English
    "monday",
    "mon",
    "tuesday",
    "tue",
    "tues",
    "wednesday",
    "wed",
    "thursday",
    "thu",
    "thur",
    "thurs",
    "friday",
    "fri",
    "saturday",
    "sat",
    "sunday",
    "sun",
    // French
    "lundi",
    "lun",
    "mardi",
    "mar",
    "mercredi",
    "mer",
    "jeudi",
    "jeu",
    "vendredi",
    "ven",
    "samedi",
    "sam",
    "dimanche",
    "dim",
    // German
    "montag",
    "mo",
    "dienstag",
    "di",
    "mittwoch",
    "mi",
    "donnerstag",
    "do",
    "freitag",
    "fr",
    "samstag",
    "sonnabend",
    "sa",
    "sonntag",
    "so",
    // Spanish
    "lunes",
    "martes",
    "miércoles",
    "miercoles",
    "mié",
    "mie",
    "jueves",
    "jue",
    "viernes",
    "vie",
    "sábado",
    "sabado",
    "sáb",
    "sab",
    "domingo",
    "dom",
    // Italian
    "lunedì",
    "lunedi",
    "martedì",
    "martedi",
    "mercoledì",
    "mercoledi",
    "giovedì",
    "giovedi",
    "gio",
    "venerdì",
    "venerdi",
    "sabato",
    "domenica",
    // Portuguese ("segunda-feira" is two words around a hyphen)
    "segunda",
    "seg",
    "terça",
    "terca",
    "ter",
    "quarta",
    "qua",
    "quinta",
    "qui",
    "sexta",
    "sex",
    "feira",
    // Dutch
    "maandag",
    "ma",
    "dinsdag",
    "woensdag",
    "wo",
    "donderdag",
    "vrijdag",
    "vr",
    "zaterdag",
    "za",
    "zondag",
    "zo",
];

/// Words that join the parts of a date without changing it: "the 12th
/// of October", "12 de octubre de 2026", "à 9h30", "um 9:30 Uhr".
const FILLERS: &[&str] = &[
    "the", "of", "on", "at", "t", "de", "del", "du", "le", "à", "a", "las", "la", "um", "uhr",
    "om", "alle", "ore", "às", "as", "den", "der", "el",
];

fn month(w: &str) -> Option<u32> {
    MONTHS
        .iter()
        .position(|names| names.contains(&w))
        .map(|i| i as u32 + 1)
}

/// Zone abbreviations, as offsets in minutes east of UTC. Only names that
/// mean one zone are here. CST is US Central, as RFC 2822 has it (China
/// Standard Time is also CST, and loses); IST (India, Ireland or Israel)
/// is left out because it can't be told apart.
const ZONES: &[(&str, i32)] = &[
    ("utc", 0),
    ("ut", 0),
    ("gmt", 0),
    ("z", 0),
    // North America
    ("est", -5 * 60),
    ("edt", -4 * 60),
    ("cst", -6 * 60),
    ("cdt", -5 * 60),
    ("mst", -7 * 60),
    ("mdt", -6 * 60),
    ("pst", -8 * 60),
    ("pdt", -7 * 60),
    ("akst", -9 * 60),
    ("akdt", -8 * 60),
    ("hst", -10 * 60),
    // Europe
    ("wet", 0),
    ("west", 60),
    ("bst", 60),
    ("cet", 60),
    ("cest", 2 * 60),
    ("met", 60),
    ("mest", 2 * 60),
    ("mez", 60),
    ("mesz", 2 * 60),
    ("eet", 2 * 60),
    ("eest", 3 * 60),
    ("msk", 3 * 60),
    // Asia and the Pacific
    ("jst", 9 * 60),
    ("kst", 9 * 60),
    ("hkt", 8 * 60),
    ("sgt", 8 * 60),
    ("pht", 8 * 60),
    ("awst", 8 * 60),
    ("acst", 9 * 60 + 30),
    ("acdt", 10 * 60 + 30),
    ("aest", 10 * 60),
    ("aedt", 11 * 60),
    ("nzst", 12 * 60),
    ("nzdt", 13 * 60),
];

fn zone(w: &str) -> Option<i32> {
    ZONES
        .iter()
        .find(|(name, _)| *name == w)
        .map(|(_, m)| m * 60)
}

// --- absolute dates -----------------------------------------------------

#[derive(Debug)]
enum Part<'a> {
    Num {
        digits: &'a str,
        ordinal: bool,
    },
    /// A month name; `or_weekday` for "mar", which is March or a French,
    /// Spanish or Italian Tuesday.
    Month {
        month: u32,
        or_weekday: bool,
    },
}

fn absolute(tokens: &[Token<'_>], day_first: Option<bool>, now: Civil) -> Option<Parsed> {
    let mut parts: Vec<(usize, Part<'_>)> = Vec::new();
    let mut time: Option<(u32, u32, u32, u32)> = None;
    let mut offset: Option<i32> = None;
    let mut days_from_now: Option<i64> = None;
    let mut i = 0;
    while i < tokens.len() {
        if let Some((t, next)) = time_at(tokens, i) {
            if time.replace(t).is_some() {
                return None;
            }
            i = next;
            if let Some((o, next)) = offset_at(tokens, i) {
                if !set_offset(&mut offset, o) {
                    return None;
                }
                i = next;
            }
            continue;
        }
        match tokens[i].tok {
            Tok::Num { digits, ordinal } => parts.push((i, Part::Num { digits, ordinal })),
            Tok::Word(w) => {
                if let Some(m) = month(w) {
                    let or_weekday = WEEKDAYS.contains(&w);
                    parts.push((
                        i,
                        Part::Month {
                            month: m,
                            or_weekday,
                        },
                    ));
                } else if let Some(o) = zone(w).filter(|_| w != "z") {
                    // "UTC+1", "GMT+01:00"
                    let o = match signed_offset(tokens, i + 1) {
                        Some((extra, next)) if matches!(w, "utc" | "gmt" | "ut") => {
                            i = next - 1;
                            extra
                        }
                        _ => o,
                    };
                    if !set_offset(&mut offset, o) {
                        return None;
                    }
                } else if let Some(d) = match w {
                    "today" => Some(0),
                    "yesterday" => Some(-1),
                    "tomorrow" => Some(1),
                    _ => None,
                } {
                    if days_from_now.replace(d).is_some() {
                        return None;
                    }
                } else if w == "am" {
                    // German "am 12. Oktober": only before the date starts.
                    // A time reads its own "am" first, so "9 am" is a time.
                    if !parts.is_empty() || time.is_some() || offset.is_some() {
                        return None;
                    }
                } else if !WEEKDAYS.contains(&w) && !FILLERS.contains(&w) {
                    return None;
                }
            }
            // JavaScript's Date.toString() ends with the zone's name in
            // brackets, in the browser's language: once an offset is read,
            // a trailing bracket is only a comment. A lone zone name in
            // brackets is still read, so "+0100 (EST)" can disagree.
            Tok::Punct('(') if offset.is_some() && trailing_comment(tokens, i) => break,
            // "12 Oct 2026 +0100": an offset without a time, only as the
            // last thing and after a space, so it's never a date's dash.
            Tok::Punct('+' | '-' | '\u{2212}') if tokens[i].spaced && !parts.is_empty() => {
                match signed_offset(tokens, i) {
                    Some((o, next)) if next == tokens.len() && set_offset(&mut offset, o) => {
                        break;
                    }
                    _ => return None,
                }
            }
            Tok::Punct(c) => {
                if !matches!(c, ',' | '.' | '/' | '-' | '(' | ')') {
                    return None;
                }
            }
        }
        i += 1;
    }
    // "Mar 12 2026" is March; "mar., 12 oct. 2026" is a Tuesday in October.
    if parts
        .iter()
        .filter(|(_, p)| {
            matches!(
                p,
                Part::Month {
                    or_weekday: false,
                    ..
                }
            )
        })
        .count()
        > 0
    {
        parts.retain(|(_, p)| {
            !matches!(
                p,
                Part::Month {
                    or_weekday: true,
                    ..
                }
            )
        });
    }
    let (y, m, d) = if let Some(days) = days_from_now {
        if !parts.is_empty() {
            return None;
        }
        let date = add_seconds(Civil::date(now.year, now.month, now.day), days * 86_400)?;
        (date.year, date.month, date.day)
    } else {
        date_of(tokens, &parts, day_first, now)?
    };
    let (hour, minute, second, micro) = time.unwrap_or_default();
    if !(1..=9999).contains(&y) || m == 0 || m > 12 || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    let zone = match (offset, days_from_now) {
        (Some(o), _) => Zone::Offset(o),
        (None, Some(_)) => Zone::AsNow,
        (None, None) => Zone::Naive,
    };
    Some(Parsed {
        civil: Civil {
            year: y,
            month: m,
            day: d,
            hour,
            minute,
            second,
            micro,
        },
        zone,
        elapsed: 0,
    })
}

/// Whether the bracket at `i` closes at the very end, with no brackets
/// inside, and holds more than one known zone name.
fn trailing_comment(tokens: &[Token<'_>], i: usize) -> bool {
    if tokens.len() < i + 2 || punct(tokens.last()) != Some(')') {
        return false;
    }
    let inside = &tokens[i + 1..tokens.len() - 1];
    !inside
        .iter()
        .any(|t| matches!(t.tok, Tok::Punct('(' | ')')))
        && !matches!(inside, [t] if word(Some(t)).and_then(zone).is_some())
}

/// Sets the zone; false if it was already set to a different one. Saying
/// the same thing twice, as in "-0400 (EDT)", is fine.
fn set_offset(offset: &mut Option<i32>, o: i32) -> bool {
    match *offset {
        Some(existing) if existing != o => false,
        _ => {
            *offset = Some(o);
            true
        }
    }
}

/// Year, month and day from the parts left once the time and words are
/// taken out.
fn date_of(
    tokens: &[Token<'_>],
    parts: &[(usize, Part<'_>)],
    day_first: Option<bool>,
    now: Civil,
) -> Option<(i32, u32, u32)> {
    let nums: Vec<(usize, &str, bool)> = parts
        .iter()
        .filter_map(|(i, p)| match p {
            Part::Num { digits, ordinal } => Some((*i, *digits, *ordinal)),
            Part::Month { .. } => None,
        })
        .collect();
    let months: Vec<(usize, u32)> = parts
        .iter()
        .filter_map(|(i, p)| match p {
            Part::Month { month, .. } => Some((*i, *month)),
            Part::Num { .. } => None,
        })
        .collect();
    let year = |digits: &str| -> Option<i32> {
        match digits.len() {
            4 => Some(value(digits)? as i32),
            2 => Some(two_digit_year(value(digits)? as i32, now.year)),
            _ => None,
        }
    };
    let day = |digits: &str| -> Option<u32> { (digits.len() <= 2).then(|| value(digits))? };
    match (months.as_slice(), nums.as_slice()) {
        // "12 Oct 2026", "October 12th, 2026", "2026 Oct 12", "12-Oct-26"
        ([(mi, m)], [(ai, a, a_ord), (_, b, b_ord)]) => {
            let (d, y) = if a.len() == 4 && !a_ord {
                (b, a)
            } else if b.len() == 4 && !b_ord {
                (a, b)
            } else if mi < ai || (mi > ai && !b_ord) {
                // Month first ("Oct 12, 26") or in the middle ("12 Oct 26"):
                // the year is last.
                if *b_ord {
                    return None;
                }
                (a, b)
            } else {
                return None;
            };
            Some((year(y)?, *m, day(d)?))
        }
        // "12/10/2026", "2026-10-12", "12.10.26"
        ([], [(ai, a, false), (bi, b, false), (ci, c, false)]) => {
            let sep = punct(tokens.get(ai + 1))?;
            if !matches!(sep, '/' | '-' | '.')
                || *bi != ai + 2
                || *ci != bi + 2
                || punct(tokens.get(bi + 1)) != Some(sep)
                // "12. 10. 2026" may have spaces after its dots.
                || (sep != '.' && (tokens[*bi].spaced || tokens[*ci].spaced))
                || tokens[ai + 1].spaced
                || tokens[bi + 1].spaced
            {
                return None;
            }
            if a.len() == 4 {
                return Some((year(a)?, day(b)?, day(c)?));
            }
            let (a, b) = (day(a)?, day(b)?);
            let day_first = day_first.unwrap_or(sep == '.');
            // When only one reading is a real date, that's the one.
            let (d, m) = if a > 12 {
                (a, b)
            } else if b > 12 || !day_first {
                (b, a)
            } else {
                (a, b)
            };
            Some((year(c)?, m, d))
        }
        _ => None,
    }
}

/// 19xx or 20xx, whichever is nearer `now`'s year (the earlier on a tie).
fn two_digit_year(yy: i32, now_year: i32) -> i32 {
    let (a, b) = (1900 + yy, 2000 + yy);
    if (b - now_year).abs() < (a - now_year).abs() {
        b
    } else {
        a
    }
}

/// A time starting at token `i`: `09:30`, `9:30:15.25`, `9.30pm`,
/// `9 PM`, `9h30`. Gives (hour, minute, second, microsecond) and the index
/// after it.
fn time_at(tokens: &[Token<'_>], i: usize) -> Option<((u32, u32, u32, u32), usize)> {
    let h = num(tokens.get(i)).filter(|d| d.len() <= 2)?;
    let mut h = value(h)?;
    let two = |j: usize| {
        num(tokens.get(j))
            .filter(|d| d.len() == 2 && !tokens[j].spaced)
            .and_then(value)
    };
    let (mut minute, mut second, mut micro, mut j) = (0, 0, 0, i + 1);
    let mut needs_meridiem = true;
    match (punct(tokens.get(j)), word(tokens.get(j))) {
        (Some(':'), _) if !tokens[j].spaced => {
            minute = two(j + 1)?;
            j += 2;
            needs_meridiem = false;
            if punct(tokens.get(j)) == Some(':') && !tokens[j].spaced {
                second = two(j + 1)?;
                j += 2;
                let frac = num(tokens.get(j + 1)).filter(|_| !tokens[j + 1].spaced);
                if let (Some('.' | ','), false, Some(frac)) = (
                    punct(tokens.get(j)),
                    tokens.get(j).is_none_or(|t| t.spaced),
                    frac,
                ) {
                    let six: String = frac.chars().chain(std::iter::repeat('0')).take(6).collect();
                    micro = six.parse().ok()?;
                    j += 2;
                }
            }
        }
        // French: "9h30", "9 h", "21h"
        // German: "um 9 Uhr" (the "Uhr" is left for the filler words)
        (_, Some("uhr")) => needs_meridiem = false,
        (_, Some("h")) => {
            j += 1;
            if let Some(m) = two(j) {
                minute = m;
                j += 1;
            }
            needs_meridiem = false;
        }
        // "9.30pm" (only with am or pm, or it's a date)
        (Some('.'), _) if !tokens[j].spaced && two(j + 1).is_some() => {
            minute = two(j + 1)?;
            j += 2;
        }
        _ => {}
    }
    match meridiem(tokens, j) {
        Some((pm, next)) => {
            if !(1..=12).contains(&h) {
                return None;
            }
            h = h % 12 + if pm { 12 } else { 0 };
            j = next;
        }
        None if needs_meridiem => return None,
        None => {}
    }
    if h > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(((h, minute, second, micro), j))
}

/// `am`, `pm`, `a.m.`, `p.m.`: (whether it's pm, index after it).
fn meridiem(tokens: &[Token<'_>], j: usize) -> Option<(bool, usize)> {
    match word(tokens.get(j))? {
        "am" => Some((false, j + 1)),
        "pm" => Some((true, j + 1)),
        w @ ("a" | "p") => {
            if punct(tokens.get(j + 1)) != Some('.') || word(tokens.get(j + 2)) != Some("m") {
                return None;
            }
            let end = if punct(tokens.get(j + 3)) == Some('.') {
                j + 4
            } else {
                j + 3
            };
            Some((w == "p", end))
        }
        _ => None,
    }
}

/// The zone right after a time: `Z`, `+01:00`, `-0500`, `+01`, or a name.
fn offset_at(tokens: &[Token<'_>], i: usize) -> Option<(i32, usize)> {
    if word(tokens.get(i)) == Some("z") {
        return Some((0, i + 1));
    }
    signed_offset(tokens, i)
}

/// `+01:00`, `-0500`, `+1`: seconds east of UTC and the index after it.
fn signed_offset(tokens: &[Token<'_>], i: usize) -> Option<(i32, usize)> {
    let sign = match punct(tokens.get(i))? {
        '+' => 1,
        '-' | '\u{2212}' => -1,
        _ => return None,
    };
    let digits = num(tokens.get(i + 1)).filter(|_| !tokens[i + 1].spaced)?;
    let (h, m, next) = match digits.len() {
        1 | 2 => {
            let h = value(digits)?;
            if punct(tokens.get(i + 2)) == Some(':') {
                let m = num(tokens.get(i + 3)).filter(|d| d.len() == 2)?;
                (h, value(m)?, i + 4)
            } else {
                (h, 0, i + 2)
            }
        }
        4 => (value(&digits[..2])?, value(&digits[2..])?, i + 2),
        _ => return None,
    };
    if h > 23 || m > 59 {
        return None;
    }
    Some((sign * (h * 3600 + m * 60) as i32, next))
}

// --- relative dates -----------------------------------------------------

/// `now`, `3 days ago`, `in 2 weeks`, `2 hours, 5 minutes ago`.
fn relative(tokens: &[Token<'_>], now: Civil) -> Option<Parsed> {
    let words: Vec<&str> = tokens.iter().filter_map(|t| word(Some(t))).collect();
    if tokens.len() == words.len()
        && matches!(words.as_slice(), ["now"] | ["just" | "right", "now"])
    {
        return Some(Parsed {
            civil: now,
            zone: Zone::AsNow,
            elapsed: 0,
        });
    }
    let (ahead, body) = match (word(tokens.first()), word(tokens.last())) {
        (Some("in"), last) if last != Some("ago") => (true, &tokens[1..]),
        (first, Some("ago")) if first != Some("in") => (false, &tokens[..tokens.len() - 1]),
        _ => return None,
    };
    let (mut months, mut days, mut seconds) = (0i64, 0i64, 0i64);
    let mut i = 0;
    let mut any = false;
    while i < body.len() {
        if any {
            // Parts are joined by commas or "and".
            let mut joined = false;
            if punct(body.get(i)) == Some(',') {
                i += 1;
                joined = true;
            }
            if word(body.get(i)) == Some("and") {
                i += 1;
                joined = true;
            }
            if !joined {
                return None;
            }
        }
        let n: i64 = match (num(body.get(i)), word(body.get(i))) {
            (Some(d), _) => i64::from(value(d)?),
            (_, Some(w)) => [
                "a", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
            ]
            .iter()
            .position(|n| *n == w)
            .map(|p| p.max(1) as i64)
            .or((w == "an").then_some(1))?,
            _ => return None,
        };
        let unit = word(body.get(i + 1))?;
        let (m, d, s) = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => (0, 0, 1),
            "m" | "min" | "mins" | "minute" | "minutes" => (0, 0, 60),
            "h" | "hr" | "hrs" | "hour" | "hours" => (0, 0, 3_600),
            "d" | "day" | "days" => (0, 1, 0),
            "w" | "wk" | "wks" | "week" | "weeks" => (0, 7, 0),
            "mo" | "month" | "months" => (1, 0, 0),
            "y" | "yr" | "yrs" | "year" | "years" => (12, 0, 0),
            _ => return None,
        };
        months = months.checked_add(n.checked_mul(m)?)?;
        days = days.checked_add(n.checked_mul(d)?)?;
        seconds = seconds.checked_add(n.checked_mul(s)?)?;
        any = true;
        i += 2;
    }
    if !any {
        return None;
    }
    let sign = if ahead { 1 } else { -1 };
    let civil = add_seconds(add_months(now, sign * months)?, sign * days * 86_400)?;
    // The elapsed part must land in range too.
    add_seconds(civil, sign * seconds)?;
    Some(Parsed {
        civil,
        zone: Zone::AsNow,
        elapsed: sign * seconds,
    })
}

// --- calendar arithmetic -------------------------------------------------

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Months later (or earlier), the day kept within the month: a month after
/// 31 January is 28 or 29 February.
fn add_months(c: Civil, months: i64) -> Option<Civil> {
    let total = i64::from(c.year) * 12 + i64::from(c.month) - 1 + months;
    let year = i32::try_from(total.div_euclid(12)).ok()?;
    let month = total.rem_euclid(12) as u32 + 1;
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some(Civil {
        year,
        month,
        day: c.day.min(days_in_month(year, month)),
        ..c
    })
}

/// Days from 1970-01-01 to a date (Howard Hinnant's algorithm).
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn add_seconds(c: Civil, seconds: i64) -> Option<Civil> {
    let t = days_from_civil(c.year, c.month, c.day)
        .checked_mul(86_400)?
        .checked_add(i64::from(c.hour * 3600 + c.minute * 60 + c.second))?
        .checked_add(seconds)?;
    let (y, month, day) = civil_from_days(t.div_euclid(86_400));
    let secs = t.rem_euclid(86_400) as u32;
    let year = i32::try_from(y).ok().filter(|y| (1..=9999).contains(y))?;
    Some(Civil {
        year,
        month,
        day,
        hour: secs / 3600,
        minute: secs / 60 % 60,
        second: secs % 60,
        micro: c.micro,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Civil = Civil {
        year: 2026,
        month: 10,
        day: 12,
        hour: 15,
        minute: 45,
        second: 30,
        micro: 0,
    };

    type Expect = (i32, u32, u32, u32, u32, u32, u32, Zone);

    fn read(text: &str, day_first: bool) -> Option<Expect> {
        parse_date(text, Some(day_first), NOW).map(|p| {
            let c = p.naive().unwrap();
            (
                c.year, c.month, c.day, c.hour, c.minute, c.second, c.micro, p.zone,
            )
        })
    }

    const N: Zone = Zone::Naive;
    const UTC: Zone = Zone::Offset(0);
    const H: i32 = 3600;

    fn check(cases: &[(&str, Expect)], day_first: bool) {
        for (text, want) in cases {
            assert_eq!(read(text, day_first), Some(*want), "{text:?}");
        }
    }

    #[test]
    fn without_day_first_dots_are_day_first_and_slashes_month_first() {
        let read = |text| parse_date(text, None, NOW).map(|p| (p.civil.month, p.civil.day));
        assert_eq!(read("03.04.2026"), Some((4, 3)));
        assert_eq!(read("03/04/2026"), Some((3, 4)));
        assert_eq!(read("03-04-2026"), Some((3, 4)));
        // One reading only: taken whatever the separator.
        assert_eq!(read("04.13.2026"), Some((4, 13)));
    }

    #[test]
    fn iso_8601_and_rfc_3339() {
        check(
            &[
                ("2026-10-12", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("2026-10-12T09:30:00Z", (2026, 10, 12, 9, 30, 0, 0, UTC)),
                ("2026-10-12t09:30:00z", (2026, 10, 12, 9, 30, 0, 0, UTC)),
                ("2026-10-12T09:30", (2026, 10, 12, 9, 30, 0, 0, N)),
                ("2026-10-12 09:30:15", (2026, 10, 12, 9, 30, 15, 0, N)),
                (
                    "2026-10-12T09:30:00.123456+01:00",
                    (2026, 10, 12, 9, 30, 0, 123_456, Zone::Offset(H)),
                ),
                (
                    "2026-10-12T09:30:00.5-05:00",
                    (2026, 10, 12, 9, 30, 0, 500_000, Zone::Offset(-5 * H)),
                ),
                (
                    "2026-10-12T09:30:00,25+0530",
                    (2026, 10, 12, 9, 30, 0, 250_000, Zone::Offset(5 * H + 1800)),
                ),
                (
                    "2026-10-12T09:30:00.123456789Z",
                    (2026, 10, 12, 9, 30, 0, 123_456, UTC),
                ),
                (
                    "2026-10-12T09:30:00+01",
                    (2026, 10, 12, 9, 30, 0, 0, Zone::Offset(H)),
                ),
                ("2026-10-12 09:30:00 UTC", (2026, 10, 12, 9, 30, 0, 0, UTC)),
                ("2026/10/12", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("2024-02-29", (2024, 2, 29, 0, 0, 0, 0, N)),
            ],
            false,
        );
    }

    #[test]
    fn rfc_2822_and_http_dates() {
        check(
            &[
                (
                    "Mon, 12 Oct 2026 09:30:00 GMT",
                    (2026, 10, 12, 9, 30, 0, 0, UTC),
                ),
                (
                    "Mon, 12 Oct 2026 09:30:00 +0200",
                    (2026, 10, 12, 9, 30, 0, 0, Zone::Offset(2 * H)),
                ),
                (
                    "Mon, 12 Oct 2026 09:30:00 -0400 (EDT)",
                    (2026, 10, 12, 9, 30, 0, 0, Zone::Offset(-4 * H)),
                ),
                (
                    "12 Oct 2026 09:30 EST",
                    (2026, 10, 12, 9, 30, 0, 0, Zone::Offset(-5 * H)),
                ),
                // RFC 850 and asctime, which HTTP still allows.
                (
                    "Monday, 12-Oct-26 09:30:00 GMT",
                    (2026, 10, 12, 9, 30, 0, 0, UTC),
                ),
                ("Mon Oct 12 09:30:00 2026", (2026, 10, 12, 9, 30, 0, 0, N)),
                ("Mon Oct  2 09:30:00 2026", (2026, 10, 2, 9, 30, 0, 0, N)),
            ],
            false,
        );
    }

    #[test]
    fn numeric_dates() {
        check(
            &[
                ("10/12/2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("10-12-2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("10.12.2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("13/10/2026", (2026, 10, 13, 0, 0, 0, 0, N)),
                ("10/13/2026", (2026, 10, 13, 0, 0, 0, 0, N)),
                ("1/2/2026", (2026, 1, 2, 0, 0, 0, 0, N)),
                ("10/12/26", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("10/12/99", (1999, 10, 12, 0, 0, 0, 0, N)),
                ("10/12/76", (1976, 10, 12, 0, 0, 0, 0, N)),
                ("10/12/75", (2075, 10, 12, 0, 0, 0, 0, N)),
                ("10/12/2026 9:30 PM", (2026, 10, 12, 21, 30, 0, 0, N)),
                ("10/12/2026, 21:30", (2026, 10, 12, 21, 30, 0, 0, N)),
            ],
            false,
        );
        check(
            &[
                ("12/10/2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("12.10.2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("12-10-26", (2026, 10, 12, 0, 0, 0, 0, N)),
                // Over 12 can only be a day, either way round.
                ("13/10/2026", (2026, 10, 13, 0, 0, 0, 0, N)),
                ("10/13/2026", (2026, 10, 13, 0, 0, 0, 0, N)),
                ("2026-10-12", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("12.10.2026 09:30", (2026, 10, 12, 9, 30, 0, 0, N)),
            ],
            true,
        );
    }

    #[test]
    fn month_names_in_english() {
        check(
            &[
                ("12 Oct 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("12 October 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("October 12th, 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("Oct. 12, 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("Sept 1, 2026", (2026, 9, 1, 0, 0, 0, 0, N)),
                ("1st September 2026", (2026, 9, 1, 0, 0, 0, 0, N)),
                ("the 2nd of March, 2026", (2026, 3, 2, 0, 0, 0, 0, N)),
                ("Monday, October 12, 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("Thu 3rd Dec 2026", (2026, 12, 3, 0, 0, 0, 0, N)),
                ("12-Oct-2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("2026 Oct 12", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("Oct 12, 26", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("OCTOBER 12, 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                (
                    "October 12th, 2026 at 9:30 PM EST",
                    (2026, 10, 12, 21, 30, 0, 0, Zone::Offset(-5 * H)),
                ),
                ("12 Oct 2026, 9.30pm", (2026, 10, 12, 21, 30, 0, 0, N)),
                ("12 Oct 2026 9 a.m.", (2026, 10, 12, 9, 0, 0, 0, N)),
                ("12 Oct 2026 12 a.m.", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("12 Oct 2026 12:15 pm", (2026, 10, 12, 12, 15, 0, 0, N)),
                ("12 Oct 2026 9:30:15 PM", (2026, 10, 12, 21, 30, 15, 0, N)),
                ("12 Oct 2026 UTC", (2026, 10, 12, 0, 0, 0, 0, UTC)),
                (
                    "12 Oct 2026 09:30 UTC+1",
                    (2026, 10, 12, 9, 30, 0, 0, Zone::Offset(H)),
                ),
                (
                    "12 Oct 2026 09:30 GMT-05:00",
                    (2026, 10, 12, 9, 30, 0, 0, Zone::Offset(-5 * H)),
                ),
                (
                    "12 Oct 2026 09:30 CEST",
                    (2026, 10, 12, 9, 30, 0, 0, Zone::Offset(2 * H)),
                ),
                (
                    "12 Oct 2026 09:30 PDT",
                    (2026, 10, 12, 9, 30, 0, 0, Zone::Offset(-7 * H)),
                ),
                ("Mar 3, 2026", (2026, 3, 3, 0, 0, 0, 0, N)),
                ("12 Oct 2026 09:30:00.", (2026, 10, 12, 9, 30, 0, 0, N)),
            ],
            false,
        );
    }

    #[test]
    fn month_names_in_other_languages() {
        check(
            &[
                // French
                ("12 octobre 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                (
                    "lundi 12 octobre 2026 à 9h30",
                    (2026, 10, 12, 9, 30, 0, 0, N),
                ),
                ("le 1er février 2026", (2026, 2, 1, 0, 0, 0, 0, N)),
                ("3 févr. 2026", (2026, 2, 3, 0, 0, 0, 0, N)),
                ("15 août 2026 21h", (2026, 8, 15, 21, 0, 0, 0, N)),
                ("1 déc. 2026", (2026, 12, 1, 0, 0, 0, 0, N)),
                ("mar. 12 oct. 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                // German
                ("12. Oktober 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                (
                    "Montag, 12. Oktober 2026, 09:30 Uhr",
                    (2026, 10, 12, 9, 30, 0, 0, N),
                ),
                ("3. März 2026", (2026, 3, 3, 0, 0, 0, 0, N)),
                ("1. Dez. 2026", (2026, 12, 1, 0, 0, 0, 0, N)),
                ("12. Okt. 2026 um 9:30 Uhr", (2026, 10, 12, 9, 30, 0, 0, N)),
                // Spanish
                ("12 de octubre de 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                (
                    "lunes, 12 de octubre de 2026",
                    (2026, 10, 12, 0, 0, 0, 0, N),
                ),
                ("1º de enero de 2026", (2026, 1, 1, 0, 0, 0, 0, N)),
                ("12 dic 2026", (2026, 12, 12, 0, 0, 0, 0, N)),
                ("mar., 12 oct. 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                // Italian
                ("12 ottobre 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                (
                    "lunedì 12 ottobre 2026 alle ore 9:30",
                    (2026, 10, 12, 9, 30, 0, 0, N),
                ),
                ("1 gen 2026", (2026, 1, 1, 0, 0, 0, 0, N)),
                // Portuguese
                ("12 de outubro de 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                (
                    "segunda-feira, 12 de outubro de 2026 às 09:30",
                    (2026, 10, 12, 9, 30, 0, 0, N),
                ),
                ("5 de março de 2026", (2026, 3, 5, 0, 0, 0, 0, N)),
                // Dutch
                ("12 oktober 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                (
                    "maandag 12 oktober 2026 om 09:30",
                    (2026, 10, 12, 9, 30, 0, 0, N),
                ),
                ("1 mrt 2026", (2026, 3, 1, 0, 0, 0, 0, N)),
                ("5 mei 2026", (2026, 5, 5, 0, 0, 0, 0, N)),
            ],
            false,
        );
    }

    #[test]
    fn hours_minutes_and_seconds_are_elapsed_time_days_are_on_the_calendar() {
        let split = |text| {
            parse_date(text, None, NOW)
                .map(|p| ((p.civil.day, p.civil.hour, p.civil.minute), p.elapsed))
        };
        // Python adds elapsed time in UTC, so "an hour ago" is an hour
        // even across a change to or from summer time.
        assert_eq!(split("2 hours, 5 minutes ago"), Some(((12, 15, 45), -7500)));
        assert_eq!(split("in 30 seconds"), Some(((12, 15, 45), 30)));
        assert_eq!(split("1 day ago"), Some(((11, 15, 45), 0)));
        assert_eq!(split("1 day and 3 hours ago"), Some(((11, 15, 45), -10800)));
        assert_eq!(split("12 Oct 2026"), Some(((12, 0, 0), 0)));
        let last_moment = Civil {
            year: 9999,
            month: 12,
            day: 31,
            hour: 23,
            ..NOW
        };
        assert_eq!(parse_date("in 2 hours", None, last_moment), None);
    }

    #[test]
    fn german_am_and_a_few_loose_forms() {
        check(
            &[
                ("am 12. Oktober 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("Am Montag, 12. Oktober 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("12. Oktober 2026 um 9 Uhr", (2026, 10, 12, 9, 0, 0, 0, N)),
                ("12. Oktober 2026, 21 Uhr", (2026, 10, 12, 21, 0, 0, 0, N)),
                ("12. 10. 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                ("12. 10. 2026 09:30", (2026, 10, 12, 9, 30, 0, 0, N)),
                (
                    "12 Oct 2026 +0100",
                    (2026, 10, 12, 0, 0, 0, 0, Zone::Offset(H)),
                ),
                (
                    "12 Oct 2026 -05:00",
                    (2026, 10, 12, 0, 0, 0, 0, Zone::Offset(-5 * H)),
                ),
            ],
            true,
        );
        for text in [
            "12 Oct 2026 am",
            "12 Oct 2026 09:30 am am",
            "12. Oktober am 2026",
            "12 Oct 2026 9",
            "12 Oct 2026 25 Uhr",
            "12 .10. 2026",
            "12. 10.2026 x",
            "12 Oct 2026 +0100 x",
            "12 Oct 2026 + 0100",
        ] {
            assert_eq!(read(text, true), None, "{text:?}");
        }
    }

    #[test]
    fn more_zones_and_javascript_date_strings() {
        let half = |h: i32, m: i32| Zone::Offset(h * H + m * 60);
        check(
            &[
                (
                    "12 Oct 2026 09:30 JST",
                    (2026, 10, 12, 9, 30, 0, 0, half(9, 0)),
                ),
                (
                    "12 Oct 2026 09:30 KST",
                    (2026, 10, 12, 9, 30, 0, 0, half(9, 0)),
                ),
                (
                    "12 Oct 2026 09:30 HKT",
                    (2026, 10, 12, 9, 30, 0, 0, half(8, 0)),
                ),
                (
                    "12 Oct 2026 09:30 SGT",
                    (2026, 10, 12, 9, 30, 0, 0, half(8, 0)),
                ),
                (
                    "12 Oct 2026 09:30 PHT",
                    (2026, 10, 12, 9, 30, 0, 0, half(8, 0)),
                ),
                (
                    "12 Oct 2026 09:30 AWST",
                    (2026, 10, 12, 9, 30, 0, 0, half(8, 0)),
                ),
                (
                    "12 Oct 2026 09:30 ACST",
                    (2026, 10, 12, 9, 30, 0, 0, half(9, 30)),
                ),
                (
                    "12 Oct 2026 09:30 ACDT",
                    (2026, 10, 12, 9, 30, 0, 0, half(10, 30)),
                ),
                (
                    "12 Oct 2026 09:30 AEST",
                    (2026, 10, 12, 9, 30, 0, 0, half(10, 0)),
                ),
                (
                    "12 Oct 2026 09:30 AEDT",
                    (2026, 10, 12, 9, 30, 0, 0, half(11, 0)),
                ),
                (
                    "12 Oct 2026 09:30 NZST",
                    (2026, 10, 12, 9, 30, 0, 0, half(12, 0)),
                ),
                (
                    "12 Oct 2026 09:30 NZDT",
                    (2026, 10, 12, 9, 30, 0, 0, half(13, 0)),
                ),
                // What JavaScript's Date.toString() writes.
                (
                    "Mon Oct 12 2026 09:30:00 GMT+0100 (Central European Standard Time)",
                    (2026, 10, 12, 9, 30, 0, 0, half(1, 0)),
                ),
                (
                    "Mon Oct 12 2026 09:30:00 GMT-0400 (Eastern Daylight Time)",
                    (2026, 10, 12, 9, 30, 0, 0, half(-4, 0)),
                ),
                (
                    "Mon Oct 12 2026 18:30:00 GMT+0900 (日本標準時)",
                    (2026, 10, 12, 18, 30, 0, 0, half(9, 0)),
                ),
                // German "am" is "on", and doesn't clash with 9 am.
                ("am 12. Oktober 2026", (2026, 10, 12, 0, 0, 0, 0, N)),
                (
                    "am 12. Oktober 2026 um 9:30 Uhr",
                    (2026, 10, 12, 9, 30, 0, 0, N),
                ),
                ("12 Oct 2026 9 am", (2026, 10, 12, 9, 0, 0, 0, N)),
                ("12 Oct 2026 12 am", (2026, 10, 12, 0, 0, 0, 0, N)),
            ],
            false,
        );
        for text in [
            "12 Oct 2026 09:30 IST",
            // A name in brackets is only skipped once an offset is read,
            // and only at the end.
            "12 Oct 2026 09:30 (Central European Standard Time)",
            "12 Oct 2026 09:30 GMT+0100 (Central European Time) extra",
            "12 Oct 2026 09:30 +0100 (EST)",
        ] {
            assert_eq!(read(text, false), None, "{text:?}");
        }
        // Brackets on their own are punctuation, as before.
        for text in ["12 Oct 2026 09:30 +0100 (", "12 Oct 2026 09:30 +0100 ()"] {
            assert!(read(text, false).is_some(), "{text:?}");
        }
    }

    #[test]
    fn relative_phrases() {
        let at = |y, mo, d, h, mi, s| (y, mo, d, h, mi, s, 0, Zone::AsNow);
        check(
            &[
                ("now", at(2026, 10, 12, 15, 45, 30)),
                ("just now", at(2026, 10, 12, 15, 45, 30)),
                ("today", at(2026, 10, 12, 0, 0, 0)),
                ("Yesterday", at(2026, 10, 11, 0, 0, 0)),
                ("tomorrow", at(2026, 10, 13, 0, 0, 0)),
                ("yesterday at 9:30 pm", at(2026, 10, 11, 21, 30, 0)),
                ("Today, 08:15", at(2026, 10, 12, 8, 15, 0)),
                ("3 days ago", at(2026, 10, 9, 15, 45, 30)),
                ("an hour ago", at(2026, 10, 12, 14, 45, 30)),
                ("a minute ago", at(2026, 10, 12, 15, 44, 30)),
                ("1 day ago", at(2026, 10, 11, 15, 45, 30)),
                ("in 2 weeks", at(2026, 10, 26, 15, 45, 30)),
                ("2 hours, 5 minutes ago", at(2026, 10, 12, 13, 40, 30)),
                ("1 day and 3 hours ago", at(2026, 10, 11, 12, 45, 30)),
                ("5m ago", at(2026, 10, 12, 15, 40, 30)),
                ("3h ago", at(2026, 10, 12, 12, 45, 30)),
                ("2 months ago", at(2026, 8, 12, 15, 45, 30)),
                ("a year ago", at(2025, 10, 12, 15, 45, 30)),
                ("in 3 years", at(2029, 10, 12, 15, 45, 30)),
                ("13 days ago", at(2026, 9, 29, 15, 45, 30)),
                ("two weeks ago", at(2026, 9, 28, 15, 45, 30)),
                ("30 seconds ago", at(2026, 10, 12, 15, 45, 0)),
            ],
            false,
        );
        // A month before 31 March is the end of February.
        let end_of_march = Civil {
            day: 31,
            month: 3,
            ..NOW
        };
        let p = parse_date("1 month ago", Some(false), end_of_march).unwrap();
        assert_eq!((p.civil.month, p.civil.day), (2, 28));
    }

    #[test]
    fn what_is_not_a_date() {
        for text in [
            "",
            "   ",
            "Free",
            "soon",
            "Oct 2026",
            "12 Oct",
            "12/10",
            "2026",
            "12345",
            "2026-13-01",
            "2026-02-30",
            "2025-02-29",
            "31/04/2026",
            "13/13/2026",
            "12 Oct 2026 25:00",
            "12 Oct 2026 09:60",
            "12 Oct 2026 13 pm",
            "12 Oct 2026 09:30 XYZ",
            "Posted 12 Oct 2026",
            "12 Oct 2026 or so",
            "12/10-2026",
            "12 Oct Nov 2026",
            "12 13 Oct 2026",
            "10/12/2026 10/12/2026",
            "09:30",
            "3 days",
            "days ago",
            "in a bit",
            "3 parsecs ago",
            "in 3 days ago",
            "yesterday 12 Oct 2026",
            "12 Oct 2026 UTC EST",
            "12 Oct 2026 #1",
            "in 9999 years",
            "123 Oct 2026",
            "12 Oct 202",
            "10/12/202",
            "0000-01-01",
        ] {
            assert_eq!(read(text, false), None, "{text:?}");
        }
        assert_eq!(read(&"12 Oct 2026 ".repeat(100), false), None);
    }
}
