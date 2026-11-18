//! Parse errors, against the `#errors` and `#new-errors` sections of the
//! tree-construction tests.
//!
//! `#errors` lists every error a case has, in the formats of the several
//! parsers the cases came from and mostly in html5lib's names, so only
//! the count is compared. `#new-errors` gives the HTML Standard's names
//! with line and column, for the tokenizer errors whose names changed
//! since; each must be among the errors found, at that place.

use netweir_html::{Document, Options, SyntaxError, parse_fragment_with, parse_with};

mod inputs;

fn parse(data: &str, case: &inputs::TreeInput, scripting: bool) -> Document {
    let options = Options {
        errors: true,
        scripting,
        ..Options::default()
    };
    match &case.fragment {
        Some((ns, context)) => parse_fragment_with(data, *ns, context, options),
        None => parse_with(data, options),
    }
    .unwrap()
}

/// `(1:5) abrupt-closing-of-empty-comment` as its parts.
fn new_error(line: &str) -> Option<(usize, usize, &str)> {
    let (at, code) = line.strip_prefix('(')?.split_once(") ")?;
    let (l, c) = at.split_once(':')?;
    Some((l.parse().ok()?, c.parse().ok()?, code.trim()))
}

#[test]
fn tree_construction_errors() {
    let (mut counted, mut miscounted) = (0, Vec::new());
    let (mut found, mut missing) = (0, Vec::new());
    for case in inputs::tree_inputs() {
        // As tests/tree.rs: these need script run during the parse.
        if case.file.starts_with("scripted_") && case.scripting == Some(true) {
            continue;
        }
        let doc = parse(&case.data, &case, case.scripting.unwrap_or(false));
        let errors = doc.errors();
        let codes = || {
            errors
                .iter()
                .map(|e| format!("({}:{}) {}", e.line, e.column, e.code))
                .collect::<Vec<_>>()
        };
        if errors.len() == case.errors.len() {
            counted += 1;
        } else {
            miscounted.push(format!(
                "{}: {:?}\n  expected {}: {:?}\n  found {}: {:?}",
                case.file,
                case.data,
                case.errors.len(),
                case.errors,
                errors.len(),
                codes()
            ));
        }
        for line in &case.new_errors {
            let Some((l, c, code)) = new_error(line) else {
                continue;
            };
            let here = |e: &&SyntaxError| e.line == l && e.column == c && e.code == code;
            if errors.iter().any(|e| here(&e)) {
                found += 1;
            } else {
                missing.push(format!(
                    "{}: {:?}: {line} not in {:?}",
                    case.file,
                    case.data,
                    codes()
                ));
            }
        }
    }
    eprintln!(
        "tree construction errors: {counted} counts match, {} don't; \
         #new-errors: {found} found, {} missing",
        miscounted.len(),
        missing.len()
    );
    if std::env::var("ERRORS_BRIEF").is_ok() {
        for m in miscounted.iter().chain(&missing) {
            eprintln!("{m}");
        }
    }
    // A ratchet: as many counts match as when this was written. Most of
    // the rest are cases whose files list no errors at all
    // (processing-instructions.dat, plain-text-unsafe.dat,
    // html5test-com.dat), or where the parsers the cases came from count
    // differently from each other.
    assert!(
        counted >= 1802,
        "only {counted} error counts match; set ERRORS_BRIEF to see them"
    );
    assert!(
        missing.is_empty(),
        "{} #new-errors missing, first 10:\n{}",
        missing.len(),
        missing[..missing.len().min(10)].join("\n")
    );
}

#[test]
fn errors_have_their_names_and_places() {
    let input = "<!DOCTYPE html>\r\n<p a=1 a=2>\0</q>";
    let doc = parse_with(
        input,
        Options {
            errors: true,
            ..Options::default()
        },
    )
    .unwrap();
    let got: Vec<_> = doc
        .errors()
        .iter()
        .map(|e| (e.code, e.line, e.column, &input[e.offset..e.offset + 1]))
        .collect();
    assert_eq!(
        got,
        [
            ("duplicate-attribute", 2, 9, "="),
            ("unexpected-null-character", 2, 12, "\0"),
            ("invalid-codepoint-in-body", 2, 12, "\0"),
            ("unexpected-end-tag", 2, 13, "<"),
        ]
    );
    assert!(
        netweir_html::parse("<p a=1 a=2>", false)
            .errors()
            .is_empty()
    );
}
