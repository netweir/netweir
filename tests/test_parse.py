import threading

import pytest

import netweir

PAGE = """
<html><body>
  <div id="main">
    <h1>All products</h1>
    <article class="product"><h3><a href="/a" title="Book A">A</a></h3><p class="price">£1.00</p></article>
    <article class="product"><h3><a href="/b" title="Book B">B</a></h3><p class="price">£2.50</p></article>
    <article class="product sold"><h3><a href="/c">C</a></h3><p class="price"><span>£</span>3.75</p></article>
  </div>
</body></html>
"""


@pytest.fixture
def page():
    return netweir.parse(PAGE)


def test_text_and_attributes(page):
    assert page.css("h1::text").get() == "All products"
    assert page.css(".price::text").getall() == ["£1.00", "£2.50", "3.75"]
    assert page.css("article a::attr(href)").getall() == ["/a", "/b", "/c"]


def test_get_default(page):
    assert page.css("table::text").get() is None
    assert page.css("table::text").get("none") == "none"


def test_plain_selector_gives_outer_html(page):
    assert page.css(".sold .price").get() == '<p class="price"><span>£</span>3.75</p>'
    assert page.css(".sold .price")[0].text == "£3.75"


def test_selections_chain(page):
    articles = page.css("article")
    assert len(articles) == 3
    assert articles.css("a::text").getall() == ["A", "B", "C"]
    assert articles[1].css(".price::text").get() == "£2.50"
    assert articles[-1].attr("class") == "product sold"


def test_nodes(page):
    link = page.css("article a")[0]
    assert link.tag == "a"
    assert link.text == "A"
    assert link.attrs == {"href": "/a", "title": "Book A"}
    assert link.attr("missing") is None
    assert page.tag is None
    assert [n.tag for n in page.css("article")] == ["article"] * 3


def test_string_selections_index_to_strings(page):
    hrefs = page.css("a::attr(href)")
    assert hrefs[0] == "/a"
    assert list(hrefs) == ["/a", "/b", "/c"]


def test_index_out_of_range(page):
    with pytest.raises(IndexError):
        page.css("article")[3]


def test_bad_selector_raises(page):
    with pytest.raises(netweir.SelectorError):
        page.css("div[")
    with pytest.raises(ValueError):  # SelectorError is a ValueError
        page.css("a::attr()")


def test_nodes_outlive_their_selection():
    node = netweir.parse("<p>kept</p>").css("p")[0]
    assert node.text == "kept"


def test_timeout_stops_hostile_pages():
    with pytest.raises(netweir.ParseTimeout):
        netweir.parse("<div>" * 200_000, timeout=0.2)
    with pytest.raises(ValueError):
        netweir.parse("<p>", timeout=-1)
    assert netweir.parse("<p>x</p>", timeout=5).css("p::text").get() == "x"


def test_threads_share_a_document():
    doc = netweir.parse("<ul>" + "<li>x</li>" * 1000 + "</ul>")
    errors = []

    def work():
        try:
            for _ in range(50):
                assert len(doc.css("li::text")) == 1000
        except Exception as e:  # noqa: BLE001 - report any failure from the thread
            errors.append(e)

    threads = [threading.Thread(target=work) for _ in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert errors == []


def test_bytes_are_refused_until_encoding_detection_exists():
    with pytest.raises(TypeError):
        netweir.parse(b"<p>x</p>")


def test_lone_surrogates_raise_instead_of_crashing():
    with pytest.raises(UnicodeEncodeError):
        netweir.parse("\ud800")


def test_names_are_case_insensitive_like_a_browser():
    root = netweir.parse("<DIV CLASS=X Data-Id=1><P>a</P></DIV>")
    div = root.css("div")[0]
    assert div.tag == "div"
    assert div.attr("CLASS") == div.attr("class") == "X"
    assert div.attrs == {"class": "X", "data-id": "1"}
    assert root.css("DIV.X::attr(data-id)").get() == "1"


def test_whitespace_text_nodes_are_kept_like_scrapy():
    root = netweir.parse("<div><p>a</p>\n  <p>b</p></div>")
    assert root.css("div::text").getall() == ["\n  "]


def test_the_version_is_the_one_installed():
    from importlib.metadata import version

    assert netweir.__version__ == version("netweir")


def test_built_without_pyo3s_reference_pool():
    # With the pool, every call into Rust takes one process-wide lock and
    # threads on free-threaded Python barely scale (docs/design/threads.md).
    # A RUSTFLAGS of your own replaces .cargo/config.toml's, flag and all.
    from netweir import _native

    assert _native._reference_pool_disabled
