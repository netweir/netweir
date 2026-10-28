//! Markdown cut into chunks for retrieval: along its headings first, each
//! chunk no longer than a limit, code blocks and tables kept whole where
//! they fit, and each chunk told the headings it sits under.
//!
//! It reads the Markdown [`crate::markdown`] writes, whose shape is
//! known: ATX headings, blocks separated by blank lines, code fenced with
//! backticks and GFM tables. Text that only looks like a heading or a
//! fence is escaped by the writer, so it's never read as one.

use std::collections::HashMap;

use url::Url;

use crate::Node;

/// How big chunks get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The most characters (Unicode scalar values) a chunk holds. At
    /// least 1.
    pub max_chars: usize,
    /// How many characters from the end of a chunk start the next one in
    /// the same section, cut at a space. Less than `max_chars`.
    pub overlap: usize,
}

/// One piece of the Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The SHA-256 of the page's URL, the headings and the text, as 32
    /// hex digits: the same chunk of the same page has the same id from
    /// crawl to crawl, and a change to one section leaves the others' ids
    /// alone. A chunk repeated under the same headings gets `-2`, `-3`
    /// after it, in page order.
    pub id: String,
    pub text: String,
    /// The headings the chunk sits under, outermost first, as written
    /// (without their `#`s).
    pub headings: Vec<String>,
    /// Its place among the page's chunks, from 0.
    pub index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Heading(usize),
    Fence,
    Table,
    Text,
}

/// `node`'s Markdown in chunks, links absolute against `base`, ids made
/// from `url`. With `main_only`, its main content's, or the whole node's
/// when it has none.
pub fn node_chunks(
    node: Node<'_>,
    base: Option<&Url>,
    url: &str,
    main_only: bool,
    options: Options,
) -> Vec<Chunk> {
    let markdown = main_only
        .then(|| crate::content::main_content(node))
        .flatten()
        .map(|m| m.markdown(base))
        .unwrap_or_else(|| crate::markdown::markdown(node, base));
    chunks(&markdown, url, options)
}

impl Chunk {
    /// The chunk as a JSON object: `id`, `text`, `headings`, `index` and
    /// the page's `url`.
    pub fn to_json(&self, url: Option<&Url>) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "text": self.text,
            "headings": self.headings,
            "index": self.index,
            "url": url.map(Url::as_str),
        })
    }
}

/// `markdown` in chunks, with ids made from `url`.
pub fn chunks(markdown: &str, url: &str, options: Options) -> Vec<Chunk> {
    let max = options.max_chars.max(1);
    let overlap = options.overlap.min(max - 1);
    let mut packer = Packer {
        max,
        overlap,
        out: Vec::new(),
        parts: Vec::new(),
        len: 0,
        headings: Vec::new(),
        only_headings: false,
        last_level: 0,
        last_text: false,
        carry: None,
    };
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (kind, block) in blocks(markdown) {
        match kind {
            Kind::Heading(level) => {
                // A heading under one with nothing below it joins it, so
                // "# A" then "## B" is one chunk under both, if both fit.
                let joins = packer.only_headings
                    && level > packer.last_level
                    && packer.len + 2 + chars(block) <= max;
                if !joins {
                    packer.flush(false);
                }
                packer.carry = None;
                while stack.last().is_some_and(|(l, _)| *l >= level) {
                    stack.pop();
                }
                stack.push((level, block[level..].trim().to_string()));
                packer.headings = stack.iter().map(|(_, h)| h.clone()).collect();
                for piece in split_text(block, max) {
                    packer.add(piece, false);
                }
                packer.only_headings = true;
                packer.last_level = level;
            }
            Kind::Fence | Kind::Table | Kind::Text => {
                let pieces = match kind {
                    _ if chars(block) <= max => vec![block.to_string()],
                    Kind::Fence => split_fence(block, max),
                    Kind::Table => split_table(block, max),
                    _ => split_text(block, max),
                };
                for piece in pieces {
                    packer.add(piece, kind == Kind::Text);
                }
                packer.only_headings = false;
            }
        }
    }
    packer.flush(false);
    let mut seen: HashMap<String, usize> = HashMap::new();
    packer
        .out
        .into_iter()
        .enumerate()
        .map(|(index, (text, headings))| {
            let hash = id(url, &headings, &text);
            let n = seen.entry(hash.clone()).or_insert(0);
            *n += 1;
            let id = if *n == 1 { hash } else { format!("{hash}-{n}") };
            Chunk {
                id,
                text,
                headings,
                index,
            }
        })
        .collect()
}

struct Packer {
    max: usize,
    overlap: usize,
    out: Vec<(String, Vec<String>)>,
    parts: Vec<String>,
    /// The characters in `parts`, with the blank lines between them.
    len: usize,
    headings: Vec<String>,
    /// Whether `parts` holds only headings.
    only_headings: bool,
    last_level: usize,
    /// Whether the last piece added was text, which an overlap may come
    /// from; a fence's or a table's end would open or break what follows.
    last_text: bool,
    /// The end of the chunk just written, to start the next with.
    carry: Option<String>,
}

impl Packer {
    fn add(&mut self, piece: String, text: bool) {
        if piece.trim().is_empty() {
            return;
        }
        let n = chars(&piece);
        if !self.parts.is_empty() && self.len + 2 + n > self.max {
            self.flush(true);
        }
        if self.parts.is_empty() {
            // Only text takes the overlap: a code block or a table piece
            // starts clean.
            if let Some(carry) = self
                .carry
                .take()
                .filter(|c| text && chars(c) + 2 + n <= self.max)
            {
                self.len = chars(&carry);
                self.parts.push(carry);
            }
        }
        self.len += if self.parts.is_empty() { n } else { 2 + n };
        self.parts.push(piece);
        self.last_text = text;
    }

    /// Writes the chunk being gathered; with `carry`, keeps its end to
    /// start the next one.
    fn flush(&mut self, carry: bool) {
        if self.parts.is_empty() {
            return;
        }
        let text = self.parts.join("\n\n");
        self.carry = (carry && self.overlap > 0 && self.last_text)
            .then(|| tail(&text, self.overlap))
            .filter(|t| !t.is_empty());
        self.out.push((text, self.headings.clone()));
        self.parts.clear();
        self.len = 0;
        self.only_headings = false;
    }
}

/// The SHA-256 of `url`, `headings` and `text`, as 32 hex digits.
fn id(url: &str, headings: &[String], text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    h.update([0]);
    h.update(headings.join("\u{1f}").as_bytes());
    h.update([0]);
    h.update(text.as_bytes());
    h.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// The Markdown's blocks, in order: what blank lines separate, with a
/// code fence's blank lines kept inside it.
fn blocks(markdown: &str) -> Vec<(Kind, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut fence: Option<usize> = None;
    let mut end = 0;
    let mut at = 0;
    for line in markdown.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let bare = content.trim_start_matches([' ', '>']);
        let ticks = bare.bytes().take_while(|b| *b == b'`').count();
        match fence {
            Some(open) => {
                if ticks >= open && bare[ticks..].trim().is_empty() {
                    fence = None;
                }
                end = at + content.len();
            }
            // A blank line, or one inside a quotation (`>`), ends a block.
            None if bare.trim().is_empty() => {
                if let Some(s) = start.take() {
                    out.push(&markdown[s..end]);
                }
            }
            None => {
                if opens_fence(bare) {
                    fence = Some(ticks);
                }
                start.get_or_insert(at);
                end = at + content.len();
            }
        }
        at += line.len();
    }
    if let Some(s) = start {
        out.push(&markdown[s..end]);
    }
    out.into_iter().map(|b| (kind(b), b)).collect()
}

/// Whether a line, past any indent and `>`s, opens a code fence: three
/// or more backticks and no backtick after them, which CommonMark
/// forbids in a fence's info string. A paragraph starting with a code
/// span (`` ```a``b``` ``) has one, so it isn't taken for a fence.
fn opens_fence(bare: &str) -> bool {
    let ticks = bare.bytes().take_while(|b| *b == b'`').count();
    ticks >= 3 && !bare[ticks..].contains('`')
}

/// Whether a line is a table's delimiter row: `| --- | :-: |`.
fn delimiter_row(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('|')
        && line.contains('-')
        && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn kind(block: &str) -> Kind {
    let hashes = block.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes) && !block.contains('\n') && block[hashes..].starts_with(' ') {
        return Kind::Heading(hashes);
    }
    let first = block.lines().next().unwrap_or("");
    if opens_fence(first.trim_start_matches([' ', '>'])) {
        return Kind::Fence;
    }
    // A paragraph can start with a pipe; a table has its delimiter row.
    if block.starts_with('|') && block.lines().nth(1).is_some_and(delimiter_row) {
        return Kind::Table;
    }
    Kind::Text
}

/// A code block too big for one chunk, a run of its lines at a time, each
/// fenced as the block was. One indented in a list item comes out
/// unindented, so it still reads as code on its own; one in a quotation
/// keeps its `>`s.
fn split_fence(block: &str, max: usize) -> Vec<String> {
    let first = block.lines().next().unwrap_or("");
    let bare = first.trim_start_matches([' ', '>']);
    let prefix = &first[..first.len() - bare.len()];
    let indent = !prefix.contains('>');
    let lines: Vec<&str> = block
        .lines()
        .map(|l| match indent {
            true => l.strip_prefix(prefix).unwrap_or(l.trim_start_matches(' ')),
            false => l,
        })
        .collect();
    let prefix = if indent { "" } else { prefix };
    let open = lines[0];
    let ticks = bare.bytes().take_while(|b| *b == b'`').count();
    let last = lines[lines.len() - 1]
        .trim_start_matches([' ', '>'])
        .trim_end();
    let closed = lines.len() > 1 && last.len() >= ticks && last.bytes().all(|b| b == b'`');
    // Its own closing line, or one as long as the opening fence.
    let close = match closed {
        true => lines[lines.len() - 1].to_string(),
        false => format!("{prefix}{}", &bare[..ticks]),
    };
    let body = &lines[1..lines.len() - usize::from(closed)];
    let room = max.saturating_sub(chars(open) + chars(&close) + 2);
    if room == 0 {
        return split_text(block, max);
    }
    let wrap = |lines: &[String]| format!("{open}\n{}\n{close}", lines.join("\n"));
    rows(body, room).iter().map(|run| wrap(run)).collect()
}

/// A table too big for one chunk, a run of its rows at a time, each
/// under the table's header and delimiter rows.
fn split_table(block: &str, max: usize) -> Vec<String> {
    let lines: Vec<&str> = block.lines().collect();
    if lines.len() < 3 {
        return split_text(block, max);
    }
    let head = format!("{}\n{}", lines[0], lines[1]);
    let room = max.saturating_sub(chars(&head) + 1);
    if room == 0 {
        return split_text(block, max);
    }
    rows(&lines[2..], room)
        .iter()
        .map(|run| format!("{head}\n{}", run.join("\n")))
        .collect()
}

/// `lines` in runs of at most `room` characters with their line breaks;
/// a line longer than that is cut.
fn rows(lines: &[&str], room: usize) -> Vec<Vec<String>> {
    let mut runs: Vec<Vec<String>> = Vec::new();
    let mut run: Vec<String> = Vec::new();
    let mut len = 0;
    for line in lines {
        for piece in hard(line, room) {
            let n = chars(&piece);
            if !run.is_empty() && len + 1 + n > room {
                runs.push(std::mem::take(&mut run));
                len = 0;
            }
            len += if run.is_empty() { n } else { 1 + n };
            run.push(piece);
        }
    }
    if !run.is_empty() || runs.is_empty() {
        runs.push(run);
    }
    runs
}

/// `text` in pieces of at most `max` characters: at line breaks, then at
/// sentence ends, then at spaces, and only then within a word.
fn split_text(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    split_at(text.trim(), max, 0, &mut out);
    // Trimmed only now: a piece carried on keeps the space or line break
    // that separates it from the next.
    out.into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// `text` cut into pieces of at most `max` characters that, joined, are
/// `text` again.
fn split_at(text: &str, max: usize, level: usize, out: &mut Vec<String>) {
    if text.is_empty() {
        return;
    }
    if chars(text) <= max {
        out.push(text.to_string());
        return;
    }
    let pieces = match level {
        0 => after(text, |s| s.starts_with('\n')),
        1 => after(text, |s| {
            let mut c = s.chars();
            matches!(c.next(), Some('.' | '!' | '?')) && c.next().is_some_and(char::is_whitespace)
                || matches!(s.chars().next(), Some('。' | '！' | '？'))
        }),
        2 => after(text, |s| s.starts_with(' ')),
        _ => {
            out.extend(hard(text, max));
            return;
        }
    };
    // Packed greedily, a piece too big on its own split at the next
    // level down.
    let mut current = String::new();
    let mut len = 0;
    for piece in pieces {
        let n = chars(piece);
        if len + n <= max {
            current.push_str(piece);
            len += n;
            continue;
        }
        if !current.is_empty() {
            out.push(current);
        }
        current = String::new();
        len = 0;
        if n <= max {
            current.push_str(piece);
            len = n;
        } else {
            // The last of its pieces goes on with the rest.
            split_at(piece, max, level + 1, out);
            if let Some(last) = out.pop() {
                len = chars(&last);
                current = last;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
}

/// `text` cut after each place `ends` matches, the match kept on the
/// left: a sentence keeps its full stop, a line its break.
fn after(text: &str, ends: impl Fn(&str) -> bool) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if ends(&text[i..]) {
            let end = i + c.len_utf8();
            pieces.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// `text` cut every `max` characters.
fn hard(text: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(max.max(1))
        .map(|c| c.iter().collect())
        .collect()
}

/// At most the last `n` characters of `text`, starting after a space so
/// no word is cut; empty when the last word alone is longer.
fn tail(text: &str, n: usize) -> String {
    let total = chars(text);
    if total <= n {
        return text.to_string();
    }
    let from = text
        .char_indices()
        .nth(total - n)
        .map_or(text.len(), |(i, _)| i);
    let rest = &text[from..];
    if text[..from].ends_with(char::is_whitespace) {
        return rest.trim().to_string();
    }
    match rest.find(char::is_whitespace) {
        Some(space) => rest[space..].trim().to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(md: &str, max: usize, overlap: usize) -> Vec<Chunk> {
        chunks(
            md,
            "https://e.com/",
            Options {
                max_chars: max,
                overlap,
            },
        )
    }

    fn texts(chunks: &[Chunk]) -> Vec<&str> {
        chunks.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn each_heading_starts_a_chunk_and_names_the_path() {
        let md = "Intro.\n\n# Guide\n\nWelcome.\n\n## Install\n\nRun it.\n\n## Use\n\nCall it.\n\n# FAQ\n\nAsk.";
        let got = cut(md, 1000, 0);
        assert_eq!(
            texts(&got),
            [
                "Intro.",
                "# Guide\n\nWelcome.",
                "## Install\n\nRun it.",
                "## Use\n\nCall it.",
                "# FAQ\n\nAsk."
            ]
        );
        let paths: Vec<Vec<String>> = got.iter().map(|c| c.headings.clone()).collect();
        assert_eq!(
            paths,
            [
                vec![],
                vec!["Guide".to_string()],
                vec!["Guide".into(), "Install".into()],
                vec!["Guide".into(), "Use".into()],
                vec!["FAQ".into()],
            ]
        );
        assert_eq!(
            got.iter().map(|c| c.index).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn a_heading_with_nothing_under_it_joins_the_next() {
        let got = cut(
            "# Guide\n\n## Install\n\nRun it.\n\n## Use\n\n# FAQ\n\nAsk.",
            1000,
            0,
        );
        assert_eq!(
            texts(&got),
            [
                "# Guide\n\n## Install\n\nRun it.",
                "## Use",
                "# FAQ\n\nAsk."
            ]
        );
        assert_eq!(got[0].headings, ["Guide", "Install"]);
        assert_eq!(got[1].headings, ["Guide", "Use"]);
    }

    #[test]
    fn blocks_are_packed_up_to_the_limit() {
        let got = cut("aaaa\n\nbbbb\n\ncccc", 10, 0);
        assert_eq!(texts(&got), ["aaaa\n\nbbbb", "cccc"]);
    }

    #[test]
    fn a_long_paragraph_is_cut_at_sentences_then_spaces_then_anywhere() {
        let got = cut("One two. Three four five. Six.", 12, 0);
        assert_eq!(texts(&got), ["One two.", "Three four", "five. Six."]);
        let got = cut("abcdefghij", 4, 0);
        assert_eq!(texts(&got), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn a_list_is_cut_between_items() {
        let got = cut("- one\n- two\n- three", 12, 0);
        assert_eq!(texts(&got), ["- one\n- two", "- three"]);
    }

    #[test]
    fn a_code_block_with_blank_lines_stays_one_block() {
        let md = "```py\na = 1\n\nb = 2\n```\n\nAfter.";
        let got = cut(md, 1000, 0);
        assert_eq!(texts(&got), [md]);
        let got = cut(md, 22, 0);
        assert_eq!(texts(&got), ["```py\na = 1\n\nb = 2\n```", "After."]);
    }

    #[test]
    fn a_big_code_block_is_cut_into_fenced_runs_of_lines() {
        let md = "```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```";
        let got = cut(md, 33, 0);
        assert_eq!(
            texts(&got),
            [
                "```rust\nlet a = 1;\nlet b = 2;\n```",
                "```rust\nlet c = 3;\n```"
            ]
        );
    }

    #[test]
    fn a_big_table_keeps_its_header_on_each_piece() {
        let md = "| a | b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n| 5 | 6 |";
        let got = cut(md, 43, 0);
        assert_eq!(
            texts(&got),
            [
                "| a | b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |",
                "| a | b |\n| --- | --- |\n| 5 | 6 |"
            ]
        );
    }

    #[test]
    fn the_overlap_starts_the_next_chunk_at_a_word_and_stops_at_headings() {
        let got = cut("alpha beta gamma\n\ndelta epsilon\n\n# H\n\nzeta", 30, 11);
        assert_eq!(
            texts(&got),
            [
                "alpha beta gamma",
                "beta gamma\n\ndelta epsilon",
                "# H\n\nzeta"
            ]
        );
        assert_eq!(tail("alpha beta gamma", 11), "beta gamma");
        assert_eq!(tail("abcdefghijklmnop", 5), "");
    }

    #[test]
    fn no_chunk_is_over_the_limit() {
        let md = "# Héllo wörld ünïcode\n\n```\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n```\n\n| a |\n| - |\n| ééééééééééééééééééééé |\n\n日本語の文。とても長い文章です。\n\nplain words go here and there";
        for max in 1..40 {
            for overlap in [0, max / 2, max] {
                for c in cut(md, max, overlap) {
                    assert!(chars(&c.text) <= max, "{max} {overlap}: {:?}", c.text);
                    assert!(!c.text.is_empty());
                }
            }
        }
    }

    #[test]
    fn characters_are_counted_not_bytes() {
        let got = cut("日本語\n\n日本語", 8, 0);
        assert_eq!(texts(&got), ["日本語\n\n日本語"]);
    }

    #[test]
    fn ids_are_stable_distinct_and_tell_repeats_apart() {
        let a = cut("# A\n\nSame.\n\n# B\n\nSame.\n\n# B\n\nSame.", 1000, 0);
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].id.len(), 32);
        assert_ne!(a[0].id, a[1].id, "the headings are part of it");
        assert_eq!(a[2].id, format!("{}-2", a[1].id));
        let b = cut("# A\n\nChanged.\n\n# B\n\nSame.\n\n# B\n\nSame.", 1000, 0);
        assert_ne!(a[0].id, b[0].id);
        assert_eq!(a[1].id, b[1].id, "one section's change leaves the rest");
        let other = chunks(
            "# A\n\nSame.",
            "https://e.com/2",
            Options {
                max_chars: 1000,
                overlap: 0,
            },
        );
        assert_ne!(a[0].id, other[0].id, "and the page");
    }

    fn from_html(html: &str, max: usize, overlap: usize) -> Vec<Chunk> {
        let doc = crate::Document::parse(html);
        cut(&crate::markdown::markdown(doc.root(), None), max, overlap)
    }

    #[test]
    fn a_cut_keeps_the_space_or_break_between_words() {
        assert_eq!(
            texts(&cut("aaaa bbbb cccc\nxy", 10, 0)),
            ["aaaa bbbb", "cccc\nxy"]
        );
        assert_eq!(
            texts(&cut("abcdefghijkl mn", 5, 0)),
            ["abcde", "fghij", "kl mn"]
        );
    }

    #[test]
    fn a_code_block_in_a_list_item_is_cut_as_code() {
        let got = from_html(
            "<ol><li>Run:<pre>line one\nline two\nline three\nline four</pre></li></ol>",
            25,
            0,
        );
        assert_eq!(
            texts(&got),
            [
                "1. Run:",
                "```\nline one\nline two\n```",
                "```\nline three\n```",
                "```\nline four\n```"
            ]
        );
    }

    #[test]
    fn a_code_block_in_a_quotation_is_cut_as_code() {
        let got = from_html(
            "<blockquote><p>Said:</p><pre>aaaa\nbbbb\ncccc</pre></blockquote>",
            25,
            0,
        );
        assert_eq!(
            texts(&got),
            [
                "> Said:",
                "> ```\n> aaaa\n> bbbb\n> ```",
                "> ```\n> cccc\n> ```"
            ]
        );
    }

    #[test]
    fn a_paragraph_starting_with_a_code_span_is_not_a_fence() {
        let got = from_html(
            "<p><code>a``b</code> starts</p><h1>Later</h1><p>body</p>",
            1000,
            0,
        );
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[1].headings, ["Later"]);
    }

    #[test]
    fn the_overlap_never_comes_from_a_fence_or_table() {
        let got = cut(
            "```\ncode code code\n```\n\nword word word word word",
            40,
            12,
        );
        assert!(got.iter().all(|c| !c.text.starts_with("code")), "{got:?}");
        assert_eq!(texts(&got)[1], "word word word word word");
    }

    #[test]
    fn a_paragraph_starting_with_a_pipe_is_not_a_table() {
        let got = from_html(
            "<p>| one<br>| two<br>| three<br>| four four four</p>",
            20,
            0,
        );
        assert!(
            !got.iter()
                .any(|c| c.text.matches("| one").count() > 0 && c.index > 0)
        );
        assert_eq!(
            got.iter().filter(|c| c.text.contains("one")).count(),
            1,
            "{got:?}"
        );
    }

    #[test]
    fn a_heading_that_doesnt_fit_beside_the_one_above_starts_its_own_chunk() {
        let got = cut("# Alpha heading\n\n## Beta heading\n\ntext", 21, 0);
        assert_eq!(texts(&got), ["# Alpha heading", "## Beta heading\n\ntext"]);
        assert_eq!(got[0].headings, ["Alpha heading"]);
        assert_eq!(got[1].headings, ["Alpha heading", "Beta heading"]);
    }

    #[test]
    fn no_chunk_is_only_whitespace() {
        for c in cut("aaaa\t\t\t\t\t\tbbbb", 4, 0) {
            assert!(!c.text.trim().is_empty(), "{:?}", c.text);
        }
    }

    #[test]
    fn nothing_gives_no_chunks() {
        assert!(cut("", 100, 0).is_empty());
        assert!(cut("\n\n  \n", 100, 0).is_empty());
    }

    #[test]
    fn a_big_page_is_cut_in_linear_time() {
        let section = "## Part\n\nSome words in a sentence. Another one here.\n\n- a\n- b\n\n";
        let md = section.repeat(20_000);
        let start = std::time::Instant::now();
        let got = cut(&md, 500, 50);
        assert_eq!(got.len(), 20_000);
        assert!(start.elapsed().as_secs_f64() < 5.0, "{:?}", start.elapsed());
        let long = "word ".repeat(400_000);
        let start = std::time::Instant::now();
        cut(&long, 1000, 100);
        assert!(start.elapsed().as_secs_f64() < 5.0, "{:?}", start.elapsed());
    }
}
