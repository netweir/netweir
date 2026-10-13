//! A node as Markdown, or as plain text that keeps the page's blocks:
//! paragraphs, headings, list items and table rows each on lines of their
//! own.
//!
//! One walk does both. Text is gathered a block at a time, with its
//! whitespace collapsed as a browser collapses it, and written out when
//! the block ends, behind whatever the blocks around it put at the start
//! of each line (`> ` for a quotation, a list item's marker and then its
//! indent).

use std::collections::HashSet;

use url::Url;

use crate::{Node, NodeId, NodeKind};

/// `node` as Markdown: CommonMark, with GitHub's tables and
/// strikethrough. Links and images are made absolute against `base`.
pub fn markdown(node: Node<'_>, base: Option<&Url>) -> String {
    render(node, Style::Markdown, base, &HashSet::new(), false)
}

/// `node`'s readable text with its blocks kept apart: a blank line between
/// paragraphs and headings, a line each for list items and table rows
/// (cells separated by tabs), whitespace inside a block collapsed to single
/// spaces, and preformatted text as it is.
pub fn clean_text(node: Node<'_>) -> String {
    render(node, Style::Text, None, &HashSet::new(), false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Markdown,
    Text,
}

/// `node` written in `style`, leaving out the elements in `skip`.
pub(crate) fn render(
    node: Node<'_>,
    style: Style,
    base: Option<&Url>,
    skip: &HashSet<NodeId>,
    noscript: bool,
) -> String {
    let mut w = Writer {
        style,
        base,
        skip,
        noscript,
        fragile: None,
        last_list: None,
        scanned: HashSet::new(),
        nested: HashSet::new(),
        last: String::new(),
        out: String::new(),
        frames: Vec::new(),
        lists: Vec::new(),
        line: String::new(),
        space: false,
        lead: false,
        sep: Sep::Blank,
        inline: 0,
        cell: 0,
        heading: 0,
        depth: [0; 4],
    };
    w.node(node);
    w.flush();
    w.out
}

/// Elements whose contents a reader never sees as text: code, embedded
/// media and form controls. `<head>` is here too, so a whole document
/// starts at its body.
const UNSEEN: [&str; 19] = [
    "script", "style", "noscript", "template", "head", "svg", "math", "canvas", "iframe", "object",
    "embed", "button", "select", "input", "textarea", "option", "datalist", "audio", "video",
];

/// Classes of the links docs and wikis put in headings for themselves
/// (Sphinx's `¶`, MediaWiki's `[edit]`), which aren't part of the text,
/// and of labels written for screen readers only ("Opens in a new
/// window"), which a page shows nobody.
const FURNITURE: [&str; 8] = [
    "headerlink",
    "mw-editsection",
    "anchorjs-link",
    "mw-jump-link",
    "mw-pt-languages",
    "visually-hidden",
    "sr-only",
    "screen-reader-text",
];

/// Class and id words of widgets: what a page hides until it pops up,
/// rather than content it folds away.
const WIDGET_WORDS: [&str; 15] = [
    "carousel",
    "consent",
    "cookie",
    "dialog",
    "drawer",
    "dropdown",
    "menu",
    "modal",
    "newsletter",
    "notify",
    "overlay",
    "popover",
    "popup",
    "toast",
    "tooltip",
];

/// Whether `node` is an element nobody reading the page sees: one of
/// [`UNSEEN`] (a `<button>` only when it doesn't open a panel), a closed
/// `<dialog>`, a heading permalink or edit link, or a screen-reader
/// label ([`FURNITURE`]). `<noscript>` is unseen unless `noscript` is
/// true.
///
/// What's hidden until a click is still the page's content: an accordion
/// panel, a tab, a FAQ's answer. So hiding (the `hidden` attribute,
/// `aria-hidden="true"`, an inline `display: none`, a hiding class) only
/// makes an element unseen when it's a widget (a form, a dialog, a
/// modal, a toast, a menu: see [`WIDGET_WORDS`]) or holds no block of its
/// own (an icon, a template's leftovers, a "copied!" label).
///
/// Known limit: a stylesheet hiding something by a class of its own
/// isn't known about.
pub(crate) fn unseen(node: Node<'_>, noscript: bool) -> bool {
    let Some(tag) = node.tag() else {
        return false;
    };
    if tag == "button" {
        return node.attr("aria-expanded").is_none() && node.attr("aria-controls").is_none();
    }
    if (UNSEEN.contains(&tag) && !(noscript && tag == "noscript"))
        || (tag == "dialog" && node.attr("open").is_none())
        || node
            .attr("class")
            .is_some_and(|c| c.split_whitespace().any(|w| FURNITURE.contains(&w)))
    {
        return true;
    }
    let hidden = node.attr("hidden").is_some()
        || node
            .attr("aria-hidden")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        || node.attr("style").is_some_and(hidden_by_style)
        || node.attr("class").is_some_and(hidden_by_class);
    hidden
        && (widget(node, tag)
            || !holds_block(node)
            || mostly_links(node)
            || (aria_hidden(node) && !panel(node)))
}

fn aria_hidden(node: Node<'_>) -> bool {
    node.attr("aria-hidden")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// Whether a hidden element is a panel some control opens: it has an id
/// for the control to name, a tab panel's or region's role, or a class
/// that says so. `aria-hidden` on anything else marks a copy kept for
/// looks (a caption or byline repeated over an image), which a reader
/// has already read.
fn panel(node: Node<'_>) -> bool {
    node.attr("id").is_some()
        || node.attr("role").is_some_and(|r| {
            ["tabpanel", "region"]
                .iter()
                .any(|w| r.trim().eq_ignore_ascii_case(w))
        })
        || node
            .attr("class")
            .unwrap_or_default()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| {
                [
                    "accordion",
                    "panel",
                    "collapse",
                    "collapsible",
                    "tab",
                    "answer",
                    "expand",
                    "more",
                    "faq",
                ]
                .iter()
                .any(|x| w.eq_ignore_ascii_case(x))
            })
}

/// Whether most of an element's text is links: a hidden menu.
fn mostly_links(node: Node<'_>) -> bool {
    let size = |n: Node<'_>| {
        n.text()
            .bytes()
            .filter(|b| !b.is_ascii_whitespace())
            .count()
    };
    let linked: usize = node
        .descendants()
        .filter(|d| d.tag() == Some("a"))
        .map(size)
        .sum();
    linked * 2 >= size(node)
}

/// Whether a hidden element is a widget rather than folded content.
fn widget(node: Node<'_>, tag: &str) -> bool {
    tag == "form"
        || node.attr("aria-modal").is_some()
        || node.attr("role").is_some_and(|r| {
            [
                "dialog",
                "alertdialog",
                "alert",
                "status",
                "menu",
                "tooltip",
            ]
            .iter()
            .any(|w| r.trim().eq_ignore_ascii_case(w))
        })
        || [
            node.attr("class"),
            node.attr("id"),
            Some(tag).filter(|t| t.contains('-')),
        ]
        .into_iter()
        .flatten()
        .flat_map(|v| v.split(|c: char| !c.is_ascii_alphanumeric()))
        .any(|w| WIDGET_WORDS.iter().any(|x| w.eq_ignore_ascii_case(x)))
}

/// Whether `node` has a block of text in it: a paragraph, a list, a
/// table, a heading.
fn holds_block(node: Node<'_>) -> bool {
    node.descendants().any(|d| {
        matches!(
            d.tag(),
            Some(
                "p" | "ul"
                    | "ol"
                    | "dl"
                    | "table"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "blockquote"
                    | "pre"
            )
        )
    })
}

/// Whether a class hides its element: `hidden`, `is-hidden`, `u-hidden`
/// or `d-none`. Not when another class shows it again at some screen
/// size (`hidden md:block`, `d-none d-lg-flex`), which is how utility CSS
/// writes "only on wide screens".
fn hidden_by_class(class: &str) -> bool {
    let mut hidden = false;
    for c in class.split_whitespace() {
        if c.contains(':') || (c.starts_with("d-") && c != "d-none") {
            return false;
        }
        hidden |= matches!(c, "hidden" | "is-hidden" | "u-hidden" | "d-none");
    }
    hidden
}

fn hidden_by_style(style: &str) -> bool {
    let flat: String = style
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    flat.contains("display:none") || flat.contains("visibility:hidden")
}

/// Elements that start a new block. Anything else (including tags
/// nobody has heard of) runs inline, as a browser lays unknown elements
/// out.
const BLOCKS: [&str; 34] = [
    "address",
    "article",
    "aside",
    "body",
    "caption",
    "center",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "hgroup",
    "html",
    "legend",
    "main",
    "nav",
    "p",
    "search",
    "section",
    "summary",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
];

pub(crate) fn is_block(tag: &str) -> bool {
    BLOCKS.contains(&tag)
        || matches!(
            tag,
            "h1" | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "ul"
                | "ol"
                | "li"
                | "menu"
                | "blockquote"
                | "pre"
                | "table"
                | "hr"
        )
}

/// The image an `<img>` shows: its `src`, or where lazy-loading scripts
/// keep it (`data-src` and the like, or the first of a `srcset`).
///
/// Known limit: `data:` URLs are left out; inlined into Markdown they'd
/// be most of it.
pub(crate) fn image_source<'a>(img: Node<'a>) -> Option<&'a str> {
    let usable = |s: &&str| !s.is_empty() && !s.starts_with("data:");
    for name in ["src", "data-src", "data-lazy-src", "data-original"] {
        if let Some(src) = img.attr(name).map(str::trim).filter(usable) {
            return Some(src);
        }
    }
    img.attr("srcset")
        .or_else(|| img.attr("data-srcset"))
        .and_then(|s| s.split(',').next())
        .and_then(|c| c.split_whitespace().next())
        .filter(usable)
}

/// Lists and quotations nested deeper than this are written at this
/// depth: each level adds to every line inside it, so a page of a few
/// thousand nested lists would otherwise make Markdown thousands of times
/// its size.
const MAX_DEPTH: usize = 10;

/// What a block owes the lines it writes: a quotation's `> `, or a list
/// item's marker on its first line and an indent the width of the marker
/// on the rest.
struct Frame {
    first: String,
    rest: String,
    used: bool,
}

/// What goes between the last block written and the next one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sep {
    Line,
    Blank,
}

struct List {
    ordered: bool,
    next: i64,
    items: u32,
    /// Written with the other marker (`*`, `)`), because it follows a
    /// list of its kind that Markdown would otherwise run it into.
    alt: bool,
}

/// The largest number an ordered list's marker can have: nine digits.
const MAX_NUMBER: i64 = 999_999_999;

/// Emphasis just written whose closing marker only works if what comes
/// next isn't a letter or digit (`**"q"**` before `word` doesn't close);
/// if it is, the emphasis is rewritten as HTML.
struct Fragile {
    start: usize,
    end: usize,
    html: String,
}

const EMPHASIS: usize = 0;
const STRONG: usize = 1;
const STRIKE: usize = 2;
const LINK: usize = 3;

struct Writer<'s> {
    style: Style,
    base: Option<&'s Url>,
    skip: &'s HashSet<NodeId>,
    out: String,
    frames: Vec<Frame>,
    lists: Vec<List>,
    /// The block being gathered, already escaped.
    line: String,
    /// Whitespace has been seen since the last text: a space is due before
    /// the next, unless the block is still empty.
    space: bool,
    /// Whitespace was seen while `line` was empty, which an inline element
    /// gathered on its own needs to know to keep the space before it.
    lead: bool,
    sep: Sep,
    /// Gathering an inline element's contents on their own, or a table
    /// cell's or a heading's: blocks inside run on as text.
    inline: u32,
    cell: u32,
    heading: u32,
    /// How deep inside emphasis, strong, strikethrough and links we are;
    /// nested ones don't repeat their markers.
    depth: [u32; 4],
    noscript: bool,
    fragile: Option<Fragile>,
    /// The last list written: whether it was ordered, its `alt`, and the
    /// length of `out` and `frames` when it ended.
    last_list: Option<(bool, bool, usize, usize)>,
    /// The last paragraph written, so a hidden copy and its visible twin
    /// side by side come out once.
    last: String,
    /// Tables already looked inside, and those of them holding a table.
    scanned: HashSet<NodeId>,
    nested: HashSet<NodeId>,
}

impl Writer<'_> {
    fn md(&self) -> bool {
        self.style == Style::Markdown
    }

    fn inline_only(&self) -> bool {
        self.inline > 0 || self.cell > 0 || self.heading > 0
    }

    fn node(&mut self, n: Node<'_>) {
        stacker::maybe_grow(64 * 1024, 1024 * 1024, || match n.kind() {
            NodeKind::Text => self.text(n.data().unwrap_or_default()),
            NodeKind::Element => self.element(n),
            NodeKind::Document => self.children(n),
            _ => {}
        })
    }

    fn children(&mut self, n: Node<'_>) {
        for child in n.children() {
            self.node(child);
        }
    }

    // --- inline text ----------------------------------------------------

    /// Inline text, its whitespace collapsed. A non-breaking space is
    /// kept in Markdown, where it still keeps two words together, and is
    /// an ordinary space in text; an ideographic space (U+3000) is a
    /// character of CJK text, and kept in both.
    fn text(&mut self, text: &str) {
        let md = self.md();
        let breaks = |c: char| {
            c.is_whitespace() && c != '\u{3000}' && !(md && matches!(c, '\u{a0}' | '\u{202f}'))
        };
        for (i, word) in text.split(breaks).enumerate() {
            if i > 0 {
                self.gap();
            }
            if let Some(first) = word.chars().next() {
                self.open();
                self.settle(first);
                if md {
                    escape(word, &mut self.line);
                } else {
                    self.line.push_str(word);
                }
            }
        }
    }

    fn gap(&mut self) {
        if self.line.is_empty() {
            self.lead = true;
        }
        self.space = true;
    }

    /// Puts down the space that's due, ready for more text.
    fn open(&mut self) {
        if self.space && !self.line.is_empty() && !self.line.ends_with('\n') {
            self.line.push(' ');
            self.fragile = None;
        }
        self.space = false;
    }

    fn push(&mut self, s: &str) {
        self.open();
        if let Some(first) = s.chars().next() {
            self.settle(first);
        }
        self.line.push_str(s);
    }

    /// Rewrites the emphasis just written as HTML if `next`, coming
    /// straight after it, would stop its closing marker closing.
    fn settle(&mut self, next: char) {
        if let Some(f) = self.fragile.take()
            && self.line.len() == f.end
            && !punctuation(next)
            && !next.is_whitespace()
        {
            self.line.truncate(f.start);
            self.line.push_str(&f.html);
        }
    }

    /// The line break `<br>` makes: a hard break in Markdown, a newline in
    /// text. Two in a row end the paragraph, as old pages write them.
    fn hard_break(&mut self) {
        if self.cell > 0 {
            if self.md() && !self.line.is_empty() {
                self.push("<br>");
            } else {
                self.gap();
            }
            return;
        }
        if self.heading > 0 {
            self.gap();
            return;
        }
        if self.line.is_empty() {
            return;
        }
        let brk = self.line_break();
        if self.line.ends_with(brk) && self.inline == 0 {
            self.line.truncate(self.line.len() - brk.len());
            self.flush();
            return;
        }
        if !self.line.ends_with(brk) {
            self.line.push_str(brk);
        }
        self.space = false;
    }

    fn line_break(&self) -> &'static str {
        if self.md() { "\\\n" } else { "\n" }
    }

    /// Gathers what `f` writes on its own: the text, whether whitespace
    /// came before it, and whether whitespace came after.
    fn capture(&mut self, f: impl FnOnce(&mut Self)) -> (String, bool, bool) {
        let line = std::mem::take(&mut self.line);
        let space = std::mem::replace(&mut self.space, false);
        let lead = std::mem::replace(&mut self.lead, false);
        let fragile = self.fragile.take();
        self.inline += 1;
        f(self);
        self.inline -= 1;
        self.fragile = fragile;
        let inner = std::mem::replace(&mut self.line, line);
        let trail = std::mem::replace(&mut self.space, space);
        let inner_lead = std::mem::replace(&mut self.lead, lead);
        (inner, inner_lead, trail)
    }

    /// Puts a link's gathered text back, wrapped by `wrap`, with the
    /// spaces around it outside the brackets. A `!` straight before it is
    /// escaped, or the link would be an image.
    fn splice(&mut self, inner: &str, lead: bool, trail: bool, wrap: impl FnOnce(&str) -> String) {
        if lead {
            self.gap();
        }
        let text = trim_breaks(inner, self.line_break());
        if !text.is_empty() {
            self.open();
            self.settle('[');
            if self.line.ends_with('!') {
                self.line.pop();
                self.line.push_str("\\!");
            }
            self.line.push_str(&wrap(text));
        }
        if trail {
            self.gap();
        }
    }

    // --- blocks ---------------------------------------------------------

    /// Ends the block being gathered and writes it.
    fn flush(&mut self) {
        if self.inline_only() {
            self.gap();
            return;
        }
        let line = std::mem::take(&mut self.line);
        self.space = false;
        self.lead = false;
        self.fragile = None;
        let text = trim_breaks(&line, self.line_break());
        // A paragraph the same as the one just written is a hidden copy
        // beside its visible twin.
        if !text.trim().is_empty() && !(text.len() >= 20 && text == self.last) {
            let fix = self.md();
            self.write(text, fix);
            self.last = text.to_string();
        }
    }

    /// Writes a finished block, each line behind the prefixes it owes.
    /// `fix` escapes what would start a Markdown block at a line's start.
    fn write(&mut self, block: &str, fix: bool) {
        self.last.clear();
        if !self.out.is_empty() {
            self.out.push('\n');
            if self.sep == Sep::Blank {
                let blank: String = self
                    .frames
                    .iter()
                    .filter(|f| f.used)
                    .map(|f| f.rest.as_str())
                    .collect();
                self.out.push_str(blank.trim_end());
                self.out.push('\n');
            }
        }
        for (i, line) in block.split('\n').enumerate() {
            if i > 0 {
                self.out.push('\n');
            }
            let start = self.out.len();
            for f in &mut self.frames {
                self.out.push_str(if f.used { &f.rest } else { &f.first });
                f.used = true;
            }
            if line.is_empty() {
                let kept = self.out[start..].trim_end().len();
                self.out.truncate(start + kept);
            } else if fix {
                fix_start(line, &mut self.out);
            } else {
                self.out.push_str(line);
            }
        }
        self.sep = Sep::Blank;
    }

    fn element(&mut self, n: Node<'_>) {
        if unseen(n, self.noscript) || self.skip.contains(&n.id()) {
            return;
        }
        let tag = n.tag().unwrap_or_default();
        match tag {
            "br" => self.hard_break(),
            "hr" => {
                self.flush();
                if self.md() && !self.inline_only() {
                    self.write("---", false);
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.heading_block(n, usize::from(tag.as_bytes()[1] - b'0'))
            }
            "ul" | "ol" | "menu" | "dir" => self.list(n, tag == "ol"),
            "li" => self.item(n),
            "blockquote" => self.quote(n),
            "pre" => self.code_block(n),
            "code" | "kbd" | "samp" | "tt" => self.code(n),
            "em" | "i" | "cite" | "dfn" | "var" => self.wrap(n, "*", EMPHASIS),
            "strong" | "b" => self.wrap(n, "**", STRONG),
            "del" | "s" | "strike" => self.wrap(n, "~~", STRIKE),
            "a" => self.link(n),
            "img" => self.image(n),
            "table" => self.table(n),
            _ if BLOCKS.contains(&tag) => {
                self.flush();
                self.children(n);
                self.flush();
            }
            _ => self.children(n),
        }
    }

    fn heading_block(&mut self, n: Node<'_>, level: usize) {
        if self.inline_only() {
            self.gap();
            self.children(n);
            self.gap();
            return;
        }
        self.flush();
        self.heading += 1;
        self.children(n);
        self.heading -= 1;
        let line = std::mem::take(&mut self.line);
        self.space = false;
        self.lead = false;
        let text = line.trim();
        if text.is_empty() {
            return;
        }
        if self.md() {
            // A heading's trailing #s would be read as a closing sequence.
            let text = match text.trim_end_matches('#').len() {
                end if end < text.len() => format!("{}\\{}", &text[..end], &text[end..]),
                _ => text.to_string(),
            };
            self.write(&format!("{} {text}", "#".repeat(level)), false);
        } else {
            self.write(text, false);
        }
    }

    fn list(&mut self, n: Node<'_>, ordered: bool) {
        if self.inline_only() {
            self.children(n);
            return;
        }
        self.flush();
        // A list inside an item follows the item's text on the next line.
        if self.lists.last().is_some_and(|l| l.items > 0) {
            self.sep = Sep::Line;
        }
        let next = match ordered {
            true => n
                .attr("start")
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(1)
                .clamp(0, MAX_NUMBER),
            false => 1,
        };
        // Two lists of a kind in a row are one list to Markdown unless
        // their markers differ.
        let alt = self.last_list.is_some_and(|(o, alt, at, depth)| {
            o == ordered && at == self.out.len() && depth == self.frames.len() && !alt
        });
        self.lists.push(List {
            ordered,
            next,
            items: 0,
            alt,
        });
        self.children(n);
        self.flush();
        self.lists.pop();
        self.last_list = Some((ordered, alt, self.out.len(), self.frames.len()));
    }

    fn item(&mut self, n: Node<'_>) {
        if self.inline_only() {
            self.gap();
            self.children(n);
            self.gap();
            return;
        }
        self.flush();
        let (ordered, number, first, alt) = match self.lists.last_mut() {
            Some(l) => {
                let at = (l.ordered, l.next, l.items == 0, l.alt);
                l.next = (l.next + 1).min(MAX_NUMBER);
                l.items += 1;
                at
            }
            // An <li> outside any list, as some pages write it.
            None => (false, 1, false, false),
        };
        if !first {
            self.sep = Sep::Line;
        }
        if self.md() && self.frames.len() < MAX_DEPTH {
            let marker = match (ordered, alt) {
                (true, false) => format!("{number}. "),
                (true, true) => format!("{number}) "),
                (false, false) => "- ".to_string(),
                (false, true) => "* ".to_string(),
            };
            let rest = " ".repeat(marker.len());
            self.frames.push(Frame {
                first: marker,
                rest,
                used: false,
            });
            self.children(n);
            self.flush();
            self.frames.pop();
        } else {
            self.children(n);
            self.flush();
        }
    }

    fn quote(&mut self, n: Node<'_>) {
        self.flush();
        let framed = self.md() && !self.inline_only() && self.frames.len() < MAX_DEPTH;
        if framed {
            self.frames.push(Frame {
                first: "> ".into(),
                rest: "> ".into(),
                used: false,
            });
        }
        self.children(n);
        self.flush();
        if framed {
            self.frames.pop();
        }
    }

    fn code_block(&mut self, n: Node<'_>) {
        let mut body = String::new();
        for d in n.descendants() {
            match d.kind() {
                NodeKind::Text => body.push_str(d.data().unwrap_or_default()),
                NodeKind::Element if d.tag() == Some("br") => body.push('\n'),
                _ => {}
            }
        }
        if self.cell > 0 {
            // A cell is one line: each line of the code is a span, with
            // the breaks between them kept as <br>.
            for (i, line) in body.trim_matches('\n').lines().enumerate() {
                if i > 0 {
                    self.hard_break();
                }
                self.code_span(line);
            }
            return;
        }
        if self.inline_only() {
            self.code_span(&body);
            return;
        }
        self.flush();
        let body = body.trim_start_matches('\n').trim_end();
        if body.is_empty() {
            return;
        }
        if self.md() {
            let fence = "`".repeat(longest_run(body, '`').max(2) + 1);
            let lang = language(n).unwrap_or_default();
            self.write(&format!("{fence}{lang}\n{body}\n{fence}"), false);
        } else {
            self.write(body, false);
        }
    }

    fn code(&mut self, n: Node<'_>) {
        let text = n.text();
        self.code_span(&text);
    }

    fn code_span(&mut self, raw: &str) {
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if raw.starts_with(char::is_whitespace) {
            self.gap();
        }
        if !collapsed.is_empty() {
            if self.md() {
                let ticks = "`".repeat(longest_run(&collapsed, '`') + 1);
                let pad = if collapsed.starts_with('`') || collapsed.ends_with('`') {
                    " "
                } else {
                    ""
                };
                self.push(&format!("{ticks}{pad}{collapsed}{pad}{ticks}"));
            } else {
                self.push(&collapsed);
            }
        }
        if raw.ends_with(char::is_whitespace) {
            self.gap();
        }
    }

    fn wrap(&mut self, n: Node<'_>, marker: &str, kind: usize) {
        if !self.md() || self.depth[kind] > 0 {
            self.children(n);
            return;
        }
        self.depth[kind] += 1;
        let (inner, lead, trail) = self.capture(|w| w.children(n));
        self.depth[kind] -= 1;
        if lead {
            self.gap();
        }
        let text = trim_breaks(&inner, self.line_break());
        if let (Some(first), Some(last)) = (text.chars().next(), text.chars().last()) {
            self.open();
            self.settle(first);
            let tag = ["em", "strong", "del"][kind];
            let html = format!("<{tag}>{text}</{tag}>");
            // Markdown's rule: a marker next to punctuation only opens
            // after a space or punctuation, and only closes before one.
            let before = self.line.chars().last();
            let opens =
                !punctuation(first) || before.is_none_or(|c| c.is_whitespace() || punctuation(c));
            let start = self.line.len();
            if opens {
                self.line.push_str(&format!("{marker}{text}{marker}"));
                if punctuation(last) && !trail {
                    self.fragile = Some(Fragile {
                        start,
                        end: self.line.len(),
                        html,
                    });
                }
            } else {
                self.line.push_str(&html);
            }
        }
        if trail {
            self.gap();
        }
    }

    fn link(&mut self, n: Node<'_>) {
        // A permalink: a link to a #fragment that's only a symbol (¶, #, §).
        if n.attr("href").is_some_and(|h| h.trim().starts_with('#'))
            && !n.text().chars().any(char::is_alphanumeric)
        {
            return;
        }
        let href = n.attr("href").map(str::trim).filter(|h| {
            !h.is_empty()
                && !h
                    .get(..11)
                    .is_some_and(|s| s.eq_ignore_ascii_case("javascript:"))
        });
        let Some(href) = href.filter(|_| self.md() && self.depth[LINK] == 0) else {
            self.children(n);
            return;
        };
        self.depth[LINK] += 1;
        let (inner, lead, trail) = self.capture(|w| w.children(n));
        self.depth[LINK] -= 1;
        let dest = self.destination(href);
        let title = title(n);
        self.splice(&inner, lead, trail, |t| format!("[{t}]({dest}{title})"));
    }

    fn image(&mut self, n: Node<'_>) {
        if !self.md() {
            return;
        }
        let Some(src) = image_source(n) else {
            return;
        };
        let mut alt = String::new();
        let words: Vec<&str> = n
            .attr("alt")
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        escape(&words.join(" "), &mut alt);
        let dest = self.destination(src);
        let title = title(n);
        self.push(&format!("![{alt}]({dest}{title})"));
    }

    /// A link's target, absolute against the base, with what would end
    /// a Markdown destination early percent-encoded.
    fn destination(&self, href: &str) -> String {
        let url = match self.base.and_then(|b| b.join(href).ok()) {
            Some(u) => u.to_string(),
            None => href.to_string(),
        };
        let mut out = String::with_capacity(url.len());
        for c in url.chars() {
            match c {
                ' ' => out.push_str("%20"),
                '(' => out.push_str("%28"),
                ')' => out.push_str("%29"),
                '<' => out.push_str("%3C"),
                '>' => out.push_str("%3E"),
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
        out
    }

    fn table(&mut self, n: Node<'_>) {
        if self.inline_only() {
            self.children(n);
            return;
        }
        let mut caption = None;
        let mut rows = Vec::new();
        for c in n.children() {
            match c.tag() {
                Some("tr") => rows.push(c),
                Some("thead" | "tbody" | "tfoot") if !unseen(c, false) => {
                    rows.extend(c.children().filter(|r| r.tag() == Some("tr")))
                }
                Some("caption") => caption = Some(c),
                _ => {}
            }
        }
        rows.retain(|r| !unseen(*r, false) && !self.skip.contains(&r.id()));
        self.flush();
        if let Some(c) = caption {
            self.element(c);
        }
        if !self.md() {
            for (i, row) in rows.iter().enumerate() {
                let cells: Vec<String> = cells(*row).map(|c| self.cell_text(c)).collect();
                if i > 0 {
                    self.sep = Sep::Line;
                }
                let line = cells.join("\t");
                if !line.trim().is_empty() {
                    self.write(line.trim_end(), false);
                }
            }
            return;
        }
        // The grid is as wide as the row with the most cells. A table
        // nested in another, or of one column, lays a page out rather
        // than holding data, and its cells are read as blocks; so is a
        // table whose spans would make the grid much bigger than its
        // cells, which a page only does to be laid out (or to hurt).
        let counts: Vec<usize> = rows.iter().map(|r| cells(*r).count()).collect();
        let columns = counts.iter().copied().max().unwrap_or(0);
        let real: usize = counts.iter().sum();
        if columns <= 1 || rows.len() * columns > 4 * real + 64 || self.holds_table(n) {
            for row in rows {
                self.element(row);
            }
            return;
        }
        let grid = self.grid(&rows, columns);
        let mut lines = Vec::with_capacity(grid.len() + 1);
        for (i, row) in grid.iter().enumerate() {
            let mut line = String::from("|");
            for c in 0..columns {
                line.push(' ');
                line.push_str(row.get(c).map_or("", String::as_str));
                line.push_str(" |");
            }
            lines.push(line);
            if i == 0 {
                lines.push(format!("|{}", " --- |".repeat(columns)));
            }
        }
        self.write(&lines.join("\n"), false);
    }

    /// Whether `table` has a table inside it. The first table met looks
    /// through everything inside it once, noting every table that holds
    /// another, so nested tables cost one walk rather than one each.
    fn holds_table(&mut self, table: Node<'_>) -> bool {
        if self.scanned.insert(table.id()) {
            let mut stack: Vec<(Node<'_>, Node<'_>)> =
                table.children().map(|c| (c, table)).collect();
            while let Some((n, within)) = stack.pop() {
                let mut within = within;
                if n.tag() == Some("table") {
                    self.nested.insert(within.id());
                    self.scanned.insert(n.id());
                    within = n;
                }
                stack.extend(n.children().map(|c| (c, within)));
            }
        }
        self.nested.contains(&table.id())
    }

    /// The rows' cells laid out on a grid `columns` wide: a cell spanning
    /// columns is followed by empty ones, and one spanning rows leaves its
    /// column empty in the rows below. Cells that don't fit join the
    /// row's last cell.
    ///
    /// Known limit: Markdown tables can't merge cells, so a spanning
    /// cell's text appears once, in its first row and column.
    fn grid(&mut self, rows: &[Node<'_>], columns: usize) -> Vec<Vec<String>> {
        let mut grid = Vec::with_capacity(rows.len());
        // Rows still covered by a cell above, by column.
        let mut covered: Vec<usize> = vec![0; columns];
        for row in rows {
            let mut line: Vec<String> = Vec::with_capacity(columns);
            let take_covered = |line: &mut Vec<String>, covered: &mut Vec<usize>| {
                while covered.get(line.len()).is_some_and(|&c| c > 0) {
                    covered[line.len()] -= 1;
                    line.push(String::new());
                }
            };
            for cell in cells(*row) {
                take_covered(&mut line, &mut covered);
                let text = self.cell_text(cell);
                if line.len() >= columns {
                    if let Some(last) = line.iter_mut().rev().find(|c| !c.is_empty()) {
                        last.push(' ');
                        last.push_str(&text);
                    } else if let Some(last) = line.last_mut() {
                        *last = text;
                    }
                    continue;
                }
                let span = |name: &str| {
                    cell.attr(name)
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(1)
                        .max(1)
                };
                let (across, down) = (span("colspan"), span("rowspan").min(rows.len()));
                let at = line.len();
                line.push(text);
                while line.len() < (at + across).min(columns) {
                    line.push(String::new());
                }
                for c in &mut covered[at..line.len()] {
                    *c = down - 1;
                }
            }
            take_covered(&mut line, &mut covered);
            grid.push(line);
        }
        grid
    }

    fn cell_text(&mut self, cell: Node<'_>) -> String {
        self.cell += 1;
        let (inner, _, _) = self.capture(|w| w.children(cell));
        self.cell -= 1;
        let text = inner.trim();
        match self.style {
            Style::Markdown => text.replace('|', "\\|"),
            Style::Text => text.to_string(),
        }
    }
}

fn cells<'a>(row: Node<'a>) -> impl Iterator<Item = Node<'a>> + use<'a> {
    row.children()
        .filter(|c| matches!(c.tag(), Some("td" | "th")) && !unseen(*c, false))
}

/// `text` without the line breaks and spaces at its end.
fn trim_breaks<'t>(mut text: &'t str, brk: &str) -> &'t str {
    loop {
        let trimmed = text.trim_end_matches(' ');
        match trimmed.strip_suffix(brk) {
            Some(rest) => text = rest,
            None => return trimmed,
        }
    }
}

/// ` "title"` for an element with a title, or nothing.
fn title(n: Node<'_>) -> String {
    let words: Vec<&str> = n
        .attr("title")
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    if words.is_empty() {
        return String::new();
    }
    let text = words.join(" ").replace('\\', "\\\\").replace('"', "\\\"");
    format!(" \"{text}\"")
}

/// The language a code block names in its class (`language-rust`,
/// `lang-py`), on the `<pre>` or the `<code>` in it, or in a
/// `highlight-python3` class on the wrappers Sphinx and Pygments put
/// around it (where `highlight-default` names none).
fn language(pre: Node<'_>) -> Option<String> {
    let code = pre.children().find(|c| c.tag() == Some("code"));
    let parent = pre.parent();
    let grandparent = parent.and_then(|p| p.parent());
    [Some(pre), code, parent, grandparent]
        .into_iter()
        .flatten()
        .filter_map(|n| n.attr("class"))
        .flat_map(str::split_whitespace)
        .find_map(|c| {
            c.strip_prefix("language-")
                .or_else(|| c.strip_prefix("lang-"))
                .or_else(|| c.strip_prefix("highlight-"))
        })
        .filter(|lang| !matches!(*lang, "default" | "none"))
        .map(|lang| {
            lang.chars()
                .filter(|c| c.is_alphanumeric() || "+-#._".contains(*c))
                .collect::<String>()
        })
        .filter(|lang| !lang.is_empty())
}

fn longest_run(text: &str, c: char) -> usize {
    let (mut best, mut run) = (0, 0);
    for ch in text.chars() {
        run = if ch == c { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

/// Appends `word` with a backslash before each character Markdown would
/// read as markup: `\`, `` ` ``, `*`, `[`, `]`, `<` and `~` everywhere,
/// `_` unless it's inside a word (`snake_case` is safe), and `&` when it
/// would start an entity. What only matters at the start of a line is
/// left to [`fix_start`].
fn escape(word: &str, out: &mut String) {
    const SPECIAL: &[u8] = b"\\`*_[]<~&";
    if !word.bytes().any(|b| SPECIAL.contains(&b)) {
        out.push_str(word);
        return;
    }
    let mut prev: Option<char> = None;
    let mut chars = word.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next = chars.peek().map(|&(_, c)| c);
        let needed = match c {
            '\\' | '`' | '*' | '[' | ']' | '<' | '~' => true,
            '_' => {
                !(prev.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric))
            }
            '&' => starts_entity(&word[i + 1..]),
            _ => false,
        };
        if needed {
            out.push('\\');
        }
        out.push(c);
        prev = Some(c);
    }
}

/// Whether `rest`, after an `&`, reads as an entity (`amp;`, `#38;`).
fn starts_entity(rest: &str) -> bool {
    let body = rest.strip_prefix('#').unwrap_or(rest);
    body.find(';')
        .is_some_and(|end| end > 0 && body[..end].chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Whether `c` is punctuation as Markdown's emphasis rules see it.
fn punctuation(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace()
}

/// Appends a line of text, escaped where its start would otherwise begin
/// a heading (`#`), a quotation (`>`), a list item (`- `, `+ `, `1. `,
/// `1) `) or a heading's underline (`---`, `===`), and with its pipes
/// escaped if it would make the lines above it a table (`--- | ---`).
fn fix_start(line: &str, out: &mut String) {
    let delimiter_row = line.contains('|')
        && line.contains('-')
        && line.bytes().all(|b| matches!(b, b'-' | b':' | b'|' | b' '));
    let unpiped;
    let line = if delimiter_row {
        unpiped = line.replace('|', "\\|");
        unpiped.as_str()
    } else {
        line
    };
    let b = line.as_bytes();
    let needed = match b[0] {
        b'#' | b'>' => true,
        b'-' | b'+' | b'=' => b.len() == 1 || b[1] == b' ' || b.iter().all(|&c| c == b[0]),
        b'0'..=b'9' => {
            let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
            if digits <= 9
                && matches!(b.get(digits), Some(b'.' | b')'))
                && matches!(b.get(digits + 1), None | Some(b' '))
            {
                out.push_str(&line[..digits]);
                out.push('\\');
                out.push_str(&line[digits..]);
                return;
            }
            false
        }
        _ => false,
    };
    if needed {
        out.push('\\');
    }
    out.push_str(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;

    fn md(html: &str) -> String {
        let doc = Document::parse(html);
        let base = Url::parse("https://example.com/blog/post").unwrap();
        markdown(doc.root(), Some(&base))
    }

    fn text(html: &str) -> String {
        let doc = Document::parse(html);
        clean_text(doc.root())
    }

    #[test]
    fn paragraphs_and_headings_are_blocks_with_one_blank_line_between() {
        assert_eq!(
            md(
                "<h1>Title</h1>\n\n<p>One\n  two</p><div><div><p>Three</p></div></div><h3>Small</h3>"
            ),
            "# Title\n\nOne two\n\nThree\n\n### Small"
        );
    }

    #[test]
    fn markup_characters_in_text_are_escaped() {
        assert_eq!(
            md(r"<p>2*3 = 6, [x] a_b _c_ `tick` \ &amp;copy; &lt;b&gt; ~~no~~ R&amp;D</p>"),
            r"2\*3 = 6, \[x\] a_b \_c\_ \`tick\` \\ \&copy; \<b> \~\~no\~\~ R&D"
        );
    }

    #[test]
    fn what_would_start_a_block_is_escaped_at_a_line_start_only() {
        assert_eq!(
            md(
                "<p># not a heading</p><p>- not a list</p><p>1. not ordered</p>\
                <p>2026. A year</p><p>> no quote</p><p>---</p><p>-5 degrees</p><p>a # b</p>"
            ),
            "\\# not a heading\n\n\\- not a list\n\n1\\. not ordered\n\n2026\\. A year\n\n\
             \\> no quote\n\n\\---\n\n-5 degrees\n\na # b"
        );
    }

    #[test]
    fn emphasis_keeps_its_spaces_outside_the_markers() {
        assert_eq!(
            md("<p>a<em> b </em>c <strong>d</strong>e <b><b>f</b></b> <i></i> <del>g</del></p>"),
            "a *b* c **d**e **f** ~~g~~"
        );
    }

    #[test]
    fn links_are_absolute_and_their_targets_are_safe() {
        assert_eq!(
            md(
                r##"<p><a href="/a b(1)">x</a> <a href="../c" title="The &quot;C&quot;">see
                  <em>c</em></a> <a href="javascript:go()">js</a> <a href="/i"></a>
                  <a href="#top">top</a></p>"##
            ),
            "[x](https://example.com/a%20b%281%29) \
             [see *c*](https://example.com/c \"The \\\"C\\\"\") js \
             [top](https://example.com/blog/post#top)"
        );
    }

    #[test]
    fn images_keep_alt_and_title_and_find_lazy_sources() {
        assert_eq!(
            md(
                r#"<p><img src="/i.png" alt="A [b]" title="T"> <img data-src="lazy.jpg" src="data:x">
                  <img srcset="/s1.jpg 1x, /s2.jpg 2x"> <img alt="none"></p>"#
            ),
            "![A \\[b\\]](https://example.com/i.png \"T\") \
             ![](https://example.com/blog/lazy.jpg) ![](https://example.com/s1.jpg)"
        );
    }

    #[test]
    fn lists_nest_and_ordered_ones_keep_their_start() {
        assert_eq!(
            md(
                "<ul><li>a<ul><li>b</li><li>c<ol start=9><li>d</li><li>e</li></ol></li></ul></li>\
                <li><p>f</p><p>g</p></li></ul><p>after</p>"
            ),
            "- a\n  - b\n  - c\n    9. d\n    10. e\n- f\n\n  g\n\nafter"
        );
    }

    #[test]
    fn blockquotes_prefix_every_line_including_blank_ones() {
        assert_eq!(
            md("<blockquote><p>a</p><p>b<br>c</p><blockquote>d</blockquote></blockquote>"),
            "> a\n>\n> b\\\n> c\n>\n> > d"
        );
    }

    #[test]
    fn code_blocks_are_fenced_longer_than_any_backticks_inside() {
        assert_eq!(
            md(r#"<pre><code class="hljs language-rust">let s = "```";
fn f() {}

// end
</code></pre><p>use <code>a`b</code> and <code>`x</code></p>"#),
            "````rust\nlet s = \"```\";\nfn f() {}\n\n// end\n````\n\n\
             use ``a`b`` and `` `x ``"
        );
    }

    #[test]
    fn code_in_a_list_item_is_indented_with_it() {
        assert_eq!(
            md("<ol><li>Run:<pre>a\n\nb</pre></li></ol>"),
            "1. Run:\n\n   ```\n   a\n\n   b\n   ```"
        );
    }

    #[test]
    fn tables_become_pipe_tables_with_spans_filled_in() {
        assert_eq!(
            md(
                "<table><caption>Sizes</caption><tr><th>Name<th>A|B<th>C</tr>\
                <tr><td colspan=2>wide<td rowspan=2>tall</tr><tr><td>x<br>y<td><b>z</b></tr></table>"
            ),
            "Sizes\n\n| Name | A\\|B | C |\n| --- | --- | --- |\n| wide |  | tall |\n| x<br>y | **z** |  |"
        );
    }

    #[test]
    fn layout_tables_are_read_as_blocks() {
        assert_eq!(
            md(
                "<table><tr><td><p>one</p><table><tr><td>a</td><td>b</td></tr></table></td></tr></table>"
            ),
            "one\n\n| a | b |\n| --- | --- |"
        );
        assert_eq!(
            md("<table><tr><td>only</td></tr><tr><td>col</td></tr></table>"),
            "only\n\ncol"
        );
    }

    #[test]
    fn hidden_and_unseen_elements_are_left_out() {
        assert_eq!(
            md(
                r#"<p>a<script>x()</script><style>p{}</style><span hidden>b</span>
                  <span aria-hidden="true">c</span><span style="display: none">d</span>
                  <template>e</template><noscript>f</noscript>g</p>"#
            ),
            "a g"
        );
    }

    #[test]
    fn two_line_breaks_end_a_paragraph_and_no_blank_lines_pile_up() {
        assert_eq!(
            md("<p>a<br><br><br>b</p><div></div><p> </p><div><p>c</p></div><br>"),
            "a\n\nb\n\nc"
        );
    }

    #[test]
    fn entities_are_decoded_once() {
        assert_eq!(
            md("<p>caf&eacute; &mdash; 5&nbsp;&gt;&nbsp;3</p>"),
            "café — 5\u{a0}>\u{a0}3"
        );
    }

    #[test]
    fn a_link_around_blocks_becomes_one_link() {
        assert_eq!(
            md(r#"<a href="/p"><div><h3>Lamp</h3><p>Bright</p></div></a>"#),
            "[Lamp Bright](https://example.com/p)"
        );
    }

    #[test]
    fn text_keeps_blocks_and_drops_markup() {
        assert_eq!(
            text(
                "<h1>T</h1><p>a <b>b</b> <a href=x>c</a><br>d</p><ul><li>e</li><li>f</li></ul>\
                  <table><tr><th>x</th><th>y</th></tr><tr><td>1</td><td>2</td></tr></table>\
                  <pre>  keep\n    this</pre>"
            ),
            "T\n\na b c\nd\n\ne\nf\n\nx\ty\n1\t2\n\n  keep\n    this"
        );
    }

    #[test]
    fn a_bang_before_a_link_does_not_make_an_image() {
        assert_eq!(
            md("<p>Wow!<a href=/x>link</a></p>"),
            "Wow\\![link](https://example.com/x)"
        );
    }

    #[test]
    fn adjacent_lists_alternate_their_markers_so_they_stay_apart() {
        assert_eq!(
            md(
                "<ul><li>a</li></ul><ul><li>b</li></ul><ul><li>c</li></ul><ol><li>d</li></ol><ol><li>e</li></ol>"
            ),
            "- a\n\n* b\n\n- c\n\n1. d\n\n1) e"
        );
        assert_eq!(
            md("<ul><li>a</li></ul><p>x</p><ul><li>b</li></ul>"),
            "- a\n\nx\n\n- b"
        );
    }

    #[test]
    fn ordered_list_starts_stay_within_what_markdown_allows() {
        assert_eq!(md("<ol start=-3><li>a</li></ol>"), "0. a");
        // After another ordered list it takes the other marker, still a list
        // starting at 0.
        assert_eq!(
            md("<ol><li>x</li></ol><ol start=-3><li>a</li></ol>"),
            "1. x\n\n0) a"
        );
        assert_eq!(md("<ol start=99999999999><li>a</li></ol>"), "999999999. a");
    }

    #[test]
    fn a_link_across_a_line_break_keeps_its_target() {
        assert_eq!(
            md("<p><a href=/x>a<br>b</a></p>"),
            "[a\\\nb](https://example.com/x)"
        );
    }

    #[test]
    fn a_heading_ending_in_a_hash_keeps_it() {
        assert_eq!(
            md("<h2>Issue #</h2><h3>C#</h3>"),
            "## Issue \\#\n\n### C\\#"
        );
    }

    #[test]
    fn non_breaking_spaces_are_kept_in_markdown_and_plain_in_text() {
        assert_eq!(
            md("<p>20&nbsp;g\u{3000}x</p><p>&nbsp;</p>"),
            "20\u{a0}g\u{3000}x"
        );
        assert_eq!(
            text("<p>20&nbsp;g\u{3000}x</p><p>&nbsp;</p>"),
            "20 g\u{3000}x"
        );
    }

    #[test]
    fn a_line_that_looks_like_a_table_row_is_escaped() {
        assert_eq!(md("<p>a | b<br>--- | ---</p>"), "a | b\\\n--- \\| ---");
    }

    #[test]
    fn code_in_a_table_cell_keeps_its_lines() {
        assert_eq!(
            md(
                "<table><tr><th>a</th><th>b</th></tr><tr><td><pre>x = 1\ny = 2</pre></td><td>z</td></tr></table>"
            ),
            "| a | b |\n| --- | --- |\n| `x = 1`<br>`y = 2` | z |"
        );
    }

    #[test]
    fn emphasis_that_could_not_open_or_close_is_written_as_html() {
        assert_eq!(
            md("<p><strong>\"q\"</strong>word and a<em>(x)</em>b <em>fine</em></p>"),
            "<strong>\"q\"</strong>word and a<em>(x)</em>b *fine*"
        );
    }

    #[test]
    fn permalinks_and_edit_links_are_dropped() {
        assert_eq!(
            md(r##"<h2>Setup<a class="headerlink" href="#setup">¶</a></h2>
                  <h2>Usage<span class="mw-editsection">[<a href="/w?action=edit">edit</a>]</span></h2>
                  <h2><a href="#x" aria-label="anchor">#</a>Other</h2><p>See<a href="#n1">[1]</a></p>"##),
            "## Setup\n\n## Usage\n\n## Other\n\nSee[\\[1\\]](https://example.com/blog/post#n1)"
        );
    }

    #[test]
    fn the_language_can_come_from_a_highlight_wrapper() {
        assert_eq!(
            md(
                r#"<div class="highlight-python3 notranslate"><div class="highlight"><pre>x = 1</pre></div></div>
                  <div class="highlight-default"><div class="highlight"><pre>y</pre></div></div>"#
            ),
            "```python3\nx = 1\n```\n\n```\ny\n```"
        );
    }

    #[test]
    fn deep_nesting_is_capped_so_output_stays_proportional() {
        let lists = "<ul><li>x".repeat(5000);
        let out = md(&lists);
        assert!(out.len() < lists.len() * 4, "{} bytes", out.len());
        assert!(out.lines().all(|l| l.len() <= 2 * MAX_DEPTH + 3));
        let quotes = "<blockquote>x".repeat(5000);
        let out = md(&quotes);
        assert!(out.len() < quotes.len() * 4, "{} bytes", out.len());
        let spans = "<table><tr><td>a<td>b</tr>".to_string()
            + &"<tr><td colspan=64 rowspan=64>a</td></tr>".repeat(5000)
            + "</table>";
        let out = md(&spans);
        assert!(out.len() < spans.len() * 4, "{} bytes", out.len());
    }

    #[test]
    fn nested_tables_take_linear_time() {
        let html = "<table><tr><td>x".repeat(8000);
        let start = std::time::Instant::now();
        md(&html);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn output_is_the_same_every_time() {
        let html = "<ul><li>a<table><tr><td>1<td>2</table></li></ul><blockquote>q</blockquote>";
        assert_eq!(md(html), md(html));
    }
}
