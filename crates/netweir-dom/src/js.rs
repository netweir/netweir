//! Data a page keeps in its scripts: object and array literals written as
//! JavaScript, which JSON parsers turn away (unquoted keys, single quotes,
//! trailing commas, comments, `undefined`).
//!
//! Script text is hostile input, so the reader is built not to hang or
//! overflow: nesting is capped at [`MAX_DEPTH`], a literal that failed is
//! never read again from the same place, and the search as a whole stops
//! after a fixed multiple of the text's length.

use crate::{Node, NodeKind};

/// A JavaScript value, as Python will see it.
#[derive(Debug, Clone, PartialEq)]
pub enum JsValue {
    Null,
    Bool(bool),
    Int(i64),
    /// An integer too big for 64 bits, as its decimal digits.
    BigInt(String),
    Float(f64),
    Str(String),
    Array(Vec<JsValue>),
    /// Keys in the order written; a repeated key is repeated here, and the
    /// later one wins when it becomes a dict.
    Object(Vec<(String, JsValue)>),
}

/// How deep objects and arrays may nest. Real data rarely passes twenty.
pub const MAX_DEPTH: usize = 400;

/// The first non-empty object or array literal in `text`.
pub fn first(text: &str) -> Option<JsValue> {
    Reader::new(text).literals(true).pop()
}

/// Every non-empty object or array literal in `text`, in order. One inside
/// another is part of the outer one, not listed again.
pub fn all(text: &str) -> Vec<JsValue> {
    Reader::new(text).literals(false)
}

/// The value assigned to `name` in `text`: `window.name = {...}`,
/// `var name = [...]`, `name: {...}`, `"name": {...}` or
/// `window["name"] = ...`.
pub fn assigned(text: &str, name: &str) -> Option<JsValue> {
    Reader::new(text).assigned(name)
}

/// Marks a parameter of [`nuxt`]'s function until its argument is known.
const PARAM: &str = "\u{0}param\u{0}";

/// Values at most that [`nuxt`] builds from its function's arguments,
/// against a page that uses one big argument many times over.
const MAX_BUILT: usize = 1_000_000;

/// The state Nuxt assigns to `window.__NUXT__`: an object, or in Nuxt 2
/// the object a function returns, written with its parameters in place
/// of repeated values and called at once:
/// `(function(a,b){return {name:a,price:b}}("Lamp",49.99))`.
pub fn nuxt(text: &str) -> Option<JsValue> {
    let mut r = Reader::new(text);
    let at = memchr::memmem::find(r.s, b"__NUXT__")?;
    r.pos = at + "__NUXT__".len();
    r.skip_space();
    if !r.eat(b'=') {
        return None;
    }
    r.skip_space();
    if matches!(r.peek(), Some(b'{' | b'[')) {
        return r.value(0).ok();
    }
    let wrapped = r.eat(b'(');
    r.skip_space();
    if !r.eat_word("function") {
        return None;
    }
    r.skip_space();
    if !r.eat(b'(') {
        return None;
    }
    let mut params = Vec::new();
    loop {
        r.skip_space();
        if r.eat(b')') {
            break;
        }
        let start = r.pos;
        while r.peek().is_some_and(is_ident_byte) {
            r.pos += 1;
        }
        if r.pos == start {
            return None;
        }
        params.push(text[start..r.pos].to_string());
        r.skip_space();
        if !r.eat(b',') {
            r.skip_space();
            if !r.eat(b')') {
                return None;
            }
            break;
        }
    }
    r.skip_space();
    if !r.eat(b'{') {
        return None;
    }
    r.skip_space();
    if !r.eat_word("return") {
        return None;
    }
    r.params = params;
    let body = r.value(0).ok()?;
    r.params = Vec::new();
    r.skip_space();
    r.eat(b';');
    r.skip_space();
    if !r.eat(b'}') {
        return None;
    }
    r.skip_space();
    if wrapped {
        // `(function(){...}(args))` or `(function(){...})(args)`.
        r.eat(b')');
        r.skip_space();
    }
    let args = r.arguments(0).ok()?;
    let mut built = 0;
    fill(body, &args, &mut built)
}

/// `value` with each parameter placeholder replaced by its argument.
fn fill(value: JsValue, args: &[JsValue], built: &mut usize) -> Option<JsValue> {
    fn size(v: &JsValue) -> usize {
        match v {
            JsValue::Array(items) => 1 + items.iter().map(size).sum::<usize>(),
            JsValue::Object(members) => 1 + members.iter().map(|(_, v)| size(v)).sum::<usize>(),
            _ => 1,
        }
    }
    *built += 1;
    if *built > MAX_BUILT {
        return None;
    }
    Some(match value {
        JsValue::Str(s) if s.starts_with(PARAM) => {
            let arg = s[PARAM.len()..]
                .parse::<usize>()
                .ok()
                .and_then(|i| args.get(i))
                .cloned()
                .unwrap_or(JsValue::Null);
            *built += size(&arg);
            if *built > MAX_BUILT {
                return None;
            }
            arg
        }
        JsValue::Array(items) => JsValue::Array(
            items
                .into_iter()
                .map(|v| fill(v, args, built))
                .collect::<Option<_>>()?,
        ),
        JsValue::Object(members) => JsValue::Object(
            members
                .into_iter()
                .map(|(k, v)| Some((k, fill(v, args, built)?)))
                .collect::<Option<_>>()?,
        ),
        other => other,
    })
}

/// The data in the `<script>` elements at and below `root`. With `name`,
/// the value assigned to it in the first script that has one (or the whole
/// of a JSON script whose id is `name`, as Next.js writes
/// `__NEXT_DATA__`). Without, every object and array in every script, a
/// JSON script read whole.
pub fn scripts(root: Node<'_>, name: Option<&str>) -> Vec<JsValue> {
    let mut out = Vec::new();
    let elements = std::iter::once(root)
        .chain(root.descendants())
        .filter(|n| n.kind() == NodeKind::Element && n.tag() == Some("script"));
    for script in elements {
        let Some(json) = script_kind(script.attr("type").unwrap_or_default()) else {
            continue;
        };
        let text = script.text();
        match name {
            Some(name) => {
                let found = if json && script.attr("id") == Some(name) {
                    whole(&text)
                } else {
                    assigned(&text, name)
                };
                if let Some(value) = found {
                    out.push(value);
                    break;
                }
            }
            None if json => out.extend(whole(&text)),
            None => out.extend(all(&text)),
        }
    }
    out
}

/// Some(true) for a JSON script, Some(false) for JavaScript, None for
/// templates and anything else a browser wouldn't run.
fn script_kind(kind: &str) -> Option<bool> {
    let kind = kind.split(';').next().unwrap_or_default().trim();
    let kind = kind.to_ascii_lowercase();
    match kind.as_str() {
        ""
        | "module"
        | "text/javascript"
        | "application/javascript"
        | "text/ecmascript"
        | "application/ecmascript"
        | "application/x-javascript" => Some(false),
        k if k == "application/json" || k.ends_with("+json") => Some(true),
        _ => None,
    }
}

/// A script's text as one value, unwrapped from an HTML comment or CDATA.
fn whole(text: &str) -> Option<JsValue> {
    let mut text = text.trim();
    for (open, close) in [("<!--", "-->"), ("<![CDATA[", "]]>")] {
        if let Some(inner) = text.strip_prefix(open).and_then(|t| t.strip_suffix(close)) {
            text = inner.trim();
        }
    }
    let mut r = Reader::new(text);
    let value = r.value(0).ok()?;
    r.skip_space();
    (r.pos == r.s.len()).then_some(value)
}

struct Reader<'a> {
    text: &'a str,
    s: &'a [u8],
    pos: usize,
    /// Where an object or array was tried and turned out not to be one.
    /// The same bytes read the same way, so they're never tried again.
    /// Indexed by byte, made the first time something fails.
    failed: Vec<bool>,
    /// Bytes read by attempts that failed, against `budget`.
    spent: usize,
    budget: usize,
    /// A function's parameter names, read as placeholders for the
    /// arguments it's called with (see [`nuxt`]).
    params: Vec<String>,
}

type Res<T> = Result<T, ()>;

impl<'a> Reader<'a> {
    fn new(text: &'a str) -> Self {
        Reader {
            text,
            s: text.as_bytes(),
            pos: 0,
            failed: Vec::new(),
            spent: 0,
            // Known limit: text built to make many overlapping attempts
            // each read far before failing can run out of budget, and
            // then later literals in it aren't found.
            budget: text.len().saturating_mul(16).saturating_add(1 << 20),
            params: Vec::new(),
        }
    }

    /// Tries a value at `at`; on failure counts what it read against the
    /// budget.
    fn try_value(&mut self, at: usize) -> Option<(JsValue, usize)> {
        self.pos = at;
        match self.value(0) {
            Ok(v) => Some((v, self.pos)),
            Err(()) => {
                self.spent += self.pos.saturating_sub(at).max(1);
                None
            }
        }
    }

    fn has_failed(&self, at: usize) -> bool {
        self.failed.get(at).copied().unwrap_or(false)
    }

    fn mark_failed(&mut self, at: usize) {
        if self.failed.is_empty() {
            self.failed = vec![false; self.s.len()];
        }
        self.failed[at] = true;
    }

    fn exhausted(&self) -> bool {
        self.spent > self.budget
    }

    fn literals(&mut self, first_only: bool) -> Vec<JsValue> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(off) = memchr::memchr3(b'{', b'[', b'J', &self.s[from..]) {
            let at = from + off;
            if self.exhausted() {
                break;
            }
            let prev = at.checked_sub(1).map(|i| self.s[i]);
            let skip = match self.s[at] {
                // JSON.parse('...') is read from its decoded string, and the
                // brackets inside that string aren't tried again.
                b'J' => {
                    !self.s[at..].starts_with(b"JSON")
                        || prev.is_some_and(|b| b < 0x80 && is_ident_byte(b))
                }
                // arr[0], f()[1] and m[2][3] are indexing, not arrays;
                // return[1] is an array.
                b'[' => match prev {
                    Some(b']' | b')') => true,
                    Some(b) if b < 0x80 && is_ident_byte(b) => !after_keyword(&self.s[..at]),
                    _ => false,
                },
                _ => false,
            };
            if skip || self.has_failed(at) {
                from = at + 1;
                continue;
            }
            match self.try_value(at) {
                Some((value, end)) => {
                    if matches!(&value, JsValue::Array(v) if !v.is_empty())
                        || matches!(&value, JsValue::Object(v) if !v.is_empty())
                    {
                        out.push(value);
                        if first_only {
                            break;
                        }
                    }
                    from = end;
                }
                None => from = at + 1,
            }
        }
        out
    }

    fn assigned(&mut self, name: &str) -> Option<JsValue> {
        if name.is_empty() {
            return None;
        }
        let finder = memchr::memmem::Finder::new(name.as_bytes());
        let mut from = 0;
        while let Some(off) = finder.find(&self.s[from..]) {
            let at = from + off;
            from = at + 1;
            if self.exhausted() {
                return None;
            }
            let end = at + name.len();
            let before = self.s[..at].last().copied();
            let mut after = end;
            match before {
                // "name": or ["name"]: the same quote must close it.
                Some(q @ (b'"' | b'\'' | b'`')) => {
                    if self.s.get(end) != Some(&q) {
                        continue;
                    }
                    after += 1;
                }
                Some(b) if is_ident_byte(b) => continue,
                _ => {
                    if self.s.get(end).is_some_and(|b| is_ident_byte(*b)) {
                        continue;
                    }
                }
            }
            self.pos = after;
            self.skip_space();
            if self.peek() == Some(b']') {
                self.pos += 1;
                self.skip_space();
            }
            match self.peek() {
                Some(b':') => self.pos += 1,
                // "=", but not "==" or "=>".
                Some(b'=') if !matches!(self.s.get(self.pos + 1), Some(b'=' | b'>')) => {
                    self.pos += 1
                }
                _ => continue,
            }
            self.skip_space();
            let start = self.pos;
            // Skipping a long comment is work too.
            self.spent += start - at;
            if self.has_failed(start) {
                continue;
            }
            if let Some((value, _)) = self.try_value(start) {
                return Some(value);
            }
        }
        None
    }

    // --- reading ------------------------------------------------------

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let end = self.pos + word.len();
        if self.s.get(self.pos..end) == Some(word.as_bytes())
            && !self.s.get(end).is_some_and(|b| is_ident_byte(*b))
        {
            self.pos = end;
            true
        } else {
            false
        }
    }

    /// Skips whitespace and comments.
    fn skip_space(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) => self.pos += 1,
                Some(b'/') if self.s.get(self.pos + 1) == Some(&b'/') => {
                    self.pos = memchr::memchr(b'\n', &self.s[self.pos..])
                        .map_or(self.s.len(), |i| self.pos + i);
                }
                Some(b'/') if self.s.get(self.pos + 1) == Some(&b'*') => {
                    self.pos = memchr::memmem::find(&self.s[self.pos + 2..], b"*/")
                        .map_or(self.s.len(), |i| self.pos + 2 + i + 2);
                }
                Some(0x80..) => {
                    // No-break space, line and paragraph separators, BOM.
                    let c = self.text[self.pos..].chars().next().unwrap_or_default();
                    if matches!(c, '\u{a0}' | '\u{2028}' | '\u{2029}' | '\u{feff}') {
                        self.pos += c.len_utf8();
                    } else {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    fn value(&mut self, depth: usize) -> Res<JsValue> {
        // Python threads can have small stacks; grow this one if needed.
        stacker::maybe_grow(64 * 1024, 1024 * 1024, || self.value_here(depth))
    }

    fn value_here(&mut self, depth: usize) -> Res<JsValue> {
        self.skip_space();
        match self.peek().ok_or(())? {
            b'{' | b'[' => {
                let start = self.pos;
                let got = if depth >= MAX_DEPTH {
                    Err(())
                } else if self.s[start] == b'{' {
                    self.object(depth + 1)
                } else {
                    self.array(depth + 1)
                };
                if got.is_err() {
                    self.mark_failed(start);
                }
                got
            }
            q @ (b'"' | b'\'' | b'`') => self.string(q).map(JsValue::Str),
            b'0'..=b'9' | b'.' | b'-' | b'+' => self.number(),
            b'!' => {
                // Minified true and false: !0 and !1.
                self.pos += 1;
                match self.peek() {
                    Some(b'0') => {
                        self.pos += 1;
                        Ok(JsValue::Bool(true))
                    }
                    Some(b'1') => {
                        self.pos += 1;
                        Ok(JsValue::Bool(false))
                    }
                    _ => Err(()),
                }
            }
            _ => self.word(depth),
        }
    }

    fn word(&mut self, depth: usize) -> Res<JsValue> {
        for (word, value) in [
            ("true", JsValue::Bool(true)),
            ("false", JsValue::Bool(false)),
            ("null", JsValue::Null),
            ("undefined", JsValue::Null),
            ("NaN", JsValue::Float(f64::NAN)),
            ("Infinity", JsValue::Float(f64::INFINITY)),
        ] {
            if self.eat_word(word) {
                return Ok(value);
            }
        }
        if self.eat_word("void") {
            // `void 0`, minified undefined.
            self.skip_space();
            self.number()?;
            return Ok(JsValue::Null);
        }
        if self.eat_word("new") {
            self.skip_space();
            if !self.eat_word("Date") {
                return Err(());
            }
            let mut args = self.arguments(depth)?;
            return Ok(match args.len() {
                0 => JsValue::Null,
                1 => args.pop().unwrap_or(JsValue::Null),
                _ => JsValue::Array(args),
            });
        }
        if self.eat_word("JSON") {
            self.skip_space();
            if !self.eat(b'.') {
                return Err(());
            }
            self.skip_space();
            if !self.eat_word("parse") {
                return Err(());
            }
            // JSON.parse('{"a": 1}'): the string read as data, one level
            // down so nesting through it still counts.
            let args = self.arguments(depth)?;
            let [JsValue::Str(inner)] = args.as_slice() else {
                return Err(());
            };
            let mut r = Reader::new(inner);
            let value = r.value(depth + 1)?;
            r.skip_space();
            return if r.pos == r.s.len() {
                Ok(value)
            } else {
                Err(())
            };
        }
        if !self.params.is_empty() {
            let start = self.pos;
            while self.peek().is_some_and(is_ident_byte) {
                self.pos += 1;
            }
            let name = &self.text[start..self.pos];
            if let Some(i) = self.params.iter().position(|p| p == name) {
                return Ok(JsValue::Str(format!("{PARAM}{i}")));
            }
            self.pos = start;
        }
        Err(())
    }

    /// `( value, value )` after a function's name. A call counts as a level
    /// of nesting, so `JSON.parse(JSON.parse(...` can't recurse without end.
    fn arguments(&mut self, depth: usize) -> Res<Vec<JsValue>> {
        let depth = depth + 1;
        self.skip_space();
        if depth > MAX_DEPTH || !self.eat(b'(') {
            return Err(());
        }
        let mut args = Vec::new();
        loop {
            self.skip_space();
            if self.eat(b')') {
                return Ok(args);
            }
            args.push(self.value(depth)?);
            self.skip_space();
            if !self.eat(b',') {
                self.skip_space();
                return if self.eat(b')') { Ok(args) } else { Err(()) };
            }
        }
    }

    fn object(&mut self, depth: usize) -> Res<JsValue> {
        self.pos += 1;
        let mut members = Vec::new();
        loop {
            self.skip_space();
            if self.eat(b'}') {
                return Ok(JsValue::Object(members));
            }
            let key = self.key()?;
            self.skip_space();
            if !self.eat(b':') {
                return Err(());
            }
            let value = self.value(depth)?;
            members.push((key, value));
            self.skip_space();
            if !self.eat(b',') {
                self.skip_space();
                return if self.eat(b'}') {
                    Ok(JsValue::Object(members))
                } else {
                    Err(())
                };
            }
        }
    }

    fn array(&mut self, depth: usize) -> Res<JsValue> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.eat(b']') {
                return Ok(JsValue::Array(items));
            }
            items.push(self.value(depth)?);
            self.skip_space();
            if !self.eat(b',') {
                self.skip_space();
                return if self.eat(b']') {
                    Ok(JsValue::Array(items))
                } else {
                    Err(())
                };
            }
        }
    }

    /// An object key: a string, a number or a bare identifier.
    fn key(&mut self) -> Res<String> {
        match self.peek().ok_or(())? {
            q @ (b'"' | b'\'' | b'`') => self.string(q),
            b'0'..=b'9' => {
                let start = self.pos;
                while self
                    .peek()
                    .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'.')
                {
                    self.pos += 1;
                }
                Ok(self.text[start..self.pos].to_string())
            }
            b if is_ident_byte(b) => {
                let start = self.pos;
                while self.peek().is_some_and(is_ident_byte) {
                    self.pos += 1;
                }
                Ok(self.text[start..self.pos].to_string())
            }
            _ => Err(()),
        }
    }

    /// A quoted string, escapes read. In a template (backticks) `${...}`
    /// is kept as written.
    fn string(&mut self, quote: u8) -> Res<String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            // Copy up to the next quote, backslash or line break in one go.
            let rest = &self.s[self.pos..];
            let Some(run) = rest
                .iter()
                .position(|&b| b == quote || b == b'\\' || (quote != b'`' && b == b'\n'))
            else {
                // Read to the end without finding the close.
                self.pos = self.s.len();
                return Err(());
            };
            out.push_str(&self.text[self.pos..self.pos + run]);
            self.pos += run;
            match self.s[self.pos] {
                b'\\' => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                b'\n' => return Err(()),
                _ => {
                    self.pos += 1;
                    return Ok(out);
                }
            }
        }
    }

    /// The escape after a backslash.
    fn escape(&mut self, out: &mut String) -> Res<()> {
        let c = self.text[self.pos..].chars().next().ok_or(())?;
        self.pos += c.len_utf8();
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !self.peek().is_some_and(|b| b.is_ascii_digit()) => out.push('\0'),
            'x' => {
                let code = self.hex(2)?;
                out.push(char::from_u32(code).ok_or(())?);
            }
            'u' => {
                let code = self.unicode()?;
                // A surrogate pair is written as two escapes.
                if (0xd800..0xdc00).contains(&code)
                    && self.s.get(self.pos..self.pos + 2) == Some(b"\\u")
                {
                    let save = self.pos;
                    self.pos += 2;
                    let low = self.unicode()?;
                    if (0xdc00..0xe000).contains(&low) {
                        let joined = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                        out.push(char::from_u32(joined).ok_or(())?);
                        return Ok(());
                    }
                    self.pos = save;
                }
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            // A backslash before a line break continues the line.
            '\n' => {}
            '\r' => {
                self.eat(b'\n');
            }
            other => out.push(other),
        }
        Ok(())
    }

    fn hex(&mut self, n: usize) -> Res<u32> {
        let digits = self.text.get(self.pos..self.pos + n).ok_or(())?;
        let code = u32::from_str_radix(digits, 16).map_err(|_| ())?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(());
        }
        self.pos += n;
        Ok(code)
    }

    /// The digits of `\uXXXX` or `\u{X...}`.
    fn unicode(&mut self) -> Res<u32> {
        if self.eat(b'{') {
            let close = self.s[self.pos..]
                .iter()
                .take(8)
                .position(|&b| b == b'}')
                .ok_or(())?;
            let code = self.hex(close)?;
            self.pos += 1;
            return Ok(code);
        }
        self.hex(4)
    }

    fn number(&mut self) -> Res<JsValue> {
        let negative = match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                true
            }
            Some(b'+') => {
                self.pos += 1;
                false
            }
            _ => false,
        };
        self.skip_space();
        if self.eat_word("Infinity") {
            let inf = if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
            return Ok(JsValue::Float(inf));
        }
        let start = self.pos;
        if self.s.get(start) == Some(&b'0')
            && let Some(radix) = match self.s.get(start + 1) {
                Some(b'x' | b'X') => Some(16),
                Some(b'o' | b'O') => Some(8),
                Some(b'b' | b'B') => Some(2),
                _ => None,
            }
        {
            self.pos += 2;
            let digits_start = self.pos;
            while self.peek().is_some_and(|b| b.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let digits = &self.text[digits_start..self.pos];
            let n = i128::from_str_radix(digits, radix).map_err(|_| ())?;
            return Ok(int(if negative { -n } else { n }));
        }
        let digits = |r: &mut Self| {
            let from = r.pos;
            while r.peek().is_some_and(|b| b.is_ascii_digit()) {
                r.pos += 1;
            }
            r.pos > from
        };
        let whole = digits(self);
        let mut float = false;
        if self.eat(b'.') {
            float = true;
            if !digits(self) && !whole {
                return Err(());
            }
        } else if !whole {
            return Err(());
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digits(self) {
                return Err(());
            }
        }
        // "1px" isn't a number. (Anything outside ASCII may be a space.)
        if self.peek().is_some_and(|b| b < 0x80 && is_ident_byte(b)) {
            return Err(());
        }
        let text = &self.text[start..self.pos];
        if !float && let Ok(n) = text.parse::<i64>() {
            return Ok(JsValue::Int(if negative { -n } else { n }));
        }
        if !float {
            let digits = text.trim_start_matches('0');
            let digits = if digits.is_empty() { "0" } else { digits };
            let sign = if negative { "-" } else { "" };
            return Ok(JsValue::BigInt(format!("{sign}{digits}")));
        }
        let f: f64 = text.parse().map_err(|_| ())?;
        Ok(JsValue::Float(if negative { -f } else { f }))
    }
}

/// Whether `before` ends with a keyword that an expression can follow, as
/// in minified `return[1,2]`, rather than a name being indexed.
fn after_keyword(before: &[u8]) -> bool {
    const KEYWORDS: &[&[u8]] = &[
        b"return", b"case", b"of", b"in", b"typeof", b"void", b"else", b"do", b"yield", b"await",
        b"delete", b"throw",
    ];
    let start = before
        .iter()
        .rposition(|&b| !is_ident_byte(b))
        .map_or(0, |i| i + 1);
    let word = &before[start..];
    // "obj.in[1]" is a property called "in".
    KEYWORDS.contains(&word) && before[..start].last() != Some(&b'.')
}

fn int(n: i128) -> JsValue {
    match i64::try_from(n) {
        Ok(n) => JsValue::Int(n),
        Err(_) => JsValue::BigInt(n.to_string()),
    }
}

/// Bytes that can be part of an identifier: letters, digits, `_`, `$`, and
/// anything outside ASCII.
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

#[cfg(test)]
mod tests {
    use super::JsValue::*;
    use super::*;

    fn obj(members: &[(&str, JsValue)]) -> JsValue {
        Object(
            members
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    fn s(text: &str) -> JsValue {
        Str(text.into())
    }

    #[test]
    fn nuxt_two_state_is_read_through_its_function() {
        let lamp = obj(&[(
            "product",
            obj(&[
                ("name", s("Lamp")),
                ("price", Float(49.99)),
                ("tags", Array(vec![s("new"), s("Lamp")])),
                ("gone", Null),
            ]),
        )]);
        for text in [
            r#"window.__NUXT__=(function(a,b,c){return {data:[{product:{name:a,price:b,tags:[c,a],gone:void 0}}]}}("Lamp",49.99,"new"));"#,
            r#"window.__NUXT__ = (function (a, b, c) { return {data: [{product: {name: a, price: b, tags: [c, a], gone: void 0}}]}; })("Lamp", 49.99, "new")"#,
        ] {
            let got = nuxt(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(got, obj(&[("data", Array(vec![lamp.clone()]))]), "{text}");
        }
        // Not Nuxt's shape, or a body that isn't data.
        assert_eq!(
            nuxt("window.__NUXT__ = {a: 1}"),
            Some(obj(&[("a", Int(1))]))
        );
        assert_eq!(nuxt("window.__NUXT__=(function(a){return go(a)}(1))"), None);
        assert_eq!(nuxt("var x = 1"), None);
        // One big argument used many times isn't copied without end.
        let big = format!("[{}]", "1,".repeat(100_000));
        let uses = "a,".repeat(5_000);
        let text = format!("window.__NUXT__=(function(a){{return [{uses}]}}({big}))");
        assert_eq!(nuxt(&text), None);
    }

    #[test]
    fn what_json_does_not_allow() {
        let cases: &[(&str, JsValue)] = &[
            (r#"{"a": 1}"#, obj(&[("a", Int(1))])),
            (
                "{a: 1, 'b': 2, `c`: 3}",
                obj(&[("a", Int(1)), ("b", Int(2)), ("c", Int(3))]),
            ),
            (
                "{$id: 1, _x: 2, ünï: 3}",
                obj(&[("$id", Int(1)), ("_x", Int(2)), ("ünï", Int(3))]),
            ),
            ("{1: 'a', 2.5: 'b'}", obj(&[("1", s("a")), ("2.5", s("b"))])),
            ("[1, 2, 3,]", Array(vec![Int(1), Int(2), Int(3)])),
            ("{a: 1,}", obj(&[("a", Int(1))])),
            (
                "['single', \"double\", `back`]",
                Array(vec![s("single"), s("double"), s("back")]),
            ),
            ("[`hi ${name}!`]", Array(vec![s("hi ${name}!")])),
            ("[`two\nlines`]", Array(vec![s("two\nlines")])),
            (
                "[undefined, null, true, false, !0, !1, void 0]",
                Array(vec![
                    Null,
                    Null,
                    Bool(true),
                    Bool(false),
                    Bool(true),
                    Bool(false),
                    Null,
                ]),
            ),
            (
                "[0x1F, 0o17, 0b101, -0x10, 1e3, -2.5, .5, 5., +3, 1E-2]",
                Array(vec![
                    Int(31),
                    Int(15),
                    Int(5),
                    Int(-16),
                    Float(1000.0),
                    Float(-2.5),
                    Float(0.5),
                    Float(5.0),
                    Int(3),
                    Float(0.01),
                ]),
            ),
            (
                "[Infinity, -Infinity]",
                Array(vec![Float(f64::INFINITY), Float(f64::NEG_INFINITY)]),
            ),
            (
                "[123456789012345678901234567890, -99999999999999999999]",
                Array(vec![
                    BigInt("123456789012345678901234567890".into()),
                    BigInt("-99999999999999999999".into()),
                ]),
            ),
            (
                "{a: /* inline */ 1, // to the end\n b: 2}",
                obj(&[("a", Int(1)), ("b", Int(2))]),
            ),
            (
                "{d: new Date(1760000000000), e: new Date('2026-10-12'), f: new Date(2026, 9, 12)}",
                obj(&[
                    ("d", Int(1_760_000_000_000)),
                    ("e", s("2026-10-12")),
                    ("f", Array(vec![Int(2026), Int(9), Int(12)])),
                ]),
            ),
            (
                r#"{s: JSON.parse("{\"a\": [1, 2]}")}"#,
                obj(&[("s", obj(&[("a", Array(vec![Int(1), Int(2)]))]))]),
            ),
            (
                r#"['a\'b', "c\"d", 'tab\there', '\x41B\u{43}', '😀', 'line\
continued', '\q']"#,
                Array(vec![
                    s("a'b"),
                    s("c\"d"),
                    s("tab\there"),
                    s("ABC"),
                    s("\u{1F600}"),
                    s("linecontinued"),
                    s("q"),
                ]),
            ),
            ("[\u{a0}1\u{2028}]", Array(vec![Int(1)])),
            (
                "{a: {b: [{c: [[]]}]}}",
                obj(&[(
                    "a",
                    obj(&[("b", Array(vec![obj(&[("c", Array(vec![Array(vec![])]))])]))]),
                )]),
            ),
        ];
        for (text, want) in cases {
            assert_eq!(first(text).as_ref(), Some(want), "{text:?}");
        }
        // NaN is never equal to itself.
        assert!(matches!(first("[NaN]"), Some(Array(v)) if matches!(v[0], Float(f) if f.is_nan())));
    }

    #[test]
    fn found_in_surrounding_code() {
        let script = r#"
            var s = "{not: data";  // a brace in a string
            function go() { return x[i]; }
            window.__INITIAL_STATE__ = {products: [{id: 1, 'name': 'Lamp',}], total: 42,};
            var other = [1, 2];
        "#;
        let state = obj(&[
            (
                "products",
                Array(vec![obj(&[("id", Int(1)), ("name", s("Lamp"))])]),
            ),
            ("total", Int(42)),
        ]);
        assert_eq!(first(script), Some(state));
        let found = all(script);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[1], Array(vec![Int(1), Int(2)]));
        assert_eq!(first("no data here"), None);
        assert_eq!(
            first("{} [] {a: }"),
            None,
            "empty and broken ones aren't data"
        );
        assert_eq!(all("x = {a: 1}; y = {b: 2}").len(), 2);
    }

    #[test]
    fn json_parse_payloads_are_decoded_first() {
        let single = r#"window.__S__ = JSON.parse('{"name": "caf\\u00e9", "n": [1]}');"#;
        let double = r#"var s = JSON.parse("{\"name\": \"caf\\u00e9\", \"n\": [1]}");"#;
        let want = obj(&[("name", s("café")), ("n", Array(vec![Int(1)]))]);
        for script in [single, double] {
            assert_eq!(first(script), Some(want.clone()), "{script}");
            // The [1] inside the string isn't listed again.
            assert_eq!(all(script), vec![want.clone()], "{script}");
        }
        // A JSON.parse of something other than an object or array is skipped.
        assert_eq!(
            all("JSON.parse('1'); x = {a: 1}"),
            vec![obj(&[("a", Int(1))])]
        );
        assert_eq!(all("JSONP({a: 1})"), vec![obj(&[("a", Int(1))])]);
    }

    #[test]
    fn code_that_is_not_data_is_not_read() {
        let nuxt = "window.__NUXT__=(function(a,b){return{x:a,y:b}}(1,2));";
        assert_eq!(assigned(nuxt, "__NUXT__"), None);
        assert_eq!(first(nuxt), None);
        for literal in [
            "{a, b}",
            "{...rest, a: 1}",
            "{f: function() { return 1 }}",
            "{f() {}}",
            "{re: /x+/g}",
            "{n: 10n}",
            "{n: 1_000}",
        ] {
            assert_eq!(first(literal), None, "{literal}");
            assert_eq!(assigned(&format!("x = {literal}"), "x"), None, "{literal}");
        }
        // What's nested inside one can still be found on its own.
        assert_eq!(first("{n: 10n, list: [1]}"), Some(Array(vec![Int(1)])));
    }

    #[test]
    fn index_expressions_are_not_literals() {
        assert_eq!(
            all("var x = arr[0]; f(a)[1]; m[2][3]; y = [4]"),
            vec![Array(vec![Int(4)])]
        );
        assert_eq!(
            all(r#"window["__S__"] = {a: 1};"#),
            vec![obj(&[("a", Int(1))])]
        );
        assert_eq!(all("return [1, 2]"), vec![Array(vec![Int(1), Int(2)])]);
        assert_eq!(
            all("x=[1];(a,[2])"),
            vec![Array(vec![Int(1)]), Array(vec![Int(2)])]
        );
        // Minifiers write keywords straight against an array.
        for code in [
            "return[1]",
            "case[1]",
            "for(x of[1])",
            "if(a in[1])",
            "typeof[1]",
            "void[1]",
            "else[1]",
            "do[1]",
            "yield[1]",
            "await[1]",
            "delete[1]",
            "throw[1]",
        ] {
            assert_eq!(all(code), vec![Array(vec![Int(1)])], "{code}");
        }
        assert_eq!(
            first("function(){return[{a:1},{b:2}]}"),
            Some(Array(vec![obj(&[("a", Int(1))]), obj(&[("b", Int(2))])]))
        );
        // ...but a name that ends in a keyword is still a name.
        assert_eq!(all("xreturn[1]; my_case[2]; $in[3]; obj.in[4]"), vec![]);
    }

    #[test]
    fn values_by_name() {
        let script = r#"
            var metadata = {wrong: true};
            window.__STATE__ = {"page": 1};
            var data = [1, 2];
            let count = 7;
            window["config"] = {debug: false};
            const app = {settings: {theme: 'dark'}, "items": [3]};
            if (mode == "x") {}
            var cb = () => 1;
        "#;
        assert_eq!(
            assigned(script, "__STATE__"),
            Some(obj(&[("page", Int(1))]))
        );
        assert_eq!(assigned(script, "data"), Some(Array(vec![Int(1), Int(2)])));
        assert_eq!(assigned(script, "count"), Some(Int(7)));
        assert_eq!(
            assigned(script, "config"),
            Some(obj(&[("debug", Bool(false))]))
        );
        assert_eq!(
            assigned(script, "settings"),
            Some(obj(&[("theme", s("dark"))]))
        );
        assert_eq!(assigned(script, "items"), Some(Array(vec![Int(3)])));
        assert_eq!(assigned(script, "mode"), None);
        assert_eq!(assigned(script, "cb"), None);
        assert_eq!(assigned(script, "missing"), None);
        assert_eq!(assigned(script, ""), None);
        assert_eq!(
            assigned("window.__S__ = JSON.parse('{\"a\":1}');", "__S__"),
            Some(obj(&[("a", Int(1))]))
        );
    }

    #[test]
    fn hostile_input_ends_quickly() {
        use std::time::{Duration, Instant};
        // Each case at two sizes: twice the input may take about twice the
        // time, never four times, which is what reading to the end once per
        // position costs. Timed by growth rather than against a clock, so a
        // slow machine can't fail it and a fast one can't hide it.
        let cases = |big: usize| {
            [
                "[".repeat(big),
                "{".repeat(big),
                "{a:".repeat(big),
                "[1,".repeat(big / 3),
                format!("[{}", "0,".repeat(big / 2)),
                "[\"".repeat(big / 2),
                "['['".repeat(big / 4),
                "x=[".repeat(big / 3),
                "/*".repeat(big / 2),
                "\"".repeat(big),
                "{a:[{a:[".repeat(big / 8),
                // Every attempt would read to the end of an unclosed comment
                // or string.
                "{/*".repeat(big / 3),
                "x=/*".repeat(big / 4),
                "x='[".repeat(big / 4),
                "{a:'".repeat(big / 4),
                "JSON.parse(".repeat(big / 11),
                "[new Date(".repeat(big / 10),
            ]
        };
        let time = |text: &str| {
            // The quicker of two runs, so a pause elsewhere isn't counted.
            (0..2)
                .map(|_| {
                    let started = Instant::now();
                    let _ = first(text);
                    let _ = all(text);
                    let _ = assigned(text, "x");
                    let _ = assigned(text, "a");
                    started.elapsed()
                })
                .min()
                .unwrap()
        };
        for (half, full) in cases(250_000).iter().zip(cases(500_000).iter()) {
            // A busy machine can stall one timing for longer than the whole
            // run takes; work that grows faster than its input misses the
            // bound every time.
            let fits = |(small, large): (Duration, Duration)| {
                large < small * 3 + Duration::from_millis(50)
            };
            let mut timed = (time(half), time(full));
            for _ in 0..2 {
                if fits(timed) {
                    break;
                }
                timed = (time(half), time(full));
            }
            let (small, large) = timed;
            assert!(
                fits((small, large)),
                "{:?}...: {:?} for half the input, {:?} for all of it",
                &full[..20.min(full.len())],
                small,
                large
            );
        }
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(time(&deep) < Duration::from_secs(5));
        // Nesting up to the limit is read; past it is not.
        let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert!(first(&deep(MAX_DEPTH)).is_some());
        assert_eq!(whole(&deep(MAX_DEPTH + 1)), None);
    }

    #[test]
    fn whole_scripts() {
        assert_eq!(whole(" {\"a\": 1} "), Some(obj(&[("a", Int(1))])));
        assert_eq!(whole("<!-- [1] -->"), Some(Array(vec![Int(1)])));
        assert_eq!(whole("\"just a string\""), Some(s("just a string")));
        assert_eq!(whole("{\"a\": 1} trailing"), None);
    }

    #[test]
    fn script_kinds() {
        for (kind, want) in [
            ("", Some(false)),
            ("text/javascript", Some(false)),
            ("module", Some(false)),
            ("application/json", Some(true)),
            ("application/ld+json", Some(true)),
            ("Application/JSON; charset=utf-8", Some(true)),
            ("text/template", None),
            ("text/x-handlebars", None),
        ] {
            assert_eq!(script_kind(kind), want, "{kind:?}");
        }
    }
}
