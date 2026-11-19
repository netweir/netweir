//! Every input the conformance suites have, for tests that check a
//! property of the parse rather than the tree each case expects.

#![allow(dead_code)]

use netweir_html::Ns;

/// A tree-construction case's input: its `#data`, and its context
/// element when it's a fragment case; and the errors it lists.
pub struct TreeInput {
    pub file: String,
    pub data: String,
    pub fragment: Option<(Ns, String)>,
    /// From `#script-on` or `#script-off`, when it has one.
    pub scripting: Option<bool>,
    /// The `#errors` lines, in whichever of the formats the files use.
    pub errors: Vec<String>,
    /// The `#new-errors` lines: `(line:column) code`.
    pub new_errors: Vec<String>,
}

const HEADERS: &[&str] = &[
    "#data",
    "#errors",
    "#new-errors",
    "#document-fragment",
    "#document",
    "#script-on",
    "#script-off",
    "#description",
];

pub fn tree_inputs() -> Vec<TreeInput> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/tree-construction");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "dat"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        // `#data` runs to the `#errors` line after it; the sections up
        // to `#document` follow, each a header line and then its lines.
        for case in text.split("#data\n").skip(1) {
            let Some((data, rest)) = case
                .strip_prefix("#errors\n")
                .map(|rest| ("", rest))
                .or_else(|| case.split_once("\n#errors\n"))
            else {
                continue;
            };
            let mut section = "#errors";
            let (mut errors, mut new_errors) = (Vec::new(), Vec::new());
            let (mut fragment, mut scripting) = (None, None);
            for line in rest.lines() {
                if HEADERS.contains(&line) {
                    section = line;
                    match line {
                        "#script-on" => scripting = Some(true),
                        "#script-off" => scripting = Some(false),
                        "#document" => break,
                        _ => {}
                    }
                    continue;
                }
                match section {
                    "#errors" if !line.trim().is_empty() => errors.push(line.to_string()),
                    "#new-errors" if !line.trim().is_empty() => new_errors.push(line.to_string()),
                    "#document-fragment" if fragment.is_none() => {
                        fragment = Some(match line.trim().split_once(' ') {
                            Some(("svg", n)) => (Ns::Svg, n.to_string()),
                            Some(("math", n)) => (Ns::MathMl, n.to_string()),
                            _ => (Ns::Html, line.trim().to_string()),
                        });
                    }
                    _ => {}
                }
            }
            out.push(TreeInput {
                file: file.clone(),
                data: data.to_string(),
                fragment,
                scripting,
                errors,
                new_errors,
            });
        }
    }
    out
}
/// Every tokenizer test's input that a Rust string can hold.
pub fn tokenizer_inputs() -> Vec<String> {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/html5lib-tests/tokenizer"
    );
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "test"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let suite: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let Some(tests) = suite.get("tests").and_then(|t| t.as_array()) else {
            continue;
        };
        for test in tests {
            let input = test["input"].as_str().unwrap();
            if test.get("doubleEscaped").and_then(|d| d.as_bool()) == Some(true) {
                if let Some(s) = unescape(input) {
                    out.push(s);
                }
            } else {
                out.push(input.to_string());
            }
        }
    }
    out
}

/// A doubleEscaped test's `\uXXXX` escapes, or None for a lone surrogate.
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
