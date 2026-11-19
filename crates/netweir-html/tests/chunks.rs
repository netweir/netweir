//! Input fed in pieces builds the tree the whole input does: every input
//! the conformance suites have, cut in two at every character boundary
//! and into many pieces at random, against a parse of the whole.

use netweir_html::{Document, Ns, Options, Parser, parse_fragment_with, parse_with};

mod inputs;

/// Everything a parse made, in the order it made it: each node with its
/// links, attributes and spans, then the errors. Two parses that agree on
/// this built the same tree the same way.
fn dump(doc: &Document) -> String {
    let mut out = String::new();
    for id in 0..doc.len() as u32 {
        out += &format!(
            "{id} {:?} parent {:?} next {:?} contents {:?} span {:?} end {:?}\n",
            doc.kind(id),
            doc.parent(id),
            doc.next_sibling(id),
            doc.template_contents(id),
            doc.span(id),
            doc.end_span(id),
        );
        for (i, a) in doc.attrs_written(id).enumerate() {
            out += &format!("  {a:?} {:?}\n", doc.attr_span(id, i));
        }
    }
    for e in doc.errors() {
        out += &format!("{e:?}\n");
    }
    out
}

struct Case {
    data: String,
    fragment: Option<(Ns, String)>,
    options: Options,
}

impl Case {
    fn whole(&self) -> String {
        dump(
            &match &self.fragment {
                Some((ns, context)) => parse_fragment_with(&self.data, *ns, context, self.options),
                None => parse_with(&self.data, self.options),
            }
            .unwrap(),
        )
    }

    fn pieces<'a>(&self, pieces: impl IntoIterator<Item = &'a str>) -> String {
        let mut parser = match &self.fragment {
            Some((ns, context)) => Parser::fragment(*ns, context, self.options),
            None => Parser::new(self.options),
        };
        for piece in pieces {
            parser.feed(piece);
        }
        dump(&parser.finish().unwrap())
    }
}

/// Every input, as written and with each LF a CRLF (so CRs land at the
/// end of pieces), with spans and errors on to check those too.
fn cases() -> Vec<Case> {
    let options = Options {
        spans: true,
        errors: true,
        ..Options::default()
    };
    let mut out = Vec::new();
    for case in inputs::tree_inputs() {
        // As tests/tree.rs: these need script run during the parse.
        if case.file.starts_with("scripted_") && case.scripting == Some(true) {
            continue;
        }
        let options = Options {
            scripting: case.scripting.unwrap_or(false),
            ..options
        };
        for data in [case.data.clone(), case.data.replace('\n', "\r\n")] {
            out.push(Case {
                data,
                fragment: case.fragment.clone(),
                options,
            });
        }
    }
    for data in inputs::tokenizer_inputs() {
        out.push(Case {
            data,
            fragment: None,
            options,
        });
    }
    out
}

#[test]
fn two_pieces_cut_anywhere_build_the_same_tree() {
    let (mut checked, mut failures) = (0, Vec::new());
    for case in cases() {
        let whole = case.whole();
        for (at, _) in case.data.char_indices().skip(1) {
            checked += 1;
            if case.pieces([&case.data[..at], &case.data[at..]]) != whole {
                failures.push(format!("{:?} cut at {at}", case.data));
            }
        }
    }
    eprintln!("two pieces: {checked} cuts, {} differ", failures.len());
    assert!(
        failures.is_empty(),
        "{} cuts differ, first 20:\n{}",
        failures.len(),
        failures[..failures.len().min(20)].join("\n")
    );
}

#[test]
fn many_pieces_build_the_same_tree() {
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut checked, mut failures) = (0, Vec::new());
    for case in cases() {
        // As given, and without spans or errors: the default parse takes
        // the tokenizer's other way through tags.
        for options in [
            case.options,
            Options {
                scripting: case.options.scripting,
                ..Options::default()
            },
        ] {
            let case = Case {
                options,
                data: case.data.clone(),
                fragment: case.fragment.clone(),
            };
            let whole = case.whole();
            for _ in 0..4 {
                // Pieces of up to eight bytes, empty ones among them,
                // each ending on a character boundary.
                let mut cuts = vec![0];
                let mut at = 0;
                while at < case.data.len() {
                    at = (at + (next() % 9) as usize).min(case.data.len());
                    while !case.data.is_char_boundary(at) {
                        at += 1;
                    }
                    cuts.push(at);
                }
                let pieces: Vec<&str> = cuts.windows(2).map(|w| &case.data[w[0]..w[1]]).collect();
                checked += 1;
                if case.pieces(pieces.iter().copied()) != whole {
                    failures.push(format!("{:?} in {pieces:?}", case.data));
                }
            }
        }
    }
    eprintln!("many pieces: {checked} splits, {} differ", failures.len());
    assert!(
        failures.is_empty(),
        "{} splits differ, first 20:\n{}",
        failures.len(),
        failures[..failures.len().min(20)].join("\n")
    );
}
