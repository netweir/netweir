import datetime
from pathlib import Path

import netweir
from netweir._native import _response

FIXTURES = Path(__file__).parent.parent / "crates/netweir-dom/tests/fixtures/content"

PARA = (
    "<p>The river rose through the night, and by morning the lower town was under "
    "a foot of water, with the bridge closed to traffic.</p>"
)
STORY = f"""<html lang="en"><head><title>Flood | Paper</title>
<meta property="article:published_time" content="2026-10-12T09:30:00Z">
</head><body>
<nav><a href="/">Home</a> <a href="/news">News</a></nav>
<article><h1>Town flooded</h1><p class="byline">By Ada Lovelace</p>
{PARA}<div class="share"><a href="/s">Share</a></div>{PARA}
<p><a href="/more">Read more</a> about <em>floods</em>.</p><img src="lead.jpg" alt="Water"></article>
<section id="comments">{PARA}{PARA}{PARA}</section>
<footer>Paper Ltd</footer></body></html>"""


def page(html, url="https://paper.example/2026/flood"):
    return netweir.Page(
        _response(url, 200, "HTTP/1.1", [("content-type", "text/html")], html.encode())
    )


def test_main_content_is_the_article_not_the_page_around_it():
    main = page(STORY).main_content()
    assert main.tag == "article"
    doc = netweir.parse(STORY)
    assert doc.main_content() == doc.find("article")


def test_a_listing_has_no_main_content():
    listing = page(
        (FIXTURES / "listing.html").read_text(encoding="utf-8"), "https://greenhand.example/tools"
    )
    assert listing.main_content() is None
    assert listing.markdown(main_only=True) is None
    assert listing.clean_text(main_only=True) is None
    assert listing.markdown().startswith("[Greenhand](https://greenhand.example/)")


def test_markdown_of_the_main_content_drops_boilerplate_and_resolves_links():
    md = page(STORY).markdown(main_only=True)
    assert md.startswith("# Town flooded\n\nBy Ada Lovelace\n\nThe river rose")
    assert "[Read more](https://paper.example/more) about *floods*." in md
    assert "![Water](https://paper.example/2026/lead.jpg)" in md
    assert "Share" not in md and "Home" not in md and "Paper Ltd" not in md


def test_markdown_of_a_whole_page_or_one_element():
    whole = page(STORY).markdown()
    assert whole.startswith("[Home](https://paper.example/) [News](https://paper.example/news)")
    doc = netweir.parse("<ul><li>a<ol start=3><li>b</li></ol></li></ul>")
    assert doc.find("ul").markdown() == "- a\n  3. b"
    assert doc.find("li").markdown(main_only=True) is None
    assert netweir.parse('<a href="/x">x</a>').markdown() == "[x](/x)"
    assert netweir.parse('<a href="/x">x</a>').markdown("https://a.example/") == (
        "[x](https://a.example/x)"
    )


def test_clean_text_keeps_blocks_apart():
    doc = netweir.parse("<h1>T</h1><p>a <b>b</b><br>c</p><ul><li>d</li><li>e</li></ul>")
    assert doc.clean_text() == "T\n\na b\nc\n\nd\ne"
    text = page(STORY).clean_text(main_only=True)
    assert text.startswith("Town flooded\n\nBy Ada Lovelace\n\nThe river rose")
    assert "Share" not in text


def test_an_article_reads_its_fields():
    a = page(STORY).article()
    assert isinstance(a, netweir.Article)
    assert a.title == "Town flooded"
    assert a.byline == "Ada Lovelace"
    assert a.published == datetime.datetime(2026, 10, 12, 9, 30, tzinfo=datetime.UTC)
    assert a.image == "https://paper.example/2026/lead.jpg"
    assert a.language == "en"
    assert a.text.startswith("Town flooded\n\nBy Ada Lovelace")
    assert a.html.startswith("<article><h1>Town flooded</h1>")
    assert 'class="share"' not in a.html


def test_an_article_without_one_on_the_page_is_mostly_none():
    a = netweir.parse("<title>Hi</title><p>hello</p>").article()
    assert a == netweir.Article(title="Hi")


def test_an_unreadable_date_is_none():
    html = STORY.replace("2026-10-12T09:30:00Z", "sometime last week")
    assert page(html).article().published is None


def test_a_page_built_in_script_is_read_from_its_noscript_copy():
    html = f'<div id="app">Loading…</div><noscript><div id="outlet"><h1>Thread</h1>{PARA * 6}</div></noscript>'
    doc = netweir.parse(html)
    assert doc.main_content().attrs["id"] == "outlet"
    assert doc.markdown(main_only=True).startswith("# Thread\n\nThe river rose")


def test_the_visible_heading_is_the_title_and_a_person_the_byline():
    html = f"""<meta name="author" content="Hana Sato">
    <script type="application/ld+json">{{"@type": "Article", "headline": "a summary, not the title",
    "author": {{"@type": "Organization", "name": "Margin Notes"}}}}</script>
    <article><h1>On being wrong</h1>{PARA}{PARA}</article>"""
    a = netweir.parse(html).article()
    assert (a.title, a.byline) == ("On being wrong", "Hana Sato")
