"""Editing the tree, Beautiful Soup style: decompose, extract, replace_with,
unwrap, new_tag, append, insert, .string and prettify."""

import gc
import threading

import pytest

import netweir

PAGE = """<html><head><title>T</title><style>p { color: red }</style></head><body>
<div id="main"><p class="intro">Hello <b>big</b> world</p>
<script>var tracking = 1;</script>
<ul><li>one</li><li>two</li><li>three</li></ul></div>
<!-- the end --></body></html>"""


@pytest.fixture
def doc():
    return netweir.parse(PAGE)


def test_decompose_scripts_and_styles_before_reading_text(doc):
    for s in doc.find_all(["script", "style", "title"]):
        s.decompose()
    text = doc.get_text(" ", strip=True)
    assert text == "Hello big world one two three"
    assert "tracking" not in doc.html
    assert len(doc.css("script, style")) == 0


def test_a_decomposed_nodes_handle_reads_what_was_removed(doc):
    p = doc.find("p")
    b = p.find("b")
    p.decompose()
    # Still readable, and no longer on the page.
    assert p.html == '<p class="intro">Hello <b>big</b> world</p>'
    assert p.parent is None
    assert b.parent == p
    assert doc.find("p") is None
    assert doc.xpath("count(//b)").get() == "0.0"
    # Queries on it see only it.
    assert p.css("b::text").get() == "big"
    assert p.xpath("//b/text()").get() == "big"
    assert p.find_all("b") == [b]
    # A second decompose does nothing.
    p.decompose()


def test_extract_returns_the_node_to_put_elsewhere(doc):
    li = doc.find("li").extract()
    assert li.text == "one"
    assert doc.css("li::text").getall() == ["two", "three"]
    doc.find("ul").append(li)
    assert doc.css("li::text").getall() == ["two", "three", "one"]


def test_replace_with_nodes_and_strings(doc):
    b = doc.find("b")
    em = doc.new_tag("em", string="large")
    old = b.replace_with(em, "!")
    assert old == b
    assert old.parent is None
    assert doc.find("p").html == '<p class="intro">Hello <em>large</em>! world</p>'
    assert doc.find("p").get_text() == "Hello large! world"


def test_unwrap_keeps_the_contents(doc):
    b = doc.find("b").unwrap()
    assert doc.find("p").html == '<p class="intro">Hello big world</p>'
    assert b.html == "<b></b>"
    # Text pieces are separate nodes, as after Beautiful Soup's unwrap.
    assert [c.kind for c in doc.find("p").children] == ["text", "text", "text"]


def test_new_tag_append_and_insert(doc):
    ul = doc.find("ul")
    a = doc.new_tag("a", {"href": "/x"}, string="link", class_="nav", data_id=3)
    assert a.parent is None
    assert a.html == '<a href="/x" class="nav" data_id="3">link</a>'
    first = doc.new_tag("LI", string="zero")
    ul.insert(0, first)
    ul.insert(-1, "before-last")
    ul.insert(99, doc.new_tag("li", string="end"))
    ul.find("li").append(a)
    assert ul.html == (
        '<ul><li>zero<a href="/x" class="nav" data_id="3">link</a></li>'
        "<li>one</li><li>two</li>before-last<li>three</li><li>end</li></ul>"
    )
    assert doc.css("ul > li::text").getall() == ["zero", "one", "two", "three", "end"]


def test_string_setter_replaces_the_contents(doc):
    p = doc.find("p")
    b = p.find("b")
    p.string = "Plain & simple"
    assert p.html == '<p class="intro">Plain &amp; simple</p>'
    assert p.string == "Plain & simple"
    assert b.parent is None


def test_mutation_after_a_css_query_leaves_later_queries_right(doc):
    # Build the index with an XPath query and hold a Selection.
    assert doc.xpath("//li/text()").getall() == ["one", "two", "three"]
    items = doc.css("li")
    items[1].decompose()
    new = doc.new_tag("li", string="four")
    doc.find("ul").append(new)
    # The index is rebuilt for the next query.
    assert doc.xpath("//li/text()").getall() == ["one", "three", "four"]
    assert doc.xpath("//li[last()]/text()").get() == "four"
    assert doc.css("li::text").getall() == ["one", "three", "four"]
    assert doc.xpath("id('main')//li[2]/text()").get() == "three"
    # The old Selection still reads its nodes, the removed one included.
    assert items.getall() == ["<li>one</li>", "<li>two</li>", "<li>three</li>"]
    assert items.css("::text").getall() == ["one", "two", "three"]


def test_edits_that_cannot_work_raise_and_change_nothing(doc):
    before = doc.html
    main = doc.find("div")
    with pytest.raises(ValueError, match="inside itself"):
        main.find("li").append(main)
    with pytest.raises(ValueError, match="document itself"):
        doc.decompose()
    with pytest.raises(ValueError, match="another page"):
        main.append(netweir.parse("<p>x</p>").find("p"))
    with pytest.raises(ValueError, match="tag name"):
        doc.new_tag("not a tag")
    with pytest.raises(TypeError, match="Node or a str"):
        main.append(3)
    with pytest.raises(ValueError, match="no parent"):
        doc.new_tag("p").replace_with("x")
    assert doc.html == before


def test_editing_from_inside_a_find_all_function_raises():
    doc = netweir.parse("<p>a</p><p>b</p>")

    def drop(tag):
        tag.decompose()
        return True

    with pytest.raises(RuntimeError, match="find_all"):
        doc.find_all(drop)
    # Reading from inside one is fine.
    assert len(doc.find_all(lambda t: t.find_parent("body") is not None)) == 2


def test_prettify_output_is_stable():
    doc = netweir.parse(
        '<!DOCTYPE html><html><head><title>T</title></head><body><div class="a">  x '
        "<br><span>y</span><!-- c --></div><pre> keep\n me</pre></body></html>"
    )
    assert doc.prettify() == (
        "<!DOCTYPE html>\n"
        "<html>\n"
        " <head>\n"
        "  <title>\n"
        "   T\n"
        "  </title>\n"
        " </head>\n"
        " <body>\n"
        '  <div class="a">\n'
        "   x\n"
        "   <br>\n"
        "   <span>\n"
        "    y\n"
        "   </span>\n"
        "   <!-- c -->\n"
        "  </div>\n"
        "  <pre> keep\n"
        " me</pre>\n"
        " </body>\n"
        "</html>\n"
    )
    assert doc.find("span").prettify() == "<span>\n y\n</span>\n"


def test_handles_to_removed_nodes_survive_heavy_churn():
    # Keep handles to everything while removing, moving and replacing, and
    # let the garbage collector run in between: nothing may crash, and every
    # handle must still read.
    doc = netweir.parse("<div>" + "<p class=x><b>t</b> u</p>" * 300 + "</div>")
    div = doc.find("div")
    kept = []
    for round_ in range(300):
        nodes = div.find_all(True)
        kept.extend(nodes)
        if not nodes:
            div.append(doc.new_tag("p", string="again"))
            continue
        n = nodes[(round_ * 7919) % len(nodes)]
        match round_ % 5:
            case 0:
                n.decompose()
            case 1:
                if n.parent is not None:
                    n.unwrap()
            case 2:
                # Back into the page, wherever it was.
                div.insert(0, kept[(round_ * 31) % len(kept)])
            case 3:
                if n.parent is not None:
                    n.replace_with(doc.new_tag("i", string=str(round_)), "s")
            case 4:
                n.string = "set"
        if round_ % 30 == 0:
            gc.collect()
            assert len(doc.xpath("//div//*")) == len(div.find_all(True))
    for node in kept:
        assert isinstance(node.html, str)
        node.parent  # noqa: B018 - reading must not crash
    del kept, doc, div
    gc.collect()


def test_edits_wait_for_readers_on_other_threads():
    doc = netweir.parse("<ul>" + "<li>x</li>" * 500 + "</ul>")
    ul = doc.find("ul")
    stop = threading.Event()
    errors = []

    def read():
        try:
            while not stop.is_set():
                # Each loop below adds one, removes two and adds one.
                assert 499 <= len(doc.css("li")) <= 501
                assert doc.xpath("count(//li)").get() in ("499.0", "500.0", "501.0")
                doc.find_all("li", limit=5)
                assert doc.html.startswith("<html>")
        except Exception as e:  # noqa: BLE001 - report any failure from the thread
            errors.append(e)

    readers = [threading.Thread(target=read) for _ in range(4)]
    for t in readers:
        t.start()
    for _ in range(300):
        li = doc.new_tag("li", string="y")
        ul.append(li)
        li.decompose()
        ul.find("li").extract()
        ul.append(doc.new_tag("li", string="z"))
    stop.set()
    for t in readers:
        t.join()
    assert errors == []
    assert len(doc.css("li")) == 500


def test_new_tag_writes_attribute_values_as_beautiful_soup_does():
    doc = netweir.parse("<p>x</p>")
    a = doc.new_tag(
        "a",
        {"class": ["a", "b"], "hidden": None, "t": True, "f": False, "n": 3, "data_": "d"},
        for_="x",
    )
    # Lists are joined with spaces, None is an attribute without a value,
    # anything else is its str(). Only keyword names lose a trailing "_".
    assert a.attrs == {
        "class": "a b",
        "hidden": "",
        "t": "True",
        "f": "False",
        "n": "3",
        "data_": "d",
        "for": "x",
    }


def test_editing_any_page_from_inside_a_query_raises():
    x = netweir.parse("<p>a</p>")
    y = netweir.parse("<p>b</p>")

    def edit_other(tag):
        y.find("p").decompose()
        return True

    with pytest.raises(RuntimeError, match="running a query"):
        x.find_all(edit_other)
    assert y.find("p") is not None
