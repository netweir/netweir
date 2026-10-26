//! Finding the part of a page that is its content (the article, the post,
//! the product's description) and reading it as an article.
//!
//! Every block of text on the page is weighed once: prose (a block with
//! enough words that isn't mostly links) counts for its element, links
//! count against it, and text inside boilerplate (navigation, sidebars,
//! comments, share buttons, related links, cookie banners) counts against
//! whatever holds it. The main content is the element whose text is worth
//! the most once what it also holds has been taken off, so a wrapper that
//! adds a sidebar to an article scores less than the article. See
//! docs/design/content.md.

use std::collections::HashSet;

use serde_json::{Map, Value};
use url::Url;

use crate::markdown::{self, Style, image_source, is_block, unseen};
use crate::{Node, NodeId, NodeKind};

/// A page's main content: the element, and the boilerplate inside it that
/// reading it leaves out.
pub struct Main<'a> {
    pub node: Node<'a>,
    pub boilerplate: HashSet<NodeId>,
    /// Found inside `<noscript>`, which reading it then reads too.
    pub noscript: bool,
}

impl Main<'_> {
    pub fn markdown(&self, base: Option<&Url>) -> String {
        markdown::render(
            self.node,
            Style::Markdown,
            base,
            &self.boilerplate,
            self.noscript,
        )
    }

    pub fn clean_text(&self) -> String {
        markdown::render(
            self.node,
            Style::Text,
            None,
            &self.boilerplate,
            self.noscript,
        )
    }

    /// The element's HTML without its boilerplate and without what nobody
    /// sees (scripts, styles, hidden elements). Comments are left out too.
    pub fn html(&self) -> String {
        let mut out = String::new();
        clean_html(self.node, &self.boilerplate, self.noscript, &mut out);
        out
    }

    /// Whether `n`, inside the content, is read: not hidden and not
    /// boilerplate.
    fn reads(&self, n: Node<'_>) -> bool {
        !unseen(n, self.noscript) && !self.boilerplate.contains(&n.id())
    }
}

/// Bytes of prose a main content needs at least: a few sentences. Below
/// it, nothing on the page stands out.
const MIN_PROSE: f32 = 140.0;

/// A block with at least this many bytes of text outside links (and no
/// more link text than other text) is prose.
const PROSE_BLOCK: u32 = 25;

/// Prose blocks shorter than this count for less, in proportion: a lone
/// line of prose in a footer or a sidebar ("All times are GMT", "312
/// users online") is weaker evidence than a paragraph.
const FULL_BLOCK: f32 = 160.0;

/// Class and id words that mark an element as the content.
const CONTENT_WORDS: [&str; 11] = [
    "article",
    "articlebody",
    "body",
    "content",
    "entry",
    "main",
    "markdown",
    "post",
    "prose",
    "story",
    "text",
];

/// Class and id words that mark boilerplate whatever else the element
/// says about itself.
const BOILERPLATE_WORDS: [&str; 39] = [
    "ad",
    "ads",
    "adsbygoogle",
    "advert",
    "advertisement",
    "breadcrumb",
    "breadcrumbs",
    "comment",
    "comments",
    "consent",
    "cookie",
    "cookies",
    "disqus",
    "gdpr",
    "newsletter",
    "outbrain",
    "pager",
    "pagination",
    "popular",
    "promo",
    "recirculation",
    "recommended",
    "related",
    "share",
    "sharing",
    "social",
    "sponsor",
    "sponsored",
    "subscribe",
    "subscription",
    "taboola",
    "toolbar",
    "trending",
    "navbox",
    "interlanguage",
    "paywall",
    "skip",
    "signup",
    "tags",
];

/// Starts of class and id words that mark boilerplate: `relatedposts`,
/// `sharedaddy`, `advertorial`.
const BOILERPLATE_PREFIXES: [&str; 9] = [
    "advert",
    "breadcrumb",
    "cookie",
    "newsletter",
    "promo",
    "recommend",
    "related",
    "share",
    "sponsor",
];

/// Class and id words that mark boilerplate unless the element also has a
/// content word (`content-sidebar-wrap` wraps the content).
const LAYOUT_WORDS: [&str; 16] = [
    "aside",
    "banner",
    "drawer",
    "footer",
    "masthead",
    "menu",
    "modal",
    "nav",
    "navbar",
    "navigation",
    "notify",
    "overlay",
    "popup",
    "sidebar",
    "toast",
    "widget",
];

/// ARIA roles of boilerplate.
const BOILERPLATE_ROLES: [&str; 10] = [
    "alertdialog",
    "banner",
    "complementary",
    "contentinfo",
    "dialog",
    "menu",
    "menubar",
    "navigation",
    "search",
    "toolbar",
];

/// Classes that file a post under a taxonomy (`tag-cookies`,
/// `category-social-media`, as WordPress and Drupal write them) start
/// with one of these. They say what the post is about, not what the
/// element is, so their words don't count.
const TAXONOMY: [&str; 11] = [
    "cat",
    "categories",
    "category",
    "format",
    "label",
    "status",
    "tag",
    "tags",
    "taxonomy",
    "term",
    "topic",
];

/// The words of an element's class and id: each class split at anything
/// that isn't a letter or digit, leaving out taxonomy classes.
pub(crate) fn words<'a>(n: Node<'a>) -> impl Iterator<Item = &'a str> {
    // A custom element's name says what it is too: <s-modal>, <cart-drawer>.
    let custom = n.tag().filter(|t| t.contains('-'));
    [n.attr("class"), n.attr("id"), custom]
        .into_iter()
        .flatten()
        .flat_map(str::split_whitespace)
        .filter(|class| {
            !class.split_once(['-', '_']).is_some_and(|(head, rest)| {
                !rest.is_empty() && TAXONOMY.iter().any(|t| head.eq_ignore_ascii_case(t))
            })
        })
        .flat_map(|class| class.split(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|w| !w.is_empty() && w.len() <= 20)
}

/// The last word of each of an element's classes, its id and its
/// custom element name.
fn heads<'a>(n: Node<'a>) -> impl Iterator<Item = &'a str> {
    let custom = n.tag().filter(|t| t.contains('-'));
    [n.attr("class"), n.attr("id"), custom]
        .into_iter()
        .flatten()
        .flat_map(str::split_whitespace)
        .filter_map(|class| {
            class
                .split(|c: char| !c.is_ascii_alphanumeric())
                .rfind(|w| !w.is_empty())
        })
}

/// What an element's tag, role and class and id words say about it.
#[derive(Default, Clone, Copy)]
struct Signals {
    /// The content's own container: `<main>`, `role="main"` or
    /// `itemprop="articleBody"`.
    main: bool,
    /// Boilerplate by tag (`<nav>`, `<aside>`, `<footer>`) or role.
    boilerplate_tag: bool,
    /// A word from BOILERPLATE_WORDS.
    boilerplate_word: bool,
    /// A word from LAYOUT_WORDS.
    layout_word: bool,
    /// `responses`: a story's replies, or an API's documented responses.
    responses_word: bool,
    content_word: bool,
    /// `<article>`, `role="article"`, or `main`.
    semantic: bool,
    /// One piece of writing (`<article>`, `role="article"`,
    /// `itemprop="articleBody"`), which a listing never is.
    article: bool,
}

fn signals(n: Node<'_>, tag: &str) -> Signals {
    let mut s = Signals::default();
    let role = n.attr("role").unwrap_or_default().trim();
    let itemprop = n.attr("itemprop").unwrap_or_default();
    match tag {
        "main" => s.main = true,
        "article" => s.semantic = true,
        "nav" | "aside" | "footer" => s.boilerplate_tag = true,
        _ => {}
    }
    if role.eq_ignore_ascii_case("main") {
        s.main = true;
    } else if role.eq_ignore_ascii_case("article") {
        s.semantic = true;
    } else if BOILERPLATE_ROLES
        .iter()
        .any(|r| role.eq_ignore_ascii_case(r))
    {
        s.boilerplate_tag = true;
    }
    let body = itemprop
        .split_whitespace()
        .any(|p| p == "articleBody" || p == "reviewBody");
    s.main |= body;
    s.semantic |= s.main;
    s.article = tag == "article" || role.eq_ignore_ascii_case("article") || body;
    // A class's last word names what it is: `article__gift-modal` is a
    // modal, though `overlay-tutorial` is a tutorial.
    if heads(n).any(|w| {
        ["modal", "popup", "overlay", "drawer", "toast", "notify"]
            .iter()
            .any(|x| w.eq_ignore_ascii_case(x))
    }) {
        s.boilerplate_word = true;
    }
    for word in words(n) {
        let is = |list: &[&str]| list.iter().any(|w| word.eq_ignore_ascii_case(w));
        if word.eq_ignore_ascii_case("responses") {
            s.responses_word = true;
        } else if is(&CONTENT_WORDS) {
            s.content_word = true;
        } else if is(&BOILERPLATE_WORDS)
            || BOILERPLATE_PREFIXES.iter().any(|p| {
                word.get(..p.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(p))
            })
        {
            s.boilerplate_word = true;
        } else if is(&LAYOUT_WORDS) {
            s.layout_word = true;
        }
    }
    s
}

impl Signals {
    /// Boilerplate by what the element says about itself alone.
    fn says_boilerplate(&self) -> bool {
        self.boilerplate_tag || self.boilerplate_word || (self.layout_word && !self.content_word)
    }
}

/// Whether the element itself says it's boilerplate (not what's above it).
pub(crate) fn says_boilerplate(n: Node<'_>) -> bool {
    n.tag()
        .is_some_and(|tag| signals(n, tag).says_boilerplate())
}

/// Whether `n` or an element above it says it's boilerplate.
pub(crate) fn in_boilerplate(n: Node<'_>) -> bool {
    std::iter::successors(Some(n), |a| a.parent())
        .filter_map(|a| Some((a, a.tag()?)))
        .take_while(|(_, tag)| !matches!(*tag, "body" | "html"))
        .any(|(a, tag)| signals(a, tag).says_boilerplate())
}

/// One element's tally, below and including itself.
struct Tally<'a> {
    node: Node<'a>,
    parent: usize,
    signals: Signals,
    /// Text not yet inside a closed block, and how much of it is links.
    chars: u32,
    links: u32,
    /// Prose, and text that counts against the element (links, and
    /// everything inside boilerplate).
    good: f32,
    bad: f32,
    /// Prose bytes, unweighted, with boilerplate counted like any other
    /// text.
    raw: f32,
    /// Prose bytes, unweighted, outside boilerplate.
    prose: f32,
    /// All text and all link text, never reset by a block.
    text: u32,
    link_text: u32,
    has_img: bool,
    /// Holds code or a table: documentation, not discussion.
    has_code: bool,
    /// Inside a link: all its text is link text, blocks or not (a card's
    /// headline in a `<p>` inside an `<a>`).
    in_link: bool,
    /// Holds an image that's a link.
    has_img_link: bool,
    /// Children that are cards, and their text and prose.
    card_text: u32,
    card_prose: f32,
    /// Cards anywhere below, outside boilerplate, and their text.
    all_cards: u32,
    all_card_text: u32,
    /// `<article>`s at or below, outside boilerplate, and the one with
    /// the most prose.
    articles: u32,
    top_article: usize,
    /// The child holding the most text.
    top_text_child: usize,
    top_text: u32,
    has_main: bool,
    /// Holds a link that goes somewhere else (not `#fragment`).
    has_link: bool,
    /// Holds a heading with such a link: a card in a listing.
    has_linked_heading: bool,
    /// Is a card: a linked heading, or a linked image with text that's
    /// mostly links (a headline under a thumbnail).
    card: bool,
    /// Children that are cards, and the prose in them.
    cards: u32,
    card_good: f32,
    /// The child holding the most prose.
    top_child: usize,
    top_good: f32,
    boilerplate: bool,
}

const NONE: usize = usize::MAX;

/// The element holding the main content below `scope` (a document or any
/// element), or None when no part of it stands out: too little prose, or
/// prose spread across a listing's cards.
pub fn main_content(scope: Node<'_>) -> Option<Main<'_>> {
    let (found, noscript) = find(scope, false);
    // Pages that build themselves in script (Discourse, say) give crawlers
    // their content inside <noscript>. Read it when nothing else is there.
    // A notice asking for JavaScript isn't content, though it's written
    // as prose too.
    match found {
        None if noscript => find(scope, true).0.filter(|m| !asks_for_javascript(m.node)),
        found => found,
    }
}

/// Whether `n` is short and talks about JavaScript: "JavaScript
/// required, please enable it".
fn asks_for_javascript(n: Node<'_>) -> bool {
    let text = n.text();
    text.len() < 1500 && text.to_ascii_lowercase().contains("javascript")
}

/// Whether `n` is a short notice about scripts not running ("This page
/// displays a fallback because interactive scripts did not run",
/// "Please enable JavaScript"), which is prose but not content.
fn script_notice(n: Node<'_>) -> bool {
    let text = n.text();
    if text.len() >= 1000 {
        return false;
    }
    let text = text.to_lowercase();
    // Whole words: "description" isn't about scripts.
    text.split(|c: char| !c.is_alphanumeric())
        .any(|w| matches!(w, "javascript" | "script" | "scripts"))
        && [
            "enable",
            "disabled",
            "required",
            "requires",
            "turn on",
            "did not run",
            "didn't run",
            "not supported",
            "aktivieren",
            "activer",
            "activez",
            "habilita",
            "attiva",
        ]
        .iter()
        .any(|w| text.contains(w))
}

/// Bytes of text a `<noscript>` needs before it's read as the page: more
/// than a notice, a few paragraphs.
const NOSCRIPT_PROSE: usize = 500;

/// The main content, reading `<noscript>` or not; and whether a
/// `<noscript>` holding elements was passed over.
fn find(scope: Node<'_>, noscript: bool) -> (Option<Main<'_>>, bool) {
    let (mut all, skipped) = tally(scope, noscript);
    weigh(&mut all);
    (choose(&all, noscript), skipped)
}

fn choose<'a>(all: &[Tally<'a>], noscript: bool) -> Option<Main<'a>> {
    // Elements inside boilerplate can't be the content.
    let mut below_boilerplate = vec![false; all.len()];
    let mut best: Option<(usize, f32)> = None;
    for i in 0..all.len() {
        let t = &all[i];
        let below = t.parent != NONE && (all[t.parent].boilerplate || below_boilerplate[t.parent]);
        below_boilerplate[i] = below;
        if t.boilerplate
            || below
            || t.node.kind() != NodeKind::Element
            || t.node.tag() == Some("noscript")
        {
            continue;
        }
        let score = t.good - t.bad;
        // Later wins a tie: a descendant scoring the same is the tighter fit.
        if best.is_none_or(|(_, s)| score >= s) {
            best = Some((i, score));
        }
    }
    let (mut best, score) = best?;
    // The content should hold most of the prose around it. A post with a
    // long list of links in it (release notes, an API index) can score
    // below one of its own paragraphs; when the winner holds well under
    // half of its parent's prose, the parent is the content. The page's
    // <body> is only climbed to from a lone block.
    while !all[best].signals.semantic {
        let parent = all[best].parent;
        if parent == NONE || all[parent].node.kind() != NodeKind::Element {
            break;
        }
        let lone = matches!(
            all[best].node.tag(),
            Some("p" | "li" | "dd" | "td" | "blockquote" | "pre")
        );
        if matches!(all[parent].node.tag(), Some("body" | "html")) && !lone {
            break;
        }
        if all[best].prose * 2.0 >= all[parent].prose {
            break;
        }
        best = parent;
    }
    // An <article> or <main> around the best element, holding little else
    // (a header with links, say), is the content's own container: take it,
    // title and all.
    let mut at = match all[best].signals.semantic {
        true => NONE,
        false => all[best].parent,
    };
    // What it may add is a header's worth: a tenth of the score, and no
    // more than a few hundred bytes of links however long the article.
    let floor = score - (0.1 * score).min(300.0);
    while at != NONE {
        // Nor may it add a row of cards: a product's <main> that also
        // holds recommendations isn't the product.
        let (a, b) = (&all[at], &all[best]);
        let cards = a.all_cards.saturating_sub(b.all_cards) >= 3
            && a.all_card_text.saturating_sub(b.all_card_text) * 2 >= a.text.saturating_sub(b.text);
        if a.signals.semantic && a.good - a.bad >= floor && !cards {
            best = at;
            break;
        }
        at = all[at].parent;
    }
    // A winner holding one <article> and a lot of other prose (replies
    // with no class to say so) is the article's page; the article is the
    // content.
    let t = &all[best];
    if !t.signals.semantic
        && t.articles == 1
        && t.top_article != NONE
        && all[t.top_article].prose >= MIN_PROSE
        && all[t.top_article].prose * 5.0 >= t.prose
    {
        best = t.top_article;
    }
    // On a page of cards (a homepage, a shop's collection), a winner with
    // a small share of the page's prose is a stray note, not the content;
    // so is one with under half the prose and a sliver of the text (a
    // collection's blurb under a grid of products).
    let cards: u32 = all
        .iter()
        .zip(&below_boilerplate)
        .filter(|(t, below)| !t.boilerplate && !**below)
        .map(|(t, _)| t.cards)
        .sum();
    let (b, root) = (&all[best], &all[0]);
    let crowded = cards >= 10
        && (b.prose * 5.0 < root.prose || (b.prose * 2.0 < root.prose && b.text * 5 < root.text));
    if score <= 0.0
        || all[best].raw < MIN_PROSE
        || script_notice(all[best].node)
        || crowded
        || beside_grid(all, best)
        || (!within_article(all, best) && is_listing(all, best))
    {
        return None;
    }
    let mut inside = vec![false; all.len()];
    let mut boilerplate = HashSet::new();
    for i in best..all.len() {
        let parent = all[i].parent;
        inside[i] = i == best || (parent != NONE && inside[parent]);
        if inside[i] && all[i].boilerplate {
            boilerplate.insert(all[i].node.id());
        }
    }
    Some(Main {
        node: all[best].node,
        boilerplate,
        noscript,
    })
}

/// Every element a reader can see below `scope`, parents before children,
/// with its text counted into blocks.
fn tally(scope: Node<'_>, noscript: bool) -> (Vec<Tally<'_>>, bool) {
    let mut all: Vec<Tally<'_>> = Vec::new();
    let mut skipped = false;
    let mut stack = vec![(scope, NONE)];
    while let Some((n, parent)) = stack.pop() {
        match n.kind() {
            NodeKind::Text if parent != NONE => {
                // Bytes rather than characters: faster to count, and it
                // weighs a CJK character, which says as much as a word, as
                // three.
                let data = n.data().unwrap_or_default().as_bytes();
                let count = data.iter().filter(|b| !b.is_ascii_whitespace()).count() as u32;
                let t = &mut all[parent];
                t.chars = t.chars.saturating_add(count);
                t.text = t.text.saturating_add(count);
            }
            NodeKind::Element | NodeKind::Document => {
                if unseen(n, noscript) {
                    // Worth a second look only if it holds some prose.
                    skipped |= n.tag() == Some("noscript")
                        && n.children().any(|c| c.kind() == NodeKind::Element)
                        && n.text().split_whitespace().map(str::len).sum::<usize>()
                            >= NOSCRIPT_PROSE;
                    continue;
                }
                let i = all.len();
                let tag = n.tag().unwrap_or_default();
                let in_link = parent != NONE && {
                    let p = &all[parent];
                    p.in_link || (p.node.tag() == Some("a") && p.node.attr("href").is_some())
                };
                all.push(Tally {
                    node: n,
                    parent,
                    signals: signals(n, tag),
                    chars: 0,
                    links: 0,
                    good: 0.0,
                    bad: 0.0,
                    raw: 0.0,
                    prose: 0.0,
                    text: 0,
                    link_text: 0,
                    has_img: tag == "img",
                    has_code: matches!(tag, "pre" | "code" | "table" | "samp"),
                    in_link,
                    has_img_link: false,
                    card_text: 0,
                    card_prose: 0.0,
                    all_cards: 0,
                    all_card_text: 0,
                    articles: 0,
                    top_article: NONE,
                    top_text_child: NONE,
                    top_text: 0,
                    has_main: false,
                    has_link: false,
                    has_linked_heading: false,
                    card: false,
                    cards: 0,
                    card_good: 0.0,
                    top_child: NONE,
                    top_good: 0.0,
                    boilerplate: false,
                });
                for child in n.children() {
                    stack.push((child, i));
                }
            }
            _ => {}
        }
    }
    (all, skipped)
}

/// Adds each element's text up into its ancestors, children first,
/// closing blocks as it goes and deciding what's boilerplate.
fn weigh(all: &mut [Tally<'_>]) {
    // What's boilerplate depends on how much of the page an element holds,
    // so the prose is counted once before anything is.
    for i in (0..all.len()).rev() {
        close_block(&mut all[i]);
        let t = &all[i];
        let (parent, raw, chars, links, text, link_text, has_img, has_code) = (
            t.parent,
            t.raw,
            t.chars,
            t.links,
            t.text,
            t.link_text,
            t.has_img,
            t.has_code,
        );
        if parent != NONE {
            let p = &mut all[parent];
            p.raw += raw;
            p.chars = p.chars.saturating_add(chars);
            p.links = p.links.saturating_add(links);
            p.text = p.text.saturating_add(text);
            p.link_text = p.link_text.saturating_add(link_text);
            p.has_img |= has_img;
            p.has_code |= has_code;
        }
    }
    let total = all.first().map_or(0.0, |t| t.raw);
    for i in (0..all.len()).rev() {
        let t = &mut all[i];
        let tag = t.node.tag().unwrap_or_default();
        let s = t.signals;
        t.has_main |= s.main;
        if matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") && t.has_link {
            t.has_linked_heading = true;
        }
        // Something holding cards is a grid of them, not a card.
        t.card = t.cards < 2
            && (t.has_linked_heading
                || (t.has_img_link
                    && t.text > 0
                    && t.text <= 300
                    && (t.link_text * 2 >= t.text || (t.text <= 200 && priced(t.node)))));
        // A wrapper holding nearly everything is the content's, whatever
        // its class says (`widget-area-group`, `overlay-tutorial`): nine
        // tenths of the page's prose, or three quarters with a content
        // word too (`with-comments`, `has-sidebar`).
        let holds = t.raw >= 0.9 * total || (s.content_word && t.raw >= 0.75 * total);
        // An API's responses come with code and tables; a story's don't.
        let responses = s.responses_word && !t.has_code;
        t.boilerplate = t.parent != NONE
            && !matches!(tag, "html" | "body")
            && !t.has_main
            && !holds
            && (s.boilerplate_tag
                // An <article> that names itself a post too
                // (`newsletter-post post`) is one, whatever else it says.
                || ((s.boilerplate_word || responses) && !(s.article && s.content_word))
                || (s.layout_word && !s.content_word));
        // An article holds at least the prose of any article inside it.
        if t.signals.article && !t.boilerplate {
            t.articles += 1;
            t.top_article = i;
        }
        let parent = t.parent;
        if parent == NONE {
            continue;
        }
        let (good, bad, prose) = match t.boilerplate {
            true => (0.0, t.good + t.bad, 0.0),
            // A small card's name and blurb aren't the page's writing: a
            // product page's recommendations shouldn't pull the content
            // out to the wrapper holding both.
            false if t.card && t.text <= 200 => (0.0, t.bad, t.prose),
            false => (t.good, t.bad, t.prose),
        };
        let kept = !t.boilerplate;
        let (articles, top_article) = match kept {
            true => (t.articles, t.top_article),
            false => (0, NONE),
        };
        let (has_main, has_link, linked_heading, img_link, card, text) = (
            t.has_main,
            t.has_link && kept,
            t.has_linked_heading && kept,
            t.has_img_link && kept,
            t.card && kept,
            if kept { t.text } else { 0 },
        );
        // A card counts once, however many cards it holds.
        let (all_cards, all_card_text) = match (kept, card) {
            (false, _) => (0, 0),
            (true, true) => (1, t.text),
            (true, false) => (t.all_cards, t.all_card_text),
        };
        let better = top_article != NONE
            && (all[parent].top_article == NONE
                || all[all[parent].top_article].prose < all[top_article].prose);
        let p = &mut all[parent];
        p.articles += articles;
        p.all_cards += all_cards;
        p.all_card_text = p.all_card_text.saturating_add(all_card_text);
        if better {
            p.top_article = top_article;
        }
        p.good += good;
        p.bad += bad;
        p.prose += prose;
        p.has_main |= has_main;
        p.has_link |= has_link;
        p.has_linked_heading |= linked_heading;
        p.has_img_link |= img_link;
        if card {
            p.cards += 1;
            p.card_good += good;
            p.card_text = p.card_text.saturating_add(text);
            p.card_prose += prose;
        }
        if good > p.top_good {
            p.top_good = good;
            p.top_child = i;
        }
        if text > p.top_text {
            p.top_text = text;
            p.top_text_child = i;
        }
    }
}

/// Whether `at` is an `<article>` or inside one: one piece of writing,
/// whose sections (an episode's acts, a guide's picks) aren't a listing
/// even when each links somewhere.
fn within_article(all: &[Tally<'_>], mut at: usize) -> bool {
    while at != NONE {
        if all[at].signals.article {
            return true;
        }
        at = all[at].parent;
    }
    false
}

/// Whether the winner is a note beside a grid: some element above it
/// has, as the child holding most of its text, a grid of six or more
/// cards with more text than the winner (a collection's blurb next to
/// its products, small print on a homepage).
fn beside_grid(all: &[Tally<'_>], best: usize) -> bool {
    let mut chain = vec![best];
    let mut at = all[best].parent;
    while at != NONE {
        let c = all[at].top_text_child;
        if c != NONE && !chain.contains(&c) {
            let g = &all[c];
            if g.cards >= 6 && g.card_text * 2 >= g.text && g.text >= all[best].text {
                return true;
            }
        }
        chain.push(at);
        at = all[at].parent;
    }
    false
}

/// Whether a small card's text has a price in it (`$255.00`, `12,99 €`):
/// a product card.
fn priced(n: Node<'_>) -> bool {
    n.descendants().any(|d| {
        d.kind() == NodeKind::Text
            && d.data()
                .and_then(crate::price::parse_price)
                .is_some_and(|p| p.currency.is_some())
    })
}

/// Counts the text gathered so far into the element: a link's text is
/// all link text, and a block's text is prose or not.
fn close_block(t: &mut Tally<'_>) {
    let tag = t.node.tag().unwrap_or_default();
    if tag == "a"
        && let Some(href) = t.node.attr("href")
    {
        t.links = t.chars;
        t.link_text = t.text;
        if !href.trim().starts_with('#') {
            t.has_link = true;
            t.has_img_link = t.has_img;
        }
    }
    let block = t.node.kind() == NodeKind::Document || is_block(tag);
    if !block || t.chars == 0 {
        return;
    }
    let links = if t.in_link { t.chars } else { t.links };
    let (chars, links) = (t.chars, links.min(t.chars));
    let own = chars - links;
    let heading = matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6");
    // Headings are content however short; other short blocks (dates,
    // labels, "Share") are neither.
    if heading || (own >= PROSE_BLOCK && links <= own) {
        let own = own as f32;
        let weight = if heading {
            1.0
        } else {
            (own / FULL_BLOCK).min(1.0)
        };
        t.good += own * weight;
        t.raw += own;
        t.prose += own;
    }
    t.bad += links as f32;
    t.chars = 0;
    t.links = 0;
}

/// Whether the best element, or the child below it that holds more than
/// half its text (or else 80% of its prose), is a listing: three or more cards holding
/// at least half its prose or half its text. A card is a child with a
/// heading that links elsewhere, or with a linked image and text that's
/// mostly links.
///
/// Known limit: a listing whose cards have neither looks like content,
/// and is returned.
fn is_listing(all: &[Tally<'_>], mut at: usize) -> bool {
    loop {
        let t = &all[at];
        // Cards of a listing are short; sections of one page that each
        // link somewhere (an episode's acts) hold paragraphs.
        let short = t.card_prose < 300.0 * t.cards as f32;
        if t.cards >= 3 && short && (t.card_good * 2.0 >= t.good || t.card_text * 2 >= t.text) {
            return true;
        }
        at = match (t.top_child, t.top_text_child) {
            (_, c) if c != NONE && all[c].text * 2 > t.text => c,
            (c, _) if c != NONE && all[c].good >= 0.8 * t.good && t.good > 0.0 => c,
            _ => return false,
        };
    }
}

/// An HTML fragment (a description from structured data, say) cleaned
/// as the main content's HTML is: no scripts, styles or hidden elements,
/// no comments, nothing that runs.
pub fn clean_html_fragment(html: &str) -> String {
    let doc = crate::Document::parse(html);
    let mut out = String::new();
    let body = doc.root().descendants().find(|n| n.tag() == Some("body"));
    for child in body.into_iter().flat_map(|b| b.children()) {
        clean_html(child, &HashSet::new(), false, &mut out);
    }
    out.trim().to_string()
}

/// `node` as HTML, leaving out `skip`, what nobody sees, and comments.
fn clean_html(node: Node<'_>, skip: &HashSet<NodeId>, noscript: bool, out: &mut String) {
    const VOID: [&str; 13] = [
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source",
        "track", "wbr",
    ];
    stacker::maybe_grow(64 * 1024, 1024 * 1024, || match node.kind() {
        NodeKind::Text => escape_html(node.data().unwrap_or_default(), false, out),
        NodeKind::Element => {
            if unseen(node, noscript) || skip.contains(&node.id()) {
                return;
            }
            let tag = node.tag().unwrap_or_default();
            out.push('<');
            out.push_str(tag);
            for (name, value) in node.attrs() {
                // Cleaned HTML is for showing or storing, so nothing in it
                // runs: no event handlers, no javascript: links.
                let scripted = name.len() > 2
                    && name.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("on"))
                    || value
                        .trim_start()
                        .get(..11)
                        .is_some_and(|v| v.eq_ignore_ascii_case("javascript:"));
                if scripted {
                    continue;
                }
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                escape_html(value, true, out);
                out.push('"');
            }
            out.push('>');
            if VOID.contains(&tag) {
                return;
            }
            for child in node.children() {
                clean_html(child, skip, noscript, out);
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        NodeKind::Document => {
            for child in node.children() {
                clean_html(child, skip, noscript, out);
            }
        }
        _ => {}
    })
}

fn escape_html(text: &str, attribute: bool, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' if !attribute => out.push_str("&lt;"),
            '>' if !attribute => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// What a page says about the article on it. Fields are None when the
/// page doesn't say.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Article {
    pub title: Option<String>,
    pub byline: Option<String>,
    /// The publication date as the page writes it.
    pub published: Option<String>,
    /// The lead image, absolute against the base.
    pub image: Option<String>,
    pub language: Option<String>,
    pub text: Option<String>,
    pub html: Option<String>,
}

/// The article on the page at `root`: its main content, and the title,
/// byline, date, lead image and language the page gives for it, from
/// its structured data first and its HTML after.
pub fn article(root: Node<'_>, base: Option<&Url>) -> Article {
    let main = main_content(root);
    let meta = crate::metadata::metadata(root, base);
    let ld = linked_data(&meta);
    let elements: Vec<Node<'_>> = root
        .descendants()
        .filter(|n| n.kind() == NodeKind::Element)
        .collect();
    let og = |key: &str| first_string(&meta["opengraph"][key]);
    let tw = |key: &str| first_string(&meta["twitter"][key]);
    let meta_named = |names: &[&str]| {
        elements
            .iter()
            .filter(|n| n.tag() == Some("meta"))
            .find_map(|n| {
                let key = n
                    .attr("name")
                    .or_else(|| n.attr("property"))
                    .or_else(|| n.attr("itemprop"))
                    .or_else(|| n.attr("http-equiv"))?;
                names
                    .iter()
                    .any(|k| key.trim().eq_ignore_ascii_case(k))
                    .then(|| tidy(n.attr("content")?))
                    .flatten()
            })
    };
    let found = main.as_ref().map(Facts::of).unwrap_or_default();
    let og_title = og("og:title");

    // What the reader sees comes first: the content's <h1>, or the page's
    // when the content has none (an encyclopedia puts it above the text).
    // A JSON-LD headline can be a summary rather than the title, which is
    // why it doesn't outrank them; it still comes before og:title, which
    // is often rewritten for sharing.
    // og:title naming one of the content's headings (a post titled in an
    // <h2>, a title with a "Title:" label before it) is the title as the
    // site means it.
    let og_heading = og_title.clone().filter(|og| {
        found
            .headings
            .iter()
            .any(|h| h == og || (h.len() > og.len() && h.ends_with(og.as_str())))
    });
    let title = og_heading
        .or(found.heading)
        .or_else(|| elements.iter().find_map(|n| page_heading(*n)))
        .or_else(|| ld_field(&ld, "headline").map(|t| unescape(&t)))
        .or(og_title.map(|t| unescape(&t)))
        .or_else(|| tw("twitter:title"))
        .or_else(|| {
            elements
                .iter()
                .find(|n| n.tag() == Some("title"))
                .and_then(|n| tidy(&n.text()))
        });

    // A person's name before an organisation's: a JSON-LD author that's
    // an Organization is often the publication, and the <meta> author or
    // the byline names the writer.
    let (people, organisations) = authors(&ld);
    let byline = people
        .or_else(|| {
            meta_named(&["author", "article:author", "dc.creator"])
                .filter(|a| !a.starts_with("http") && a.chars().any(char::is_alphabetic))
        })
        .or(found.author)
        .or_else(|| {
            elements
                .iter()
                .find_map(|n| author_of(*n).filter(|_| !in_boilerplate(*n)))
        })
        .or(organisations);

    let published = ld_field(&ld, "datePublished")
        .or_else(|| og("article:published_time"))
        .or_else(|| {
            meta_named(&[
                "datePublished",
                "date",
                "pubdate",
                "publishdate",
                "dc.date",
                "dcterms.date",
                "dcterms.created",
            ])
        })
        .or_else(|| {
            elements
                .iter()
                .find(|n| n.attr("itemprop") == Some("datePublished"))
                .and_then(|n| {
                    tidy(
                        n.attr("datetime")
                            .or_else(|| n.attr("content"))
                            .unwrap_or(&n.text()),
                    )
                })
        })
        .or(found.time);

    let absolute = |url: String| match base.and_then(|b| b.join(&url).ok()) {
        Some(u) => u.to_string(),
        None => url,
    };
    let image = og("og:image")
        .or_else(|| og("og:image:url"))
        .or_else(|| tw("twitter:image"))
        .or_else(|| ld.iter().find_map(|item| image_url(item.get("image")?)))
        .or(found.image)
        .map(absolute);

    let language = elements
        .iter()
        .find(|n| n.tag() == Some("html"))
        .and_then(|n| tidy(n.attr("lang")?))
        .or_else(|| meta_named(&["content-language"]))
        .or_else(|| og("og:locale").map(|l| l.replace('_', "-")))
        .or_else(|| ld_field(&ld, "inLanguage"));

    Article {
        title,
        byline,
        published,
        image,
        language,
        text: main
            .as_ref()
            .map(Main::clean_text)
            .filter(|t| !t.is_empty()),
        html: main.as_ref().map(Main::html),
    }
}

/// Every JSON-LD object on the page, with the members of an `@graph`
/// as objects of their own.
fn linked_data(meta: &Value) -> Vec<&Map<String, Value>> {
    let mut out = Vec::new();
    for item in meta["json_ld"].as_array().into_iter().flatten() {
        if let Some(obj) = item.as_object() {
            out.push(obj);
            for member in obj
                .get("@graph")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                out.extend(member.as_object());
            }
        }
    }
    out
}

/// The first JSON-LD object's `key`, as a string.
fn ld_field(ld: &[&Map<String, Value>], key: &str) -> Option<String> {
    ld.iter().find_map(|item| first_string(item.get(key)?))
}

fn first_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => tidy(s),
        Value::Array(list) => list.iter().find_map(first_string),
        _ => None,
    }
}

/// The first JSON-LD author's names, as people (a name, or a Person)
/// and as organisations, each joined with commas.
fn authors(ld: &[&Map<String, Value>]) -> (Option<String>, Option<String>) {
    fn collect(value: &Value, people: &mut Vec<String>, orgs: &mut Vec<String>) {
        match value {
            Value::Array(list) => list.iter().for_each(|v| collect(v, people, orgs)),
            Value::Object(obj) => {
                let org = obj
                    .get("@type")
                    .and_then(first_string)
                    .is_some_and(|t| !t.eq_ignore_ascii_case("Person"));
                let list = if org { orgs } else { people };
                list.extend(obj.get("name").and_then(first_string));
            }
            other => people.extend(first_string(other)),
        }
    }
    let joined = |v: Vec<String>| (!v.is_empty()).then(|| v.join(", "));
    for item in ld {
        if let Some(author) = item.get("author") {
            let (mut people, mut orgs) = (Vec::new(), Vec::new());
            collect(author, &mut people, &mut orgs);
            if !people.is_empty() || !orgs.is_empty() {
                return (joined(people), joined(orgs));
            }
        }
    }
    (None, None)
}

/// What the main content itself gives: its first heading, byline, date
/// and lead image, found in one walk that skips its boilerplate.
#[derive(Default)]
struct Facts {
    heading: Option<String>,
    /// The text of its first few headings of any level.
    headings: Vec<String>,
    author: Option<String>,
    time: Option<String>,
    image: Option<String>,
}

impl Facts {
    fn of(main: &Main<'_>) -> Facts {
        let mut f = Facts::default();
        let mut stack: Vec<Node<'_>> = main.node.children().collect();
        stack.reverse();
        while let Some(n) = stack.pop() {
            if n.kind() != NodeKind::Element || !main.reads(n) {
                continue;
            }
            match n.tag() {
                Some("h1") if f.heading.is_none() => {
                    f.heading = heading_text(n).filter(|h| !is_breadcrumb(h))
                }
                Some("h2" | "h3") if f.headings.len() < 8 => f.headings.extend(heading_text(n)),
                Some("time") if f.time.is_none() => f.time = n.attr("datetime").and_then(tidy),
                Some("img") if f.image.is_none() => f.image = lead_image(n),
                _ => {}
            }
            if f.author.is_none() {
                f.author = author_of(n);
            }
            if let Some(h) = &f.heading
                && f.headings.len() < 8
                && n.tag() == Some("h1")
            {
                f.headings.push(h.clone());
            }
            if f.heading.is_some()
                && f.author.is_some()
                && f.time.is_some()
                && f.image.is_some()
                && f.headings.len() >= 8
            {
                break;
            }
            let at = stack.len();
            stack.extend(n.children());
            stack[at..].reverse();
        }
        f
    }
}

/// A heading's text as a reader sees it: without permalinks, edit links
/// or anything hidden.
fn heading_text(n: Node<'_>) -> Option<String> {
    tidy(&markdown::clean_text(n))
}

/// Whether a heading is a trail of sections ("Computer Science >
/// Computation and Language") rather than a title.
fn is_breadcrumb(text: &str) -> bool {
    [" > ", " › ", " » ", " / "]
        .iter()
        .any(|s| text.contains(s))
}

/// The page's title as an `<h1>` outside its boilerplate, unless it's a
/// link (a logo, most often), the site's name in its header (an `<h1>`
/// in a banner, or a `<header>` with navigation, that isn't inside an
/// `<article>` or `<main>`, or with a class or id word `site`, `logo` or `brand`), a
/// breadcrumb trail, or inside `<noscript>`.
pub(crate) fn page_heading(n: Node<'_>) -> Option<String> {
    if n.tag() != Some("h1") || in_boilerplate(n) {
        return None;
    }
    if words(n).any(|w| {
        ["site", "logo", "brand"]
            .iter()
            .any(|s| w.eq_ignore_ascii_case(s))
    }) {
        return None;
    }
    let (mut header, mut content) = (false, false);
    for a in std::iter::successors(n.parent(), |a| a.parent()) {
        match a.tag() {
            Some("article" | "main") => content = true,
            // A site's header has its navigation in it; an article's
            // header has its title.
            Some("header") if a.descendants().any(|d| d.tag() == Some("nav")) => header = true,
            Some("noscript") => return None,
            _ if a
                .attr("role")
                .is_some_and(|r| r.trim().eq_ignore_ascii_case("banner")) =>
            {
                header = true;
            }
            _ => {}
        }
    }
    if header && !content {
        return None;
    }
    let text = heading_text(n)?;
    let linked: usize = n
        .descendants()
        .filter(|d| d.tag() == Some("a"))
        .map(|a| a.text().trim().len())
        .sum();
    (linked * 2 < text.len() && !is_breadcrumb(&text)).then_some(text)
}

fn image_url(value: &Value) -> Option<String> {
    match value {
        Value::Object(obj) => obj
            .get("url")
            .or_else(|| obj.get("contentUrl"))
            .and_then(first_string),
        Value::Array(list) => list.iter().find_map(image_url),
        other => first_string(other),
    }
}

/// A byline in the HTML: an element marked as the author (`rel=author`,
/// `itemprop=author`, or the class or id word `author` or `byline`),
/// short enough to be a name, with a leading "By" taken off. A
/// commenter (`comment-author`) isn't one.
pub(crate) fn author_of(n: Node<'_>) -> Option<String> {
    let marked = n
        .attr("rel")
        .is_some_and(|r| r.split_whitespace().any(|w| w == "author"))
        || n.attr("itemprop")
            .is_some_and(|p| p.split_whitespace().any(|w| w == "author"))
        || words(n).any(|w| w.eq_ignore_ascii_case("author") || w.eq_ignore_ascii_case("byline"));
    if marked && words(n).any(|w| w.eq_ignore_ascii_case("comment")) {
        return None;
    }
    if !marked {
        return None;
    }
    let text = tidy(&n.text())?;
    let text = ["By ", "by ", "BY "]
        .iter()
        .find_map(|p| text.strip_prefix(p))
        .map_or(text.as_str(), str::trim);
    // "Ana Ruiz · 5 min read": the name, not what follows it.
    let text = [" · ", " • ", " — ", " – ", " - "]
        .iter()
        .filter_map(|sep| text.find(sep))
        .min()
        .map_or(text, |at| &text[..at]);
    let text = text.trim_end_matches([',', ';', ':', '·', '|', '-', ' ']);
    // A name, perhaps with a role; not a sentence about the author.
    let name = !text.is_empty() && text.chars().count() <= 60 && !text.contains(". ");
    name.then(|| text.to_string())
}

/// An image in the content that could lead it: not an avatar, an icon or
/// a logo, and not marked smaller than 100 pixels.
pub(crate) fn lead_image(n: Node<'_>) -> Option<String> {
    if n.tag() != Some("img") {
        return None;
    }
    let small = ["width", "height"].iter().any(|a| {
        n.attr(a)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .is_some_and(|v| v < 100)
    });
    let decoration = [n.attr("class"), n.attr("alt"), n.attr("src")]
        .into_iter()
        .flatten()
        .flat_map(|v| v.split(|c: char| !c.is_ascii_alphanumeric()))
        .any(|w| {
            [
                "avatar", "avatars", "icon", "logo", "emoji", "gravatar", "spacer",
            ]
            .iter()
            .any(|d| w.eq_ignore_ascii_case(d))
        });
    match small || decoration {
        true => None,
        false => tidy(image_source(n)?),
    }
}

/// `text` with the entities a page escaped twice decoded (a JSON-LD
/// headline with `&apos;` in it).
fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    [
        ("&apos;", "'"),
        ("&#39;", "'"),
        ("&quot;", "\""),
        ("&#34;", "\""),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&amp;", "&"),
    ]
    .iter()
    .fold(text.to_string(), |t, (from, to)| t.replace(from, to))
}

/// `text` with its whitespace collapsed, or None when that leaves nothing.
fn tidy(text: &str) -> Option<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    (!words.is_empty()).then(|| words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;

    fn main_tag(html: &str) -> Option<String> {
        let doc = Document::parse(html);
        main_content(doc.root()).map(|m| {
            let n = m.node;
            format!(
                "{}{}",
                n.tag().unwrap_or_default(),
                n.attr("id").map(|i| format!("#{i}")).unwrap_or_default()
            )
        })
    }

    const PARA: &str = "<p>The river rose through the night, and by morning the lower town \
                        was under a foot of water, with the bridge closed to traffic.</p>";

    #[test]
    fn cleaned_html_has_no_scripts_or_handlers() {
        let html = clean_html_fragment(
            r#"<p onclick="x()">Hi <a href="javascript:go()">there</a> <a href="/ok" ONMOUSEOVER="y()">ok</a></p><script>z()</script>"#,
        );
        assert_eq!(html, r#"<p>Hi <a>there</a> <a href="/ok">ok</a></p>"#);
        // Attribute names needn't be ASCII.
        assert_eq!(
            clean_html_fragment(r#"<p aéb="1" oné="2">x</p>"#),
            r#"<p aéb="1">x</p>"#
        );
    }

    #[test]
    fn the_article_wins_over_the_page_around_it() {
        let html = format!(
            "<nav><a href=/>Home</a> <a href=/news>News</a></nav>\
             <div id=wrap><article id=story><h1>Flood</h1>{PARA}{PARA}{PARA}</article>\
             <aside>{PARA}</aside></div><footer>{PARA}</footer>"
        );
        assert_eq!(main_tag(&html).as_deref(), Some("article#story"));
    }

    #[test]
    fn plain_divs_are_found_by_their_text() {
        let links = "<a href=/a>One link</a> <a href=/b>Another link</a> ".repeat(10);
        let html = format!(
            "<div id=top>{links}</div><div id=x><div id=text>{PARA}{PARA}</div></div>\
             <div id=bottom>{links}</div>"
        );
        assert_eq!(main_tag(&html).as_deref(), Some("div#text"));
    }

    #[test]
    fn comments_are_not_the_content_even_when_longer() {
        let html = format!(
            "<div id=post>{PARA}{PARA}</div><section id=comments>{}</section>",
            PARA.repeat(6)
        );
        assert_eq!(main_tag(&html).as_deref(), Some("div#post"));
    }

    #[test]
    fn a_wrapper_named_for_its_sidebar_still_holds_the_content() {
        let html = format!("<div class='content-sidebar-wrap'><div id=a>{PARA}{PARA}</div></div>");
        assert_eq!(main_tag(&html).as_deref(), Some("div#a"));
        let html = format!("<div id=content class='with-comments'><p>x</p>{PARA}{PARA}</div>");
        assert_eq!(main_tag(&html).as_deref(), Some("div#content"));
    }

    #[test]
    fn a_listing_or_a_page_without_prose_has_no_main_content() {
        let card = format!("<li><h3><a href=/p>Lamp</a></h3>{PARA}</li>");
        assert_eq!(
            main_tag(&format!("<main><ul>{}</ul></main>", card.repeat(5))),
            None
        );
        assert_eq!(main_tag("<p>Hello</p><a href=/x>x</a>"), None);
        assert_eq!(main_tag(""), None);
    }

    #[test]
    fn docs_sections_with_permalinked_headings_are_not_a_listing() {
        let section = format!("<section><h2><a href='#s'>¶</a> Setup</h2>{PARA}</section>");
        let html = format!("<div id=doc>{}</div>", section.repeat(4));
        assert_eq!(main_tag(&html).as_deref(), Some("div#doc"));
    }

    #[test]
    fn boilerplate_inside_the_content_is_left_out_when_reading_it() {
        let html = format!(
            "<article><h1>Flood</h1>{PARA}<div class=share-buttons>Share on X</div>\
             <!-- c --><script>x()</script>{PARA}<div class=related><a href=/r>Related</a></div></article>"
        );
        let doc = Document::parse(&html);
        let main = main_content(doc.root()).unwrap();
        let text = main.clean_text();
        assert!(text.starts_with("Flood\n\nThe river rose"));
        assert!(!text.contains("Share") && !text.contains("Related"));
        let html = main.html();
        assert!(html.starts_with("<article><h1>Flood</h1><p>"), "{html}");
        assert!(!html.contains("Share") && !html.contains("<script") && !html.contains("<!--"));
    }

    #[test]
    fn an_article_reads_its_fields_from_structured_data_first() {
        let html = format!(
            r#"<html lang="en-GB"><head><title>Flood | Paper</title>
            <meta property="og:image" content="/lead.jpg">
            <script type="application/ld+json">{{"@graph": [{{"@type": "NewsArticle",
              "headline": "Town flooded", "datePublished": "2026-10-12T09:30:00Z",
              "author": [{{"@type": "Person", "name": "Ada Lovelace"}}, "Bob"]}}]}}</script>
            </head><body><article><h1>Flood</h1>{PARA}{PARA}</article></body></html>"#
        );
        let doc = Document::parse(&html);
        let base = Url::parse("https://paper.example/2026/flood").unwrap();
        let a = article(doc.root(), Some(&base));
        // The visible <h1> outranks the JSON-LD headline.
        assert_eq!(a.title.as_deref(), Some("Flood"));
        assert_eq!(a.byline.as_deref(), Some("Ada Lovelace, Bob"));
        assert_eq!(a.published.as_deref(), Some("2026-10-12T09:30:00Z"));
        assert_eq!(a.image.as_deref(), Some("https://paper.example/lead.jpg"));
        assert_eq!(a.language.as_deref(), Some("en-GB"));
        assert!(a.text.unwrap().starts_with("Flood\n\nThe river"));
    }

    #[test]
    fn an_article_falls_back_to_the_html() {
        let html = format!(
            r#"<title>Page</title><div class=post><h1> The   flood </h1>
            <p class=byline>By <a href=/ada rel=author>Ada</a></p>
            <time datetime="2026-10-12">12 Oct</time><img src="/a.jpg">{PARA}{PARA}</div>"#
        );
        let doc = Document::parse(&html);
        let a = article(doc.root(), None);
        assert_eq!(a.title.as_deref(), Some("The flood"));
        assert_eq!(a.byline.as_deref(), Some("Ada"));
        assert_eq!(a.published.as_deref(), Some("2026-10-12"));
        assert_eq!(a.image.as_deref(), Some("/a.jpg"));
        assert_eq!(a.language, None);
    }

    fn comments(n: usize) -> String {
        format!(
            "<div id=comments><ol>{}</ol></div>",
            format!("<li class=comment><span class=comment-author>Margaret</span>{PARA}</li>")
                .repeat(n)
        )
    }

    #[test]
    fn taxonomy_classes_do_not_make_a_post_boilerplate() {
        let html = format!(
            "<main><article id=post class='post tag-cookies category-social-media tag-related'>\
             <h1>Oat cookies</h1>{PARA}{PARA}</article>{}</main>",
            comments(8)
        );
        assert_eq!(main_tag(&html).as_deref(), Some("article#post"));
    }

    #[test]
    fn a_post_with_a_long_list_of_links_is_still_the_post() {
        let items = "<li><a href=/api/x><code>Thing::method</code></a></li>".repeat(40);
        let html = format!(
            "<nav><a href=/>Home</a></nav><div id=post><h1>Widget 2.0</h1>\
             <p>Widget 2.0 is out today, with faster builds and a new plugin API.</p>\
             <h2>Faster builds</h2><p>Incremental builds now reuse the cache across branches, so \
             switching back is close to instant on large projects.</p>\
             <h2>Stabilized APIs</h2><ul>{items}</ul><p>Check out everything that changed.</p></div>"
        );
        assert_eq!(main_tag(&html).as_deref(), Some("div#post"));
    }

    #[test]
    fn an_article_of_linked_sections_is_not_a_listing() {
        let section = format!(
            "<section><h2><a href='https://shop.example/1'>1. Lamp</a></h2>{PARA}</section>"
        );
        let html = format!(
            "<article id=best><h1>The 5 best lamps</h1>{PARA}{}</article>",
            section.repeat(5)
        );
        assert_eq!(main_tag(&html).as_deref(), Some("article#best"));
    }

    #[test]
    fn a_grid_of_image_cards_has_no_main_content() {
        let card = "<div class=card><a href=/n/1><img src=/i.jpg></a><a href=/n/1><p>A headline about \
                    something that happened in the city today</p></a><span>3 hours ago</span></div>";
        let html = format!(
            "<main><div class=grid>{}</div>{PARA}{PARA}</main>",
            card.repeat(12)
        );
        assert_eq!(main_tag(&html), None);
    }

    #[test]
    fn content_only_in_noscript_is_read_when_nothing_else_is_there() {
        let html = format!(
            "<section id=splash>Loading...</section><noscript><div id=main-outlet role=main>\
             <h1>How do I flip a pancake?</h1><div class=topic-body>{PARA}{PARA}{PARA}{PARA}{PARA}</div></div></noscript>"
        );
        let doc = Document::parse(&html);
        let main = main_content(doc.root()).unwrap();
        assert_eq!(main.node.attr("id"), Some("main-outlet"));
        assert!(
            main.clean_text()
                .starts_with("How do I flip a pancake?\n\nThe river")
        );
        assert!(main.markdown(None).contains("# How do I flip"));
        let visible =
            format!("<div id=a>{PARA}{PARA}</div><noscript><p>Enable JavaScript</p></noscript>");
        assert_eq!(main_tag(&visible).as_deref(), Some("div#a"));
    }

    #[test]
    fn responses_are_not_the_content() {
        let html = format!(
            "<h1>Flood</h1><div id=bodyContent>{PARA}{PARA}</div>\
             <section id=responses><h2>Responses</h2>{}</section>",
            PARA.repeat(4)
        );
        assert_eq!(main_tag(&html).as_deref(), Some("div#bodyContent"));
    }

    #[test]
    fn the_visible_heading_outranks_a_differing_json_ld_headline() {
        let html = format!(
            r#"<title>Flood - Encyclopedia</title><meta property="og:title" content="Flood - Encyclopedia">
            <script type="application/ld+json">{{"@type": "Article", "headline": "natural overflow of water"}}</script>
            <h1 id=firstHeading>Flood<span class=mw-editsection>[<a href="/edit">edit</a>]</span></h1>
            <div id=bodyContent>{PARA}{PARA}</div>"#
        );
        let doc = Document::parse(&html);
        assert_eq!(article(doc.root(), None).title.as_deref(), Some("Flood"));
        let logo = format!(
            r#"<h1><a href="/">Paper</a></h1><meta property="og:title" content="Real"><div>{PARA}{PARA}</div>"#
        );
        let doc = Document::parse(&logo);
        assert_eq!(article(doc.root(), None).title.as_deref(), Some("Real"));
    }

    #[test]
    fn a_person_outranks_an_organisation_as_the_byline() {
        let page = |ld: &str, meta: &str| {
            let html = format!(
                r#"{meta}<script type="application/ld+json">{ld}</script><article>{PARA}{PARA}</article>"#
            );
            let doc = Document::parse(&html);
            article(doc.root(), None).byline
        };
        let org = r#"{"@type": "Article", "author": {"@type": "Organization", "name": "Codex"}}"#;
        let meta = r#"<meta name="author" content="Scott A.">"#;
        assert_eq!(page(org, meta).as_deref(), Some("Scott A."));
        assert_eq!(page(org, "").as_deref(), Some("Codex"));
        let both = r#"{"@type": "Article", "author": [{"@type": "Organization", "name": "Codex"}, {"@type": "Person", "name": "Ada"}]}"#;
        assert_eq!(page(both, meta).as_deref(), Some("Ada"));
    }

    #[test]
    fn a_byline_is_never_a_commenter_a_bio_or_a_footer_link() {
        let html = format!(
            "<div id=post>{PARA}{PARA}</div>{}<p class=author-bio>Ana writes about bread and \
             baking from her tiny kitchen in Leeds.</p><footer><a rel=author href=/me>Site Owner</a></footer>",
            comments(3)
        );
        let doc = Document::parse(&html);
        assert_eq!(article(doc.root(), None).byline, None);
    }

    #[test]
    fn an_enable_javascript_notice_is_not_content() {
        let html = "<title>Shop - Home</title><div id=root></div><noscript><div class=wrap>\
                    <h1>JavaScript required</h1><p>We're sorry, but this site doesn't work properly \
                    without JavaScript enabled. Please enable JavaScript in your browser settings and \
                    reload the page to continue shopping with us today.</p></div></noscript>";
        assert_eq!(main_tag(html), None);
        let doc = Document::parse(html);
        assert_eq!(
            article(doc.root(), None).title.as_deref(),
            Some("Shop - Home")
        );
    }

    #[test]
    fn hidden_widgets_and_modals_are_not_the_product() {
        let html = format!(
            "<main><h1>Era 95</h1><div class=product-info><p class=price>$90</p>\
             <div class='product__description rte'>{PARA}{PARA}</div></div></main>\
             <s-modal class=w-full><div class=oos-bis><p>Sign up to be notified when your size is back in \
             stock. We will send you one email and then delete your address from our list for good.</p>\
             <form><input type=email><p>We need your permission to send you emails about the product \
             you asked about, and you can say no at any time by replying to any of our emails.</p></form></div></s-modal>\
             <div class='visually-hidden'>{PARA}{PARA}</div><div class='hidden'>{PARA}{PARA}</div>"
        );
        assert_eq!(main_tag(&html).as_deref(), Some("main"));
    }

    #[test]
    fn a_grid_of_product_cards_has_no_main_content() {
        let card = "<li class=grid__item><div><a href=/p/1><img src=/1.jpg alt='Air 16'></a></div>\
                    <p class=name>Air Jordan 16 Metallic Silver</p><p class=price>$255.00</p></li>";
        let html = format!(
            "<main><h1>Mens Footwear</h1><p>{}</p><ul class=grid>{}</ul></main>",
            &PARA[3..PARA.len() - 4],
            card.repeat(24)
        );
        assert_eq!(main_tag(&html), None);
        // A product page with a few cards of recommendations is still a product page.
        let page = format!(
            "<main><div id=product><h1>Air 16</h1><p>$255.00</p>{PARA}{PARA}</div>\
             <ul class=recs>{}</ul></main>",
            card.repeat(4)
        );
        assert_eq!(main_tag(&page).as_deref(), Some("div#product"));
    }

    #[test]
    fn a_homepage_of_headline_cards_has_no_main_content() {
        let card = "<div class=card><h3><a href=/s/1>Senate passes the budget after a long night</a></h3>\
                    <p>Lawmakers voted 51-49 shortly before dawn.</p></div>";
        let html = format!(
            "<main><div class=grid>{}</div><div class=grid>{}</div>\
             <div class=note><p>{}</p></div></main>",
            card.repeat(12),
            card.repeat(12),
            "Ripple brings together diverse perspectives from other publications to show how the \
             country is engaging with today's biggest stories, independently of our newsroom."
        );
        assert_eq!(main_tag(&html), None);
    }

    #[test]
    fn site_names_and_breadcrumbs_are_not_the_title() {
        let site = format!(
            r#"<meta property="og:title" content="Popular git config options">
            <header><h1 class=site-title><a href="/">Julia's blog</a> notes</h1></header>
            <main><h2>Popular git config options</h2>{PARA}{PARA}</main>"#
        );
        let doc = Document::parse(&site);
        assert_eq!(
            article(doc.root(), None).title.as_deref(),
            Some("Popular git config options")
        );
        let crumbs = format!(
            r#"<meta property="og:title" content="Attention Is All You Need">
            <main><h1>Computer Science > Computation and Language</h1>
            <h1 class=title>Attention Is All You Need</h1>{PARA}{PARA}</main>"#
        );
        let doc = Document::parse(&crumbs);
        assert_eq!(
            article(doc.root(), None).title.as_deref(),
            Some("Attention Is All You Need")
        );
    }

    #[test]
    fn an_article_header_is_not_a_site_header() {
        let html = format!(
            r#"<header><nav><a href="/">Home</a></nav></header>
            <script type="application/ld+json">{{"headline": "It&apos;s a summary"}}</script>
            <div><header class=title-article><h1>It's 'No'</h1></header><div id=a>{PARA}{PARA}</div></div>"#
        );
        let doc = Document::parse(&html);
        assert_eq!(
            article(doc.root(), None).title.as_deref(),
            Some("It's 'No'")
        );
        let html = format!(
            r#"<script type="application/ld+json">{{"headline": "It&apos;s here"}}</script><div>{PARA}{PARA}</div>"#
        );
        let doc = Document::parse(&html);
        assert_eq!(
            article(doc.root(), None).title.as_deref(),
            Some("It's here")
        );
    }

    #[test]
    fn replies_under_an_article_do_not_swallow_it() {
        let html = format!(
            "<div id=page><article id=story><h1>Story</h1>{PARA}{PARA}</article>\
             <div id=replies>{}</div></div>",
            format!("<div><b>someone</b>{PARA}</div>").repeat(5)
        );
        assert_eq!(main_tag(&html).as_deref(), Some("article#story"));
    }

    #[test]
    fn a_byline_is_a_name_not_an_id_or_trailing_punctuation() {
        let html = format!(
            r#"<meta name="author" content="105875"><div id=a><p class=author>William Melville ,</p>{PARA}{PARA}</div>"#
        );
        let doc = Document::parse(&html);
        assert_eq!(
            article(doc.root(), None).byline.as_deref(),
            Some("William Melville")
        );
    }

    #[test]
    fn widget_words_yield_to_a_wrapper_holding_the_page() {
        for class in [
            "overlay-tutorial",
            "modal-analysis",
            "drawer-slides",
            "notify-me-guide",
            "widget-area-group",
        ] {
            let html = format!(
                "<nav><a href=/>Home</a></nav><div class='{class}'><div id=a><h1>Guide</h1>{PARA}{PARA}{PARA}</div>\
                 <aside><a href=/x>Other</a></aside></div>"
            );
            assert_eq!(main_tag(&html).as_deref(), Some("div#a"), "{class}");
        }
    }

    #[test]
    fn api_responses_are_documentation() {
        let html = format!(
            "<div id=op><h2>Create a pet</h2>{PARA}<section id=requestBody>{PARA}</section>\
             <section id=responses><h3>Responses</h3><table><tr><th>Status</th><th>Meaning</th></tr>\
             <tr><td>201</td><td>Created: the pet was stored and its new id is in the Location header of the response.</td></tr>\
             <tr><td>400</td><td>Bad request: a required field is missing or one of the values could not be read at all.</td></tr></table>\
             <pre><code>{{\"id\": 7, \"name\": \"Rex\"}}</code></pre></section></div>"
        );
        let doc = Document::parse(&html);
        let main = main_content(doc.root()).unwrap();
        assert_eq!(main.node.attr("id"), Some("op"));
        assert!(main.clean_text().contains("Bad request"));
    }

    #[test]
    fn a_scripts_did_not_run_notice_is_not_content() {
        let html = "<div id=app></div><p>Notice: This page displays a fallback because interactive scripts \
                    did not run. Possible causes include disabled JavaScript or failure to load scripts or \
                    stylesheets. Please try reloading the page, or come back later.</p>";
        assert_eq!(main_tag(html), None);
        let about =
            format!("<div id=a><p>JavaScript is the language of the web.</p>{PARA}{PARA}</div>");
        let docs =
            format!("<div id=d><p>Description: the required fields of a request.</p>{PARA}</div>");
        assert_eq!(main_tag(&docs).as_deref(), Some("div#d"));
        assert_eq!(main_tag(&about).as_deref(), Some("div#a"));
    }

    #[test]
    fn content_collapsed_behind_a_click_is_still_content() {
        let html = format!(
            r#"<div id=course><h1>Computer Science</h1>{PARA}
            <h2><button aria-expanded="false" aria-controls="p1">Entry requirements</button></h2>
            <div id="p1" aria-hidden="true" hidden><p>Three A-levels at grade A*AA, including Mathematics, or an equivalent qualification.</p></div>
            <details><summary>Fees</summary><p>Tuition fees for home students are set each year by the government.</p></details>
            <div role="tabpanel" class="d-none"><p>Nutrition per serving: 61 kcal, 2 g fat, 7 g carbohydrate.</p></div>
            <div class="modal hidden"><p>Sign up to our newsletter for news about open days and events.</p></div>
            <form hidden><p>Request a prospectus to be sent to your home address by post.</p></form>
            <span class="sr-only">Opens in a new window</span></div>"#
        );
        let doc = Document::parse(&html);
        let text = main_content(doc.root()).unwrap().clean_text();
        for kept in [
            "Entry requirements",
            "Three A-levels",
            "Fees",
            "Tuition fees",
            "Nutrition per serving",
        ] {
            assert!(text.contains(kept), "{kept}: {text}");
        }
        for dropped in ["newsletter", "prospectus", "new window"] {
            assert!(!text.contains(dropped), "{dropped}: {text}");
        }
    }

    #[test]
    fn sections_of_one_episode_are_not_a_listing() {
        let act = format!(
            "<article class=act><h2><a href=/1/act>Act One</a></h2><p>After he goes to Jerusalem and sleeps on the \
             spot, Kevin has a revelation: he should live the next six months as if he would die at the end of them.</p>{PARA}{PARA}</article>"
        );
        let html = format!("<div id=ep><h1>New Beginnings</h1>{}</div>", act.repeat(4));
        assert_eq!(main_tag(&html).as_deref(), Some("div#ep"));
    }

    #[test]
    fn navboxes_and_language_lists_are_boilerplate() {
        let html = format!(
            "<div id=a>{PARA}{PARA}<table class='navbox mw-collapsible'><tr><td><a href=/a>A</a> <a href=/b>B</a> \
             Some navigation box text that runs on</td></tr></table><ul class=interlanguage><li><a href=/fr>Français</a></li></ul></div>"
        );
        let doc = Document::parse(&html);
        let text = main_content(doc.root()).unwrap().clean_text();
        assert!(
            !text.contains("navigation box") && !text.contains("Français"),
            "{text}"
        );
    }

    #[test]
    fn hidden_menus_banners_and_duplicates_are_dropped_inside_content() {
        let html = format!(
            r#"<article id=a><h1>Paper</h1>{PARA}
            <div style="display:none"><ul><li><a href=/a>Author</a></li><li><a href=/v>Venue</a></li>
              <li><a href=/i>Institution</a></li><li><a href=/t>Topic</a></li></ul></div>
            <div class="hidden" id="cookie-notice"><p>We use cookies to show you personalised content and adverts on this site.</p></div>
            <div class="article__gift-modal"><p>Cet article vous est offert. Pour lire gratuitement, connectez-vous.</p></div>
            <div class="paywall"><p>Subscribe to keep reading this story and everything else we publish.</p></div>
            <figure><img src=/p.jpg alt=Photo><figcaption>Photo by Jo Lee</figcaption></figure>
            <div aria-hidden="true"><p>Photo by Jo Lee</p></div>
            <p>Same paragraph twice in a row, as hidden and visible copies of it.</p>
            <p>Same paragraph twice in a row, as hidden and visible copies of it.</p>
            {PARA}</article>"#
        );
        let doc = Document::parse(&html);
        let text = main_content(doc.root()).unwrap().clean_text();
        for dropped in ["Institution", "cookies", "offert", "Subscribe"] {
            assert!(!text.contains(dropped), "{dropped}: {text}");
        }
        assert_eq!(text.matches("Photo by Jo Lee").count(), 1, "{text}");
        assert_eq!(text.matches("Same paragraph twice").count(), 1, "{text}");
    }

    #[test]
    fn wiki_jump_links_and_translation_menus_are_left_out() {
        let html = format!(
            r##"<div id=a><a class="mw-jump-link" href="#content">Jump to content</a>
            <div class="mw-pt-languages"><ul><li><a href=/fr>français</a></li><li><a href=/de>Deutsch</a></li></ul></div>
            {PARA}{PARA}</div>"##
        );
        let doc = Document::parse(&html);
        let text = main_content(doc.root()).unwrap().clean_text();
        assert!(
            !text.contains("Jump to") && !text.contains("Deutsch"),
            "{text}"
        );
    }

    #[test]
    fn a_page_without_an_article_has_empty_fields() {
        let doc = Document::parse("<p>hi</p>");
        assert_eq!(article(doc.root(), None), Article::default());
    }
}
