//! The tree-construction tests from web-platform-tests (formerly
//! html5lib-tests), in their `.dat` format.

use netweir_html::{AttrNs, Document, Kind, NodeId, Ns, parse, parse_fragment};

mod summary;

struct Case {
    file: String,
    data: String,
    fragment: Option<String>,
    scripting: Option<bool>,
    document: String,
}

fn cases() -> Vec<Case> {
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
        // A test is a run of `#section` headers and their lines; the
        // document section ends at a blank line.
        let mut tests: Vec<Vec<(String, Vec<&str>)>> = Vec::new();
        let mut between = true;
        let lines: Vec<&str> = text.split('\n').collect();
        for (i, &line) in lines.iter().enumerate() {
            let in_document = !between
                && tests
                    .last()
                    .and_then(|t| t.last())
                    .is_some_and(|(n, _)| n == "document");
            let next_starts_test = lines.get(i + 1).is_none_or(|l| l.starts_with('#'));
            if in_document && line.is_empty() && next_starts_test {
                between = true;
                continue;
            }
            let header = line.starts_with('#')
                && matches!(
                    line,
                    "#data"
                        | "#errors"
                        | "#new-errors"
                        | "#document-fragment"
                        | "#document"
                        | "#script-on"
                        | "#script-off"
                        | "#description"
                )
                && !in_document;
            if header {
                if between {
                    tests.push(Vec::new());
                    between = false;
                }
                tests
                    .last_mut()
                    .unwrap()
                    .push((line[1..].to_string(), Vec::new()));
            } else if let Some((_, lines)) = tests.last_mut().and_then(|t| t.last_mut()) {
                lines.push(line);
            }
        }
        for fields in tests {
            let get = |name: &str| {
                fields
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.join("\n"))
            };
            let Some(document) = get("document") else {
                continue;
            };
            out.push(Case {
                file: file.clone(),
                data: get("data").unwrap_or_default(),
                fragment: get("document-fragment").map(|f| f.trim().to_string()),
                scripting: if fields.iter().any(|(n, _)| n == "script-on") {
                    Some(true)
                } else if fields.iter().any(|(n, _)| n == "script-off") {
                    Some(false)
                } else {
                    None
                },
                document: document.trim_end_matches('\n').to_string(),
            });
        }
    }
    out
}

fn serialize(doc: &Document, node: NodeId, depth: usize, out: &mut Vec<String>) {
    for child in doc.children(node) {
        let pad = "  ".repeat(depth);
        match doc.kind(child) {
            Kind::Doctype {
                name,
                public_id,
                system_id,
            } => {
                if public_id.is_empty() && system_id.is_empty() {
                    out.push(format!("| {pad}<!DOCTYPE {name}>"));
                } else {
                    out.push(format!(
                        "| {pad}<!DOCTYPE {name} \"{public_id}\" \"{system_id}\">"
                    ));
                }
            }
            Kind::Element { ns, name } => {
                let prefix = match ns {
                    Ns::Html => "",
                    Ns::Svg => "svg ",
                    Ns::MathMl => "math ",
                    Ns::Xml => "xml ",
                };
                out.push(format!("| {pad}<{prefix}{name}>"));
                let mut attrs: Vec<String> = doc
                    .attrs(child)
                    .zip(doc.attrs_written(child))
                    .map(|((ns, local, value), (_, written, _))| {
                        let name = match ns {
                            AttrNs::None => written.to_string(),
                            AttrNs::XLink => format!("xlink {local}"),
                            AttrNs::Xml => format!("xml {local}"),
                            AttrNs::Xmlns if local == "xmlns" => "xmlns".to_string(),
                            AttrNs::Xmlns => format!("xmlns {local}"),
                            AttrNs::Host(_) => local.to_string(),
                        };
                        format!("{name}=\"{value}\"")
                    })
                    .collect();
                attrs.sort();
                for a in attrs {
                    out.push(format!("| {pad}  {a}"));
                }
                if let Some(contents) = doc.template_contents(child) {
                    out.push(format!("| {pad}  content"));
                    serialize(doc, contents, depth + 2, out);
                }
            }
            Kind::Text(t) => out.push(format!("| {pad}\"{t}\"")),
            Kind::Comment(c) => out.push(format!("| {pad}<!-- {c} -->")),
            Kind::ProcessingInstruction { target, data } => {
                out.push(format!("| {pad}<?{target} {data}?>"))
            }
            Kind::Document | Kind::Fragment | Kind::Cdata(_) => {}
        }
        if !matches!(doc.kind(child), Kind::Element { .. })
            || doc.template_contents(child).is_none()
        {
            serialize(doc, child, depth + 1, out);
        }
    }
}

fn run(case: &Case, scripting: bool) -> String {
    let doc = match &case.fragment {
        Some(context) => {
            let (ns, name) = match context.split_once(' ') {
                Some(("svg", n)) => (Ns::Svg, n),
                Some(("math", n)) => (Ns::MathMl, n),
                _ => (Ns::Html, context.as_str()),
            };
            parse_fragment(&case.data, ns, name, scripting)
        }
        None => parse(&case.data, scripting),
    };
    let mut out = Vec::new();
    serialize(&doc, doc.root(), 0, &mut out);
    out.join("\n")
}

#[test]
fn tree_construction_tests() {
    let only = std::env::var("TREE_TEST").ok();
    let (mut passed, mut skipped, mut failed) = (0, 0, Vec::new());
    for case in cases() {
        if only.as_deref().is_some_and(|o| !case.file.contains(o)) {
            continue;
        }
        // These run script during the parse (document.write, DOM calls),
        // which needs a JavaScript engine; a parser without one can't.
        if case.file.starts_with("scripted_") && case.scripting == Some(true) {
            skipped += 1;
            continue;
        }
        let modes: &[bool] = match case.scripting {
            Some(s) => {
                if s {
                    &[true]
                } else {
                    &[false]
                }
            }
            None => &[false, true],
        };
        for &scripting in modes {
            let got = run(&case, scripting);
            if got == case.document {
                passed += 1;
            } else {
                failed.push(format!(
                    "{} (scripting {scripting}{})\n--- input\n{}\n--- expected\n{}\n--- got\n{}\n",
                    case.file,
                    case.fragment
                        .as_deref()
                        .map(|f| format!(", fragment {f}"))
                        .unwrap_or_default(),
                    case.data,
                    case.document,
                    got
                ));
            }
        }
    }
    eprintln!(
        "tree construction: {passed} passed, {} failed, {skipped} skipped (need JavaScript)",
        failed.len()
    );
    if only.is_none() {
        summary::write(&summary::Counts {
            suite: "html5lib-tree-construction",
            passed,
            failed: failed.len(),
            skipped,
            skipped_because: "they run script during the parse, which needs a JavaScript engine",
            data: std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/tree-construction"
            )),
        });
    }
    if std::env::var("TREE_BRIEF").is_ok() {
        for f in &failed {
            let mut lines = f.lines();
            eprintln!(
                "{} :: {}",
                lines.next().unwrap(),
                lines.nth(1).unwrap_or("")
            );
        }
    }
    if !failed.is_empty() {
        let shown: Vec<_> = failed.iter().take(15).cloned().collect();
        panic!(
            "{} failures; first {}:\n{}",
            failed.len(),
            shown.len(),
            shown.join("\n")
        );
    }
}
