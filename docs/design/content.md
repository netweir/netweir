# Main content, articles and Markdown

A scraper often wants the part of a page a person came to read: the
story, the post, the docs, the thread, the product description. Sometimes
it wants that as Markdown, which keeps headings, lists, links and tables
in a form that's small, diffable, and easy to feed to a search index or
any tool that reads Markdown. Netweir does both in Rust, in one pass over the parsed
tree each, without a second parser and without a browser.

Four calls, on any `Node` and on `Page`:

- `main_content()`: the element holding the main content, or None.
- `markdown(base_url=None, main_only=False)`: Markdown for the node, or
  for its main content.
- `clean_text(main_only=False)`: the same, as plain text with the blocks
  kept apart.
- `article(base_url=None)`: a `netweir.Article` with the title, byline,
  date, lead image, language, text and HTML.

`Page.markdown()` and `Page.article()` pass the page's URL (or its
`<base href>`) as the base. The code is in `crates/netweir-dom/src/content.rs`
and `markdown.rs`.

## Finding the main content

The idea: weigh every block of text once, add the weights up the tree,
and take the element whose total is highest. A sidebar next to an
article adds links and boilerplate to whatever holds both, so the
wrapper scores less than the article on its own. That's what picks the
article rather than the `<body>` around it, and it works without any
semantic tags at all.

### Counting

One walk collects every element a reader can see (scripts, styles,
form controls, hidden widgets and `<noscript>` are skipped, as in
Markdown; see "What's hidden" below) and adds
each text node's length to its parent. Length is in bytes of
non-whitespace: cheap to count, and it weighs a CJK character, which
says about as much as an English word, as three.

Then, children before parents, each element's text is closed into
blocks. Text belongs to the nearest block element above it (`p`, `div`,
`li`, `td`, a heading and so on); inline elements (`span`, `em`, `a`)
pass theirs up. Text inside an `<a href>` is link text, even inside a
block inside the link: a card's headline in a `<p>` within an `<a>` is a
link, not prose. When a block closes:

- Link text counts against it, as `bad`.
- What's left counts for it, as `good`, if it's prose: at least 25 bytes,
  and no more link text than other text. A prose block under 160 bytes
  counts in proportion to its length, so a stray line in a footer
  ("All times are GMT") is weaker evidence than a paragraph.
- A heading's text always counts as good.
- Anything else (a date, a label, "Share") counts for nothing either way.

### Boilerplate

An element is boilerplate when:

- it's a `<nav>`, `<aside>` or `<footer>`, or has a role of
  navigation, banner, contentinfo, complementary, search, dialog,
  alertdialog, menu, menubar or toolbar;
- or a word in its class, its id, or a custom element's name
  (`<s-modal>`, `<cart-drawer>`) says so. Each is split at anything
  that isn't a letter or digit, and case doesn't matter. Classes that
  file a post under a taxonomy are skipped, because they say what the
  post is about rather than what the element is: a class starting
  `tag-`, `tags-`, `category-`, `categories-`, `cat-`, `topic-`,
  `term-`, `taxonomy-`, `label-`, `format-` or `status-` (so WordPress's
  `tag-cookies` and `category-social-media` don't count). Some words
  always mark boilerplate: `comment(s)`, `share`, `social`, `related`,
  `ad`, `ads`, `advert…`, `sponsor…`, `promo…`, `cookie…`, `consent`,
  `gdpr`, `newsletter…`, `subscribe`, `breadcrumb…`, `pagination`,
  `recommend…`, `popular`, `trending`, `tags`, `navbox`,
  `interlanguage` and a few more (the ones marked … match as a prefix,
  so `relatedposts` and `sharedaddy` count). `responses` marks
  boilerplate (a story's replies) unless the element holds code or a
  table, as an API's documented responses do. Others mark layout, and
  only count when the element has no content word as well: `nav`,
  `menu`, `sidebar`, `widget`, `footer`, `banner`, `masthead`, `modal`,
  `popup`, `overlay`, `drawer`, `notify`, `toast`, though those last six
  count whatever else the element says when they're the last word of a
  class (`article__gift-modal` is a modal, `overlay-tutorial` a
  tutorial). `paywall` always counts. Content words are
  `article`,
  `content`, `entry`, `post`, `story`, `body`, `main`, `text`, `prose`,
  `markdown`. So `content-sidebar-wrap` isn't boilerplate, and `sidebar`
  is.

Four exceptions keep a careless class from hiding the content. The
page's `<html>` and `<body>` are never boilerplate. Nor is anything that
holds a `<main>`, a `role="main"` or an `itemprop="articleBody"`. Nor is
an `<article>` whose class also has a content word: Substack's
`newsletter-post post` is a post. And no class word makes boilerplate
of an element holding nine tenths of the page's prose, or three
quarters if it has a content word too: a two-column `widget-area-group`
around a buying guide and its sidebar, an `overlay-tutorial` around a
whole article, `id="content" class="with-comments"`. That needs the
prose counted once before boilerplate is decided.

### What's hidden

Content a page folds away until you click is still its content: an
accordion's panels, tabs, a FAQ's answers, the rest of a "read more".
So hiding alone doesn't remove anything. An element hidden by the
`hidden` attribute, `aria-hidden="true"`, an inline `display: none` or
`visibility: hidden`, or a hiding class (`hidden`, `is-hidden`,
`u-hidden`, `d-none`, unless another class shows it at some screen size,
as in `hidden md:block`) is left out only when it's a widget, holds no
block of its own, is mostly links (a hidden menu), or is an
`aria-hidden` copy kept for looks rather than a panel (one without an
id, a tab panel's or region's role, or a class like `accordion`,
`panel`, `collapse`, `tab` or `answer`). A widget is a `<form>`, something with `aria-modal`
or a role of dialog, alertdialog, alert, status, menu or tooltip, or
something whose class, id or custom-element name has one of `modal`,
`popup`, `popover`, `overlay`, `drawer`, `dropdown`, `menu`, `toast`,
`tooltip`, `notify`, `dialog`, `cookie`, `consent`, `newsletter` or
`carousel`. Holding no block means no paragraph, list, table, heading,
quotation or code inside: an icon, a "copied!" label. The button that
opens a panel (one with `aria-expanded` or `aria-controls`) is read for
its text, so an accordion's titles stay; other buttons are left out.
Labels written for screen readers only (`visually-hidden`, `sr-only`,
`screen-reader-text`) are always left out, since no reader of the page
sees them, and so are MediaWiki's `mw-jump-link` and the Translate
extension's `mw-pt-languages`. A paragraph exactly the same as the one
written just before it (20 bytes or more) is a hidden copy beside its
visible twin, and comes out once.

A boilerplate element passes nothing good up: its prose and its links
all count against its parent. Comments are the case this is for. A long
comment thread under a short article has more prose than the article,
but it can't win, and it drags down every wrapper that holds both.

### Choosing

The score of an element is `good - bad`. The highest score wins; on a
tie, the deeper element wins, because it's the tighter fit. Elements
inside boilerplate can't win.

Then two adjustments, both skipped when the winner is itself an
`<article>`, `<main>`, `role="article"`, `role="main"` or
`itemprop="articleBody"`:

- The winner should hold most of the prose around it. A post with a long
  list of links in it (release notes listing forty stabilised APIs, an
  API index, further reading) can score below one of its own paragraphs,
  since every link counts against the post. So while the winner holds
  less than half the prose (outside boilerplate) of its parent, the
  parent wins instead. The climb reaches `<body>` only from a lone
  block (a `<p>`, `<li>`, `<td>` and the like).
- Going up from there, the first `<article>`, `<main>` or the like whose
  score is within a tenth of the winner's, and no more than 300 bytes
  below it, wins instead, unless what it adds is a row of cards (three or
  more, making up at least half the text it adds): a product's `<main>`
  that also holds recommendations isn't the product. It catches the common shape of an `<article>`
  with a header of links (categories, the author, a comment count) around
  the `<div>` of paragraphs: the article is what you want, title and
  all. The 300-byte cap stops a long article's tenth from swallowing a
  language menu.

Then, if the winner isn't itself one of those and holds exactly one
`<article>` with at least 140 bytes and a fifth of its prose, the
article wins: the rest is replies or comments that no class names.

There's no main content (None) when:

- the winner has less than 140 bytes of prose, or scores nothing;
- the page is crowded with cards (ten or more) and the winner holds less
  than a fifth of the page's prose, or less than half its prose and a
  fifth of its text: a disclaimer on a news homepage, a collection's
  blurb under a grid of products; or
- it's a listing: three or more of its children are cards, and together
  they hold at least half its prose or half its text. A card holds a
  heading with a link to somewhere else, or is small (300 bytes of text
  at most, and some) with a linked image and either text that's mostly
  links (a headline under a thumbnail, as news homepages lay them out)
  or a price (a product in a shop's grid). Something holding two or more
  cards is a grid, not a card, and a small card's text counts as neither
  prose for nor against what holds it, so a row of recommendations
  doesn't pull the content out to the wrapper around both. The check
  walks down from the winner through the child
  holding 80% or more of its prose, or else more than half its text, so a
  `<main>` around a heading and a grid is still seen as the grid. Links
  to `#fragments` don't make a card, so a docs page's permalinked section
  headings aren't a listing. Nor is anything in an `<article>`, so a
  "five best lamps" piece whose section headings link to shops, or an
  episode page whose acts each link to their own page, is read as the
  article it is. And a listing's cards are short: when they average 300
  bytes of prose or more, they're sections of one page.

When there's no main content and the page has a `<noscript>` holding at
least 500 bytes of text (a few paragraphs, more than a notice), the
whole search runs again with `<noscript>` read as part of the page.
Sites that build themselves in script, such as Discourse forums, put a
plain copy of the page there for clients without it. What that finds is
dropped if it's short (under 1500 bytes) and mentions JavaScript: "this
site needs JavaScript, please enable it" is written as prose too.
Reading the content found that way reads its `<noscript>` too.

The same goes for a winner anywhere on the page: one under 1000 bytes
that says "script", "scripts" or "JavaScript" with "enable",
"disabled", "required", "did not run" or the like ("This page displays
a fallback because interactive scripts did not run") is a notice, and
there's no main content.

### Inside the content

`main_content()` returns the element as it is. Reading it with
`markdown(main_only=True)`, `clean_text(main_only=True)` or `article()`
leaves out the boilerplate elements found inside it: the share bar under
the headline, a related box between paragraphs, the tags footer. The
article's `html` is written by Netweir rather than lexbor for the same
reason, so it can leave those out, along with scripts, styles, hidden
elements and comments. Event handler attributes (`onclick`) and
`javascript:` links go too, so the HTML can be shown or stored without
anything in it running.

## The article's fields

Each field takes the first of these that the page gives, after
collapsing whitespace:

| Field | From |
|---|---|
| `title` | `og:title` when it names one of the content's headings (a post titled in an `<h2>`, or a heading with a label before the title); the first `<h1>` in the main content that isn't a breadcrumb trail (`A > B`, `A › B`, `A / B`); the first `<h1>` on the page outside boilerplate and `<noscript>` that isn't mostly a link (a logo), a breadcrumb, named `site`, `logo` or `brand`, or in a banner or a `<header>` with navigation outside any `<article>` or `<main>`; JSON-LD `headline`; `og:title`; `twitter:title`; `<title>`. Permalinks and edit links in a heading are left out, and entities a page escaped twice (`&apos;` in a JSON-LD headline) are decoded. |
| `byline` | JSON-LD `author` when it's a person (a name, a Person, or a list, joined with commas); `<meta name="author">`, `article:author` or `DC.creator` unless it's a URL or has no letters in it (a numeric id); in the main content and then on the page outside boilerplate, an element with `rel="author"`, `itemprop="author"`, or the class or id word `author` or `byline` (not a commenter's), at most 60 characters and not a sentence, with a leading "By" and trailing punctuation taken off; last, a JSON-LD author that's an Organization |
| `published` | JSON-LD `datePublished`; `article:published_time`; a `<meta>` named `datePublished`, `date`, `pubdate`, `publishdate`, `DC.date`, `dcterms.date` or `dcterms.created`; an element with `itemprop="datePublished"`; the first `<time datetime>` in the main content. Read with `parse_date`. |
| `image` | `og:image`; `twitter:image`; JSON-LD `image`; the first image in the main content that isn't an avatar, icon or logo and isn't marked under 100 pixels |
| `language` | `<html lang>`; `Content-Language` in a `<meta>`; `og:locale` (`fr_FR` as `fr-FR`); JSON-LD `inLanguage` |

For most fields, JSON-LD comes first: a page writes it for search
engines, so it tends to be right. Two exceptions. The title the reader
sees is the `<h1>`, and a JSON-LD `headline` can be a summary instead
(an encyclopedia's is a one-line definition), so the visible heading
wins; the headline still comes before `og:title`, which sites often
rewrite for sharing. And an Organization as author is usually the
publication, so a person named anywhere else comes first. The JSON-LD
objects searched include the members of an `@graph`. The content's
heading, byline, date and image come from one walk through it that skips
its boilerplate.

## Markdown

The writer walks the tree once. Inline text is gathered into the current
block with whitespace collapsed (a space is only put down before the
next word, so trailing spaces never appear), and written when the block
ends, behind the prefixes of the blocks around it: `> ` for each
blockquote, and a list item's marker on its first line and that many
spaces on the rest. A blank line between blocks gets the blockquote
prefixes, trimmed, so a paragraph break inside a quote stays inside it.

The choices worth knowing:

- Emphasis and links are gathered on their own first, so spaces at their
  edges go outside the markers: `a<em> b </em>c` is `a *b* c`, not
  `a* b *c`. Nested emphasis of one kind is written once.
- CommonMark only lets `*` open before punctuation when a space or
  punctuation comes before it, and close after punctuation when one comes
  after. Where that fails (`a<em>(x)</em>b`, `<strong>"q"</strong>word`)
  the emphasis is written as `<em>`, `<strong>` or `<del>` instead, which
  renders the same and can't pair with a later `*`. Whether a closer
  works depends on the next character, so the choice is made when it
  arrives.
- Blocks inside a link, a heading or a table cell run on as text: a card
  wrapped in `<a>` becomes one link, `[Lamp Bright](...)`.
- Escaping is by character: `\`, `` ` ``, `*`, `[`, `]`, `<` and `~`
  always; `_` unless it's inside a word, so `snake_case` stays readable;
  `&` only when it would start an entity; a `!` right before a link, which
  would make it an image. What only matters at the start of a line (`#`,
  `>`, `- `, `+ `, `1. `, `1) `, a line of `-` or `=`) is escaped when the
  line is written, since only then is it known to be the start of one, and
  so are the pipes of a line that would turn the text above it into a
  table (`--- | ---`). A heading's trailing `#` is escaped so it isn't
  read as a closing sequence.
- Whitespace collapses to single spaces, except that a non-breaking space
  stays one in Markdown (it renders the same and still keeps `20 g`
  together) and becomes an ordinary space in `clean_text()`. An
  ideographic space (U+3000) is part of CJK text, and stays in both.
- Two lists of a kind in a row would be one list to Markdown, so the
  second uses the other marker: `*` after `-`, `1)` after `1.`. An
  ordered list's start is kept between 0 and 999999999, as CommonMark
  allows.
- A link across a `<br>` keeps its target, with a hard break inside the
  link text.
- Permalinks are left out: a link to a `#fragment` whose text has no
  letter or digit (`¶`, `#`, `§`), and anything with the class
  `headerlink`, `mw-editsection` or `anchorjs-link` (Sphinx's and
  MediaWiki's).
- Link targets are made absolute and then have spaces, brackets and
  angle brackets percent-encoded, so a target never ends early.
- A code fence is three backticks, or one more than the longest run of
  backticks in the code, with the language from a `language-` or `lang-`
  class on the `<pre>` or its `<code>`, or a `highlight-` class on the
  two elements around it (Sphinx and Pygments; `highlight-default` names
  none). A code span uses one more backtick than the longest run in it,
  padded with spaces when the code starts or ends with one. A `<pre>` in a
  table cell becomes a code span per line, joined with `<br>`.
- Tables: the first row is the header, as GFM needs one. The table is as
  wide as its row with the most cells. Cells spanning columns are
  followed by empty cells and cells spanning rows leave their column
  empty below; cells that don't fit join the row's last cell. A table
  inside another, a table of one column, and a table whose grid would be
  more than four times its cells (plus 64) are read as blocks instead.
  Whether a table holds another is worked out once for the outermost
  table, so nested tables cost one walk. Pipes in cells are escaped,
  and `<br>` in a cell is written as `<br>`.
- Lists and quotations nested more than ten deep are written at ten.
  Every level adds its indent or `> ` to each line inside it, so without
  a cap a few thousand nested lists make Markdown thousands of times the
  size of the page.

`clean_text()` is the same walk with no markup: list items and table
rows on lines of their own, cells separated by tabs, links as their text,
images left out.

## Speed

Measured on Apple silicon, after parsing (`uv run python bench/content.py`):

| 1 MB page | `main_content()` | `markdown(main_only=True)` | `markdown()` | `article()` |
|---|---|---|---|---|
| long article, 1,000 comments | 2.8 ms | 4.1 ms | 6.7 ms | 5.6 ms |
| shop listing (no main content) | 6.2 ms | 6.3 ms | 7.2 ms | 9.4 ms |

Parsing the same pages takes 1.8 and 4.1 ms. The tally is one array of
elements and two passes over it, with no recursion, so deep pages don't
use the stack; the Markdown writer recurses, and grows its stack the way
the XPath evaluator does.

## Testing

`crates/netweir-dom/tests/fixtures/content/` holds a corpus of pages
written for the purpose: a news story with every kind of boilerplate, a
WordPress-style blog post, a docs page with code, tables and permalinks,
a product page, a forum thread, a listing, a page of unnamed `<div>`s and
`<font>` tags, pages in French and Japanese, and the shapes that caught
earlier versions out: a WordPress post with taxonomy classes and a long
comment thread, release notes with forty linked APIs, a Discourse thread
served in `<noscript>`, an encyclopedia article whose JSON-LD headline
isn't its title, a newsletter post with an Organization as author and a
responses section, a homepage grid of image cards, a page whose only
"author" elements are a commenter, a bio and a footer link, a Sphinx page
with permalinks and `highlight-python3`, and a "five best" article whose
section headings link to shops. Each has a JSON file of
phrases that must be in the content, phrases that must not, Markdown
that must appear, and the article's fields. `cargo test -p netweir-dom
--test content -- --nocapture` prints a line per page; all of them pass
in full. The fuzz target `markdown` feeds any HTML through all four.

## Known limits

- Responsive duplicates (a `block md:hidden` copy for phones beside a
  `hidden md:block` one for wide screens, or `d-block d-md-none` beside
  `d-none d-md-block`) are both read: neither is hidden at every size,
  and they aren't paired up unless they come out as the same paragraph
  one after the other.
- A hidden panel that's neither a widget by name nor without blocks
  (an unnamed pop-up holding paragraphs) is read as folded content.
- A page's date comes from its structured data before its HTML, so a
  stale JSON-LD `datePublished` (a changelog's from 2021) wins over the
  date printed on the page.
- Navigation boxes are left out by the words `navbox` and
  `interlanguage`; a wiki whose navboxes have other names (`nb2`) keeps
  them.
- Classes are words, not styles. A stylesheet that hides a cookie
  banner by class, or a site whose boilerplate has class names in no
  language Netweir looks for, isn't known about.
- A listing whose cards have neither a linked heading nor a linked image
  is read as content, and so is one inside an `<article>`, or whose cards
  hold 300 bytes of prose or more on average. A grid of
  fewer than ten cards with a short note beside it can give the note.
- A page with two articles of equal weight side by side gives the
  element holding both.
- The release-notes rule trusts a parent that holds twice the winner's
  prose. A page whose sidebar holds more unlabelled prose than a short
  article can give the wrapper of both.
- Markdown has no merged cells, so a spanning cell's text appears once,
  in its first row and column. `data:` images are left out, and so is
  MathML.
