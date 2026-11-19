//! html5lib-tests' tokenizer suite: the tokens, and then the parse errors
//! with their line and column.

use netweir_html::tokenizer::{Attrs, Doctype, Sink, Tag, TextMode, Tokenizer, preprocess};
use serde_json::{Value, json};

mod summary;

#[derive(Default)]
struct Collect {
    tokens: Vec<Value>,
    text: String,
    errors: Vec<(&'static str, usize)>,
}

impl Collect {
    fn flush(&mut self) {
        if !self.text.is_empty() {
            self.tokens
                .push(json!(["Character", std::mem::take(&mut self.text)]));
        }
    }
    fn push(&mut self, token: Value) {
        self.flush();
        self.tokens.push(token);
    }
}

fn attrs(attrs: Attrs<'_>) -> Value {
    let mut map = serde_json::Map::new();
    for (name, value) in attrs {
        map.insert(name.to_string(), json!(value));
    }
    Value::Object(map)
}

impl Sink for Collect {
    fn doctype(&mut self, d: &Doctype<'_>) {
        self.push(json!([
            "DOCTYPE",
            d.name,
            d.public_id,
            d.system_id,
            !d.force_quirks
        ]));
    }
    fn start_tag(&mut self, tag: &Tag<'_>) -> Option<TextMode> {
        let token = if tag.self_closing {
            json!(["StartTag", tag.name, attrs(tag.attrs()), true])
        } else {
            json!(["StartTag", tag.name, attrs(tag.attrs())])
        };
        self.push(token);
        None
    }
    fn end_tag(&mut self, tag: &Tag<'_>) {
        self.push(json!(["EndTag", tag.name]));
    }
    fn comment(&mut self, text: &str) {
        self.push(json!(["Comment", text]));
    }
    fn processing_instruction(&mut self, target: &str, data: &str) {
        self.push(json!(["ProcessingInstruction", target, data]));
    }
    fn text(&mut self, text: &str) {
        self.text.push_str(text);
    }
    fn eof(&mut self) {
        self.flush();
    }
    fn error(&mut self, code: &'static str, at: usize) {
        self.errors.push((code, at));
    }
}

/// The 1-based line and column of byte `at` of `input`, the column in
/// UTF-16 code units as the suite counts them (U+100000 is two).
fn line_col(input: &str, at: usize) -> (u64, u64) {
    let before = &input[..at];
    let line = before.matches('\n').count() + 1;
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    (
        line as u64,
        before[start..].encode_utf16().count() as u64 + 1,
    )
}

/// Tests whose errors another test contradicts: domjs.test's `<?` then
/// a CR is, once preprocessed, test3.test's `<?` then an LF, which
/// expects the error the processing instructions change brought in
/// (`invalid-first-character-of-processing-instruction-target`, after the
/// `?`) where these still expect the one from before it.
const CONTRADICTED: &[&str] = &[
    "CR in bogus comment state",
    "CRLF in bogus comment state",
    "CRLFLF in bogus comment state",
];

/// What the suite's README asks for: each error's code, line and column.
/// Compared as sets, since an error found by looking ahead may be
/// reported before one the spec's character-at-a-time reading finds
/// first.
fn sorted_errors(errors: impl Iterator<Item = (u64, u64, String)>) -> Vec<(u64, u64, String)> {
    let mut errors: Vec<_> = errors.collect();
    errors.sort();
    errors
}

/// `\uXXXX` escapes in a doubleEscaped test, or None for a lone surrogate,
/// which a Rust `str` can't hold.
fn unescape(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).unwrap())?);
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    Some(out)
}

fn unescape_value(v: &Value) -> Option<Value> {
    Some(match v {
        Value::String(s) => Value::String(unescape(s)?),
        Value::Array(a) => Value::Array(a.iter().map(unescape_value).collect::<Option<_>>()?),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| Some((unescape(k)?, unescape_value(v)?)))
                .collect::<Option<_>>()?,
        ),
        other => other.clone(),
    })
}

fn state(name: &str) -> Option<TextMode> {
    match name {
        "Data state" => None,
        "PLAINTEXT state" => Some(TextMode::Plaintext),
        "RCDATA state" => Some(TextMode::Rcdata),
        "RAWTEXT state" => Some(TextMode::Rawtext),
        "Script data state" => Some(TextMode::ScriptData),
        "CDATA section state" => Some(TextMode::Cdata),
        other => panic!("unknown state {other}"),
    }
}

/// Adjacent character tokens merged, as the suite's README says to.
fn merged(tokens: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for t in tokens {
        if t[0] == "Character"
            && let Some(last) = out.last_mut()
            && last[0] == "Character"
        {
            let joined = format!("{}{}", last[1].as_str().unwrap(), t[1].as_str().unwrap());
            last[1] = json!(joined);
            continue;
        }
        out.push(t.clone());
    }
    out
}

#[test]
fn html5lib_tokenizer_tests() {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/html5lib-tests/tokenizer"
    );
    let (mut passed, mut skipped) = (0, 0);
    let mut failures = Vec::new();
    let (mut errors_passed, mut error_failures, mut contradicted) = (0, Vec::new(), 0);
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    for path in files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "test"))
    {
        let suite: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let Some(tests) = suite.get("tests").and_then(Value::as_array) else {
            continue;
        };
        for test in tests {
            let double = test.get("doubleEscaped").and_then(Value::as_bool) == Some(true);
            let (input, expected) = if double {
                match (
                    unescape(test["input"].as_str().unwrap()),
                    unescape_value(&test["output"]),
                ) {
                    (Some(i), Some(o)) => (i, o),
                    _ => {
                        skipped += 1;
                        continue;
                    }
                }
            } else {
                (
                    test["input"].as_str().unwrap().to_string(),
                    test["output"].clone(),
                )
            };
            let expected = merged(expected.as_array().unwrap());
            let states = test
                .get("initialStates")
                .and_then(Value::as_array)
                .map(|s| s.iter().map(|v| v.as_str().unwrap().to_string()).collect())
                .unwrap_or_else(|| vec!["Data state".to_string()]);
            for name in states {
                let mut sink = Collect::default();
                let input = preprocess(&input);
                let mut tokenizer = Tokenizer::new(&input);
                if let Some(mode) = state(&name) {
                    tokenizer.set_text_mode(mode);
                }
                if let Some(last) = test.get("lastStartTag").and_then(Value::as_str) {
                    tokenizer.set_last_start_tag(last);
                }
                tokenizer.run(&mut sink);
                let got = merged(&sink.tokens);
                if got == expected {
                    passed += 1;
                } else {
                    failures.push(format!(
                        "{} / {} [{name}]\n  input:    {input:?}\n  expected: {}\n  got:      {}",
                        path.file_name().unwrap().to_string_lossy(),
                        test["description"],
                        Value::Array(expected.clone()),
                        Value::Array(got)
                    ));
                }
                // Again, reporting errors, which takes the slow path
                // through tags: the tokens are the same, and so must be
                // the errors.
                let mut sink = Collect::default();
                let mut tokenizer = Tokenizer::new(&input);
                tokenizer.report_errors();
                if let Some(mode) = state(&name) {
                    tokenizer.set_text_mode(mode);
                }
                if let Some(last) = test.get("lastStartTag").and_then(Value::as_str) {
                    tokenizer.set_last_start_tag(last);
                }
                tokenizer.run(&mut sink);
                let want = sorted_errors(test.get("errors").and_then(Value::as_array).map_or(
                    Vec::new().into_iter(),
                    |e| {
                        e.iter()
                            .map(|e| {
                                (
                                    e["line"].as_u64().unwrap(),
                                    e["col"].as_u64().unwrap(),
                                    e["code"].as_str().unwrap().to_string(),
                                )
                            })
                            .collect::<Vec<_>>()
                            .into_iter()
                    },
                ));
                let got_errors = sorted_errors(sink.errors.iter().map(|&(code, at)| {
                    let (line, col) = line_col(&input, at);
                    (line, col, code.to_string())
                }));
                if want == got_errors && merged(&sink.tokens) == expected {
                    errors_passed += 1;
                } else if path.ends_with("domjs.test")
                    && CONTRADICTED.contains(&test["description"].as_str().unwrap())
                {
                    contradicted += 1;
                } else {
                    error_failures.push(format!(
                        "{} / {} [{name}]\n  input:    {input:?}\n  expected: {want:?}\n  got:      {got_errors:?}",
                        path.file_name().unwrap().to_string_lossy(),
                        test["description"],
                    ));
                }
            }
        }
    }
    eprintln!(
        "tokenizer: {passed} passed, {} failed, {skipped} skipped (lone surrogates)",
        failures.len()
    );
    eprintln!(
        "tokenizer errors: {errors_passed} passed, {} failed, {contradicted} skipped \
         (another test expects otherwise), {skipped} skipped (lone surrogates)",
        error_failures.len()
    );
    summary::write(&summary::Counts {
        suite: "html5lib-tokenizer",
        passed,
        failed: failures.len(),
        skipped,
        skipped_because: "lone surrogates, which a Rust string can't hold",
        data: std::path::Path::new(dir).parent().unwrap(),
    });
    summary::write(&summary::Counts {
        suite: "html5lib-tokenizer-errors",
        passed: errors_passed,
        failed: error_failures.len(),
        skipped: contradicted + skipped,
        skipped_because: "lone surrogates, which a Rust string can't hold, and \
                          domjs.test's three cases that test3.test contradicts",
        data: std::path::Path::new(dir).parent().unwrap(),
    });
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        error_failures.is_empty(),
        "{} error failures:\n{}",
        error_failures.len(),
        error_failures.join("\n")
    );
}
