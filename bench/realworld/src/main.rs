//! Parses fetched pages with Netweir's parser and with html5ever, for
//! bench/realworld/run.py.
//!
//! Reads one page per line on stdin, `PATH<TAB>CONTENT-TYPE` (`-` for
//! none), and writes JSON lines on stdout: `{"stage": ...}` before each
//! parser starts, so the driver can tell which one hung or crashed the
//! process, then the page's result. Its argument is allowlist.json.

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{ParseOpts, ns, parse_document};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use regex::Regex;
use serde_json::{Value, json};

// The same code Netweir's spider decodes a response with. Included rather
// than depended on, as netweir-core would build its whole TLS stack here.
#[allow(dead_code)]
#[path = "../../../crates/netweir-core/src/decode.rs"]
mod decode;

/// As long as a parse may take before it counts as a hang.
const BUDGET: Duration = Duration::from_secs(10);

thread_local! {
    static PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The parser under test, as the tree html5lib's tests print. This is the
/// one place that names it, so moving Netweir onto Parsec's crates
/// changes this function and nothing else.
mod netweir {
    use netweir_html::{Kind, Options, ParseError, SerializeOptions, parse_with};

    pub struct Parsed {
        pub tree: String,
        pub errors: usize,
        pub nodes: usize,
        pub lang: Option<String>,
    }

    /// `as_netweir` parses as Netweir does for a page (the default
    /// options and a time budget); otherwise as html5ever does, with
    /// declarative shadow roots off, and keeping the parse errors.
    pub fn parse(
        text: &str,
        budget: std::time::Duration,
        as_netweir: bool,
    ) -> Result<Parsed, String> {
        let options = Options {
            budget: Some(budget),
            errors: !as_netweir,
            declarative_shadow_roots: (!as_netweir).then_some(false),
            ..Options::default()
        };
        let doc = match parse_with(text, options) {
            Ok(doc) => doc,
            Err(ParseError::Timeout { parsed, .. }) => {
                return Err(format!("still parsing after {budget:?}, at byte {parsed}"));
            }
            Err(e) => return Err(e.to_string()),
        };
        let html = doc
            .children(doc.root())
            .find(|&c| matches!(doc.kind(c), Kind::Element { name: "html", .. }));
        let lang = html.and_then(|h| {
            doc.attrs(h)
                .find(|(_, name, _)| *name == "lang")
                .map(|(_, _, v)| v.to_string())
        });
        let tree = if as_netweir {
            String::new()
        } else {
            doc.serialize_with(
                doc.root(),
                &SerializeOptions {
                    html5test: true,
                    ..SerializeOptions::default()
                },
            )
        };
        Ok(Parsed {
            tree,
            errors: doc.errors().len(),
            nodes: doc.len(),
            lang,
        })
    }
}

/// html5ever's tree in the same format, written as Netweir's `dump` writes
/// it: attributes sorted as whole `name="value"` strings.
fn html5ever_tree(text: &str) -> String {
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts {
            scripting_enabled: false,
            ..TreeBuilderOpts::default()
        },
        ..ParseOpts::default()
    };
    let dom = parse_document(RcDom::default(), opts).one(text);
    let mut out = String::new();
    let mut stack: Vec<(Handle, usize, bool)> = Vec::new();
    push_children(&dom.document, 0, &mut stack);
    while let Some((node, depth, contents)) = stack.pop() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("| ");
        for _ in 0..depth {
            out.push_str("  ");
        }
        if contents {
            out.push_str("content");
            push_children(&node, depth + 1, &mut stack);
            continue;
        }
        match &node.data {
            NodeData::Document => {}
            NodeData::Doctype {
                name,
                public_id,
                system_id,
            } => {
                if public_id.is_empty() && system_id.is_empty() {
                    out.push_str(&format!("<!DOCTYPE {name}>"));
                } else {
                    out.push_str(&format!(
                        "<!DOCTYPE {name} \"{public_id}\" \"{system_id}\">"
                    ));
                }
            }
            NodeData::Text { contents } => out.push_str(&format!("\"{}\"", contents.borrow())),
            NodeData::Comment { contents } => out.push_str(&format!("<!-- {contents} -->")),
            NodeData::ProcessingInstruction { target, contents } => {
                out.push_str(&format!("<?{target} {contents}?>"))
            }
            NodeData::Element {
                name,
                attrs,
                template_contents,
                ..
            } => {
                let prefix = if name.ns == ns!(svg) {
                    "svg "
                } else if name.ns == ns!(mathml) {
                    "math "
                } else {
                    ""
                };
                out.push_str(&format!("<{prefix}{}>", name.local));
                let mut list: Vec<String> = attrs
                    .borrow()
                    .iter()
                    .map(|a| {
                        let n = &a.name;
                        let name = if n.ns == ns!(xlink) {
                            format!("xlink {}", n.local)
                        } else if n.ns == ns!(xml) {
                            format!("xml {}", n.local)
                        } else if n.ns == ns!(xmlns) && &*n.local != "xmlns" {
                            format!("xmlns {}", n.local)
                        } else {
                            n.local.to_string()
                        };
                        format!("{name}=\"{}\"", a.value)
                    })
                    .collect();
                list.sort();
                for a in list {
                    out.push_str("\n| ");
                    for _ in 0..=depth {
                        out.push_str("  ");
                    }
                    out.push_str(&a);
                }
                push_children(&node, depth + 1, &mut stack);
                // On top, so a template's `content` comes before any children.
                if let Some(t) = template_contents.borrow().as_ref() {
                    stack.push((t.clone(), depth + 1, true));
                }
            }
        }
    }
    out
}

fn push_children(node: &Handle, depth: usize, stack: &mut Vec<(Handle, usize, bool)>) {
    stack.extend(
        node.children
            .borrow()
            .iter()
            .rev()
            .map(|c| (c.clone(), depth, false)),
    );
}

/// `f`, with a panic turned into its message and where it was raised.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|_| {
        PANIC
            .with(|p| p.borrow_mut().take())
            .unwrap_or_else(|| "panicked".to_string())
    })
}

/// The first line the trees differ on, with a few lines around it.
fn first_difference(ours: &str, theirs: &str) -> Value {
    let a: Vec<&str> = ours.lines().collect();
    let b: Vec<&str> = theirs.lines().collect();
    let at = a
        .iter()
        .zip(&b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    let around = |lines: &[&str]| lines[at.saturating_sub(3)..(at + 4).min(lines.len())].join("\n");
    json!({
        "line": at + 1,
        "netweir": around(&a),
        "html5ever": around(&b),
    })
}

/// A known difference between the parsers, from allowlist.json: a
/// rewrite of each dump into a form both share, so the rest of the trees
/// are still compared.
struct Rule {
    name: String,
    netweir: Option<(Regex, String)>,
    html5ever: Option<(Regex, String)>,
}

fn rules(path: &str) -> Vec<Rule> {
    let text = std::fs::read_to_string(path).expect("allowlist readable");
    let list: Vec<Value> = serde_json::from_str(&text).expect("allowlist is a JSON list");
    let side = |v: &Value| {
        v.as_array().map(|r| {
            let pattern = format!("(?m){}", r[0].as_str().expect("pattern"));
            (
                Regex::new(&pattern).expect("valid pattern"),
                r[1].as_str().expect("replacement").to_string(),
            )
        })
    };
    list.iter()
        .map(|r| Rule {
            name: r["name"].as_str().expect("name").to_string(),
            netweir: side(&r["netweir"]),
            html5ever: side(&r["html5ever"]),
        })
        .collect()
}

/// Both dumps with every rule applied, and the names of those that
/// changed either.
fn normalized(mut ours: String, mut theirs: String, rules: &[Rule]) -> (String, String, Vec<&str>) {
    let mut used = Vec::new();
    for rule in rules {
        let mut changed = false;
        for (dump, side) in [(&mut ours, &rule.netweir), (&mut theirs, &rule.html5ever)] {
            if let Some((re, with)) = side
                && let std::borrow::Cow::Owned(new) = re.replace_all(dump, with.as_str())
            {
                *dump = new;
                changed = true;
            }
        }
        if changed {
            used.push(rule.name.as_str());
        }
    }
    (ours, theirs, used)
}

fn page(
    path: &str,
    content_type: Option<&str>,
    rules: &[Rule],
    out: &mut impl Write,
) -> std::io::Result<Value> {
    let body = std::fs::read(path)?;
    let encoding = decode::encoding_name(&body, content_type);
    let text = decode::decode(&body, content_type);
    let mut stage = |name: &str| -> std::io::Result<()> {
        writeln!(out, "{}", json!({ "stage": name }))?;
        out.flush()
    };

    stage("netweir")?;
    let start = Instant::now();
    let as_netweir = guarded(|| netweir::parse(&text, BUDGET, true));
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let mut result = json!({
        "bytes": body.len(),
        "encoding": encoding,
        "netweir_ms": ms,
    });
    let failure = match &as_netweir {
        Err(panic) => Some(json!({ "kind": "panic", "parser": "netweir", "detail": panic })),
        Ok(Err(timeout)) => Some(json!({ "kind": "hang", "parser": "netweir", "detail": timeout })),
        Ok(Ok(parsed)) => {
            result["nodes"] = json!(parsed.nodes);
            result["lang"] = json!(parsed.lang);
            None
        }
    };
    if let Some(f) = failure {
        result["failure"] = f;
        return Ok(result);
    }

    stage("netweir-tree")?;
    let ours = match guarded(|| netweir::parse(&text, BUDGET * 3, false)) {
        Ok(Ok(parsed)) => parsed,
        Ok(Err(e)) => {
            result["failure"] = json!({ "kind": "hang", "parser": "netweir", "detail": format!("with errors on: {e}") });
            return Ok(result);
        }
        Err(panic) => {
            result["failure"] = json!({ "kind": "panic", "parser": "netweir", "detail": format!("with errors on: {panic}") });
            return Ok(result);
        }
    };
    result["errors"] = json!(ours.errors);

    stage("html5ever")?;
    let start = Instant::now();
    let theirs = guarded(|| html5ever_tree(&text));
    result["html5ever_ms"] = json!(start.elapsed().as_secs_f64() * 1000.0);
    match theirs {
        Err(panic) => result["html5ever_panic"] = json!(panic),
        Ok(theirs) if theirs != ours.tree => {
            let (a, b, used) = normalized(ours.tree, theirs, rules);
            if a == b {
                result["allowed"] = json!(used);
            } else {
                result["difference"] = first_difference(&a, &b);
            }
        }
        Ok(_) => {}
    }
    Ok(result)
}

fn main() -> std::io::Result<()> {
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let at = info
            .location()
            .map(|l| format!(" at {l}"))
            .unwrap_or_default();
        PANIC.with(|p| *p.borrow_mut() = Some(format!("{msg}{at}")));
    }));
    let allowlist = std::env::args()
        .nth(1)
        .expect("usage: bench-realworld ALLOWLIST.json");
    let rules = rules(&allowlist);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let (path, content_type) = line.split_once('\t').unwrap_or((&line, "-"));
        let content_type = (content_type != "-").then_some(content_type);
        let result = match page(path, content_type, &rules, &mut out) {
            Ok(r) => r,
            Err(e) => json!({ "failure": { "kind": "unreadable", "detail": e.to_string() } }),
        };
        writeln!(out, "{}", json!({ "result": result }))?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both dumps print the same tree the same way, on the parts of the
    /// format each writes by its own code: a doctype's ids, a template's
    /// contents, foreign elements and their namespaced attributes,
    /// comments, and attribute order.
    #[test]
    fn the_dumps_agree_on_the_format() {
        for html in [
            r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"><p>x"#,
            "<!doctype html><template><b>a</b><template>c</template></template>after",
            r##"<svg viewBox="0 0 1 1"><a xlink:href="#q" xml:lang=en xmlns:xlink="http://www.w3.org/1999/xlink"><foreignObject><p>x</p></foreignObject></a></svg><math><mi>y</mi></math>"##,
            "<!-- one --><p z=1 a=2 aa=3 a-b=4>text<!-- two --></p><table><td>cell</table>",
        ] {
            let ours = netweir::parse(html, BUDGET, false).unwrap().tree;
            assert_eq!(html5ever_tree(html), ours, "{html}");
        }
    }

    /// Each allowlist rule turns the difference it names into none, and
    /// leaves a different difference on the same page alone.
    // One case per rule in allowlist.json, of which there is one so far.
    #[allow(clippy::single_element_loop)]
    #[test]
    fn the_allowlist_rules_hide_only_their_difference() {
        let rules = rules(concat!(env!("CARGO_MANIFEST_DIR"), "/allowlist.json"));
        for (html, rule) in [(
            r#"<p><?start name="x"><?end?><?php
  }</style?></p>"#,
            "processing-instructions",
        )] {
            let ours = netweir::parse(html, BUDGET, false).unwrap().tree;
            let theirs = html5ever_tree(html);
            assert_ne!(ours, theirs, "{rule} is no longer a difference");
            let (a, b, used) = normalized(ours.clone(), theirs.clone(), &rules);
            assert_eq!(a, b, "{rule}");
            assert_eq!(used, [rule]);
            let (a, b, _) = normalized(ours + "\n| <x>", theirs, &rules);
            assert_ne!(a, b);
        }
    }
}
