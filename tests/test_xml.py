"""XML documents from Python: parsing, namespaces, the type a response is
read as, and the limits on hostile XML. Answers are checked against
parsel's, which Scrapy's selectors are."""

import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir
from netweir._native import _response

RSS = """<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>Example &amp; Co</title>
    <item>
      <title>First</title>
      <link>https://example.com/1</link>
      <dc:creator>Ann</dc:creator>
      <description><![CDATA[<p>Hello</p>]]></description>
      <guid isPermaLink="false">a1</guid>
    </item>
    <item>
      <title>Second</title>
      <link>https://example.com/2</link>
      <dc:creator>Bob</dc:creator>
    </item>
  </channel>
</rss>"""

ATOM = """<feed xmlns="http://www.w3.org/2005/Atom" xml:lang="en">
  <title>Atom feed</title>
  <entry>
    <title type="html">One</title>
    <link rel="alternate" href="https://example.com/one"/>
  </entry>
  <entry>
    <title>Two</title>
    <link rel="alternate" href="https://example.com/two"/>
  </entry>
</feed>"""

SOAP = """<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <soap:Body>
    <GetPriceResponse xmlns="urn:Prices">
      <Price xsi:type="decimal">12.50</Price>
    </GetPriceResponse>
  </soap:Body>
</soap:Envelope>"""

ATOM_NS = "http://www.w3.org/2005/Atom"


def test_xml_is_parsed_when_asked():
    root = netweir.parse(RSS, type="xml")
    assert root.type == "xml"
    assert root.xpath("//item/title/text()").getall() == ["First", "Second"]
    # As HTML, <link> is a void element and its URL is lost.
    assert root.xpath("//item/link/text()").getall() == [
        "https://example.com/1",
        "https://example.com/2",
    ]
    assert netweir.parse(RSS).xpath("//item/link/text()").getall() == []
    assert netweir.parse(RSS).type == "html"
    assert root.css("item > title::text").getall() == ["First", "Second"]
    assert root.xpath("//guid/@isPermaLink").get() == "false"
    assert root.xpath("//description/text()").get() == "<p>Hello</p>"


def test_type_must_be_html_or_xml():
    with pytest.raises(ValueError, match="html"):
        netweir.parse("<a/>", type="json")


def test_namespaces_by_argument_or_registered():
    root = netweir.parse(ATOM, type="xml")
    assert root.xpath("//entry").getall() == []
    assert root.xpath("//a:entry/a:title/text()", namespaces={"a": ATOM_NS}).getall() == [
        "One",
        "Two",
    ]
    with pytest.raises(netweir.XPathError, match="undefined namespace prefix"):
        root.xpath("//a:entry")
    root.register_namespace("atom", ATOM_NS)
    entries = root.xpath("//atom:entry")
    # What a node is found by carries the namespaces registered on it.
    assert entries.xpath("atom:link/@href").getall() == [
        "https://example.com/one",
        "https://example.com/two",
    ]
    assert entries[1].xpath("atom:title/text()").get() == "Two"
    assert entries[0].css("title::text").get() == "One"
    # namespaces= is for that query only, as in parsel.
    found = root.xpath("//x:entry", namespaces={"x": ATOM_NS})
    with pytest.raises(netweir.XPathError):
        found.xpath("x:title")
    assert found.xpath("x:title/text()", namespaces={"x": ATOM_NS}).get() == "One"
    with pytest.raises(TypeError, match="dict"):
        root.xpath("//x:entry", namespaces=[("x", ATOM_NS)])
    with pytest.raises(ValueError, match="prefix"):
        root.register_namespace("not a prefix", ATOM_NS)


def test_registering_after_doesnt_reach_nodes_found_before():
    root = netweir.parse(ATOM, type="xml")
    title = root.xpath("/*/*[1]")[0]
    root.register_namespace("atom", ATOM_NS)
    with pytest.raises(netweir.XPathError):
        title.xpath("self::atom:title")
    assert root.xpath("/*/*[1]")[0].xpath("self::atom:title/text()").get() == "Atom feed"


def test_names_attributes_and_written_xml():
    root = netweir.parse(SOAP, type="xml")
    root.register_namespace("p", "urn:Prices")
    root.register_namespace("xsi", "http://www.w3.org/2001/XMLSchema-instance")
    price = root.xpath("//p:Price")
    assert price.get() == (
        '<Price xmlns="urn:Prices" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"'
        ' xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/" xsi:type="decimal">12.50</Price>'
    )
    assert price.attrib == {"xsi:type": "decimal"}
    assert price[0].tag == "Price"
    assert price.xpath("@xsi:type").get() == "decimal"
    assert root.xpath("name(/*)").get() == "soap:Envelope"
    assert root.xpath("local-name(/*)").get() == "Envelope"
    assert root.xpath("namespace-uri(//p:Price)").get() == "urn:Prices"
    assert root.xpath("//p:price").getall() == []


def test_remove_namespaces_is_for_the_whole_document():
    root = netweir.parse(ATOM, type="xml")
    entry = root.xpath("/*/*[2]")[0]
    root.remove_namespaces()
    assert root.xpath("//entry/title/text()").getall() == ["One", "Two"]
    assert entry.xpath("self::entry/title/text()").get() == "One"
    assert "xmlns" not in root.html
    html = netweir.parse("<p>x</p>")
    html.remove_namespaces()
    assert html.css("p::text").get() == "x"


def test_malformed_xml_says_where():
    with pytest.raises(netweir.XMLError) as info:
        netweir.parse("<feed>\n  <entry></feed>", type="xml")
    e = info.value
    assert isinstance(e, ValueError)
    assert (e.line, e.column) == (2, 10)
    assert e.reason == "</feed> doesn't close <entry>"
    assert "line 2, column 10" in str(e)


def test_hostile_xml_is_refused():
    laughs = '<!DOCTYPE l [<!ENTITY a "' + "x" * 1000 + '">'
    for i in range(1, 8):
        laughs += f'<!ENTITY {"abcdefgh"[i]} "' + f"&{'abcdefgh'[i - 1]};" * 10 + '">'
    laughs += "]><l>&h;</l>"
    with pytest.raises(netweir.XMLError, match="billion laughs"):
        netweir.parse(laughs, type="xml")
    xxe = '<!DOCTYPE r [<!ENTITY x SYSTEM "file:///etc/passwd">]><r>&x;</r>'
    with pytest.raises(netweir.XMLError, match="never fetched"):
        netweir.parse(xxe, type="xml")
    with pytest.raises(netweir.XMLError, match="nest more than"):
        netweir.parse("<a>" * 3000, type="xml")
    declared = '<!DOCTYPE r [<!ENTITY co "Example &amp; Co">]><r>&co;</r>'
    assert netweir.parse(declared, type="xml").xpath("/r/text()").get() == "Example & Co"


def test_parse_timeout_applies_to_xml():
    big = "<r>" + "<a>x</a>" * 50_000 + "</r>"
    with pytest.raises(netweir.ParseTimeout):
        netweir.parse(big, timeout=0, type="xml")


def test_new_tags_in_xml_keep_their_case():
    root = netweir.parse("<Root/>", type="xml")
    top = root.children[0]
    top.append(top.new_tag("NewThing", attrs={"Kind": "x"}))
    assert root.html == '<Root><NewThing Kind="x"/></Root>'


def test_beautiful_soup_finders_take_either_name():
    root = netweir.parse(RSS, type="xml")
    assert [n.text for n in root.find_all("creator")] == ["Ann", "Bob"]
    assert [n.text for n in root.find_all("dc:creator")] == ["Ann", "Bob"]
    assert root.find("link").text == "https://example.com/1"
    assert root.find("guid", isPermaLink="false").text == "a1"
    assert root.find("Link") is None
    assert root.select_one("item creator").text == "Ann"


def test_processing_instructions_are_nodes():
    root = netweir.parse('<?xml-stylesheet href="a.xsl"?><r/>', type="xml")
    pi = root.xpath("/processing-instruction()")
    assert pi.get() == '<?xml-stylesheet href="a.xsl"?>'
    assert pi[0].kind == "processing_instruction"


@pytest.mark.parametrize(
    "content_type, kind",
    [
        ("text/xml", "xml"),
        ("application/xml; charset=utf-8", "xml"),
        ("application/rss+xml", "xml"),
        ("application/atom+xml", "xml"),
        ("application/rdf+xml", "xml"),
        ("application/soap+xml", "xml"),
        ("APPLICATION/XML", "xml"),
        ("application/xhtml+xml", "html"),
        ("text/html", "html"),
        ("text/plain", "html"),
        # Nothing said: the body is looked at, as Scrapy's from_body does.
        (None, "xml"),
        ("application/octet-stream", "xml"),
    ],
)
def test_a_response_is_read_as_its_content_type_says(content_type, kind):
    headers = [("content-type", content_type)] if content_type else []
    page = netweir.Page(_response("https://example.com/f", 200, "HTTP/1.1", headers, RSS.encode()))
    assert page.root.type == kind
    links = page.xpath("//item/link/text()").getall()
    assert links == (["https://example.com/1", "https://example.com/2"] if kind == "xml" else [])


def test_a_response_can_be_read_as_xml_whatever_it_says():
    response = _response("https://example.com/f", 200, "HTTP/1.1", [], RSS.encode())
    assert response.parse(type="xml").type == "xml"
    assert netweir.parse(netweir.Page(response).text, type="xml").type == "xml"


def test_the_xml_declaration_names_the_encoding():
    body = '<?xml version="1.0" encoding="ISO-8859-1"?><r>café</r>'.encode("latin-1")
    page = netweir.Page(
        _response("https://example.com/f", 200, "HTTP/1.1", [("content-type", "text/xml")], body)
    )
    assert page.xpath("/r/text()").get() == "café"


def test_page_xpath_takes_namespaces():
    page = netweir.Page(
        _response(
            "https://example.com/f",
            200,
            "HTTP/1.1",
            [("content-type", "application/atom+xml")],
            ATOM.encode(),
        )
    )
    assert page.xpath("//a:title/text()", namespaces={"a": ATOM_NS}).get() == "Atom feed"
    page.register_namespace("a", ATOM_NS)
    assert page.xpath("count(//a:entry)").get() == "2.0"
    page.remove_namespaces()
    assert page.css("entry title::text").getall() == ["One", "Two"]
    with pytest.raises(TypeError):
        page.xpath("//a:entry", track="x", namespaces={"a": ATOM_NS})


def test_page_replace_gives_a_new_page():
    page = netweir.Page(
        _response(
            "https://example.com/f",
            200,
            "HTTP/1.1",
            [("content-type", "text/xml; charset=iso-8859-1")],
            b"<r>old</r>",
        )
    )
    assert page.xpath("/r/text()").get() == "old"
    new = page.replace(body="<r>new é</r>")
    assert new.xpath("/r/text()").get() == "new é"
    # Encoded as the page is, so its charset still tells the truth.
    assert new.headers["content-type"] == "text/xml; charset=iso-8859-1"
    assert new.body == "<r>new é</r>".encode("latin-1")
    # What that encoding can't hold goes as UTF-8, and the charset says so.
    euro = page.replace(body="<r>日本</r>")
    assert euro.xpath("/r/text()").get() == "日本"
    assert euro.headers["content-type"] == "text/xml; charset=utf-8"
    assert page.xpath("/r/text()").get() == "old"
    moved = page.replace(url="https://example.com/g", status=203, headers={"X-A": "1"})
    assert (moved.url, moved.status, moved.headers) == ("https://example.com/g", 203, {"x-a": "1"})
    assert moved.body == b"<r>old</r>"


class Feeds(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    pages: dict = {}

    def log_message(self, *args):
        pass

    def do_GET(self):
        body, ctype = self.pages.get(self.path, (b"", "text/plain"))
        self.send_response(200 if self.path in self.pages else 404)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def feeds():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Feeds)
    base = f"http://127.0.0.1:{server.server_port}"
    Feeds.pages = {
        "/rss": (RSS.encode(), "application/rss+xml"),
        "/xml": (RSS.encode(), "text/xml"),
        "/links": (b"<links><link>/xml</link></links>", "application/xml"),
        "/broken": (b"<rss><channel></rss>", "application/xml"),
        "/nbsp": (b"<rss><item><title>a&nbsp;b</title></item></rss>", "text/xml"),
        "/nbsp-rss": (b"<rss><item><title>a&nbsp;b</title></item></rss>", "application/rss+xml"),
        "/slow": (b"<r>" + b"<a/>" * 200_000 + b"</r>", "application/rss+xml"),
    }
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield base
    server.shutdown()
    server.server_close()


FAST = netweir.Settings(throttle=False, start_delay=0, obey_tdmrep=False)


def test_a_crawl_reads_feeds_as_xml(feeds):
    seen = []

    class Reader(netweir.Spider):
        settings = FAST
        start_urls = [f"{feeds}/rss", f"{feeds}/xml"]

        def parse(self, page):
            seen.append((page.url.rsplit("/", 1)[1], page.xpath("//item/link/text()").getall()))

    Reader().run()
    links = ["https://example.com/1", "https://example.com/2"]
    assert sorted(seen) == [("rss", links), ("xml", links)]


class Feed(netweir.Item):
    titles = netweir.xpath("//item/title/text()", all=True)
    creators = netweir.css("creator::text", all=True)


def test_rules_and_items_read_xml_in_rust(feeds):
    items = []

    class Reader(netweir.Spider):
        settings = FAST
        start_urls = [f"{feeds}/links"]
        # <link> holds its URL as text, which HTML parsing would lose.
        rules = [netweir.Follow(xpath="//link/text()", extract=Feed)]
        pipelines = [items.append]

    Reader().run()
    assert [(i["titles"], i["creators"]) for i in items] == [(["First", "Second"], ["Ann", "Bob"])]


def test_a_broken_feed_reaches_the_callback_and_raises_there(feeds):
    caught = []

    class Reader(netweir.Spider):
        settings = FAST
        start_urls = [f"{feeds}/broken"]

        def parse(self, page):
            try:
                page.xpath("/*")
            except netweir.XMLError as e:
                caught.append((e.line, e.reason, page.text))

    Reader().run()
    assert caught == [(1, "</rss> doesn't close <channel>", "<rss><channel></rss>")]


# --- the same answers as parsel ---------------------------------------------------


@pytest.mark.parametrize(
    "doc, query, namespaces",
    [
        (RSS, "//item/title/text()", {}),
        (RSS, "//item/link", {}),
        (RSS, "//item", {}),
        (RSS, "//dc:creator/text()", {"dc": "http://purl.org/dc/elements/1.1/"}),
        (RSS, "//dc:creator", {"dc": "http://purl.org/dc/elements/1.1/"}),
        (RSS, "count(//item)", {}),
        (RSS, "//guid/@isPermaLink", {}),
        (RSS, "/rss/channel/title/text()", {}),
        (RSS, "//description/text()", {}),
        (ATOM, "//entry", {}),
        (ATOM, "//a:entry", {"a": ATOM_NS}),
        (ATOM, "//a:entry/a:link/@href", {"a": ATOM_NS}),
        (ATOM, "name(//a:entry)", {"a": ATOM_NS}),
        (ATOM, "namespace-uri(//a:entry)", {"a": ATOM_NS}),
        (ATOM, "/*/@xml:lang", {}),
        (ATOM, "//a:*", {"a": ATOM_NS}),
        (SOAP, "name(/*)", {}),
        (SOAP, "local-name(/*)", {}),
        (SOAP, "//p:Price", {"p": "urn:Prices"}),
        (
            SOAP,
            "//p:Price/@x:type",
            {"p": "urn:Prices", "x": "http://www.w3.org/2001/XMLSchema-instance"},
        ),
        (SOAP, "name(//p:Price/@*)", {"p": "urn:Prices"}),
        (SOAP, "//*[local-name()='Price']/text()", {}),
    ],
)
def test_xpath_answers_as_parsel_does(doc, query, namespaces):
    parsel = pytest.importorskip("parsel")
    theirs = parsel.Selector(text=doc, type="xml").xpath(query, namespaces=namespaces).getall()
    ours = netweir.parse(doc, type="xml").xpath(query, namespaces=namespaces).getall()
    assert ours == theirs


def test_remove_namespaces_as_parsel_does():
    parsel = pytest.importorskip("parsel")
    theirs = parsel.Selector(text=SOAP, type="xml")
    theirs.remove_namespaces()
    ours = netweir.parse(SOAP, type="xml")
    ours.remove_namespaces()
    for query in ["//Price", "//Price/@type", "name(/*)", "//Body"]:
        assert ours.xpath(query).getall() == theirs.xpath(query).getall(), query


# --- found in review ----------------------------------------------------------------


def page_of(body, content_type=None):
    headers = [("content-type", content_type)] if content_type else []
    return netweir.Page(_response("https://example.com/f", 200, "HTTP/1.1", headers, body))


@pytest.mark.parametrize(
    "body, kind",
    [
        (b"<rss><channel><item><link>/1</link></item></channel></rss>", "xml"),
        (b'\n<?xml version="1.0"?><feed/>', "xml"),
        (b"<!-- a comment --><urlset/>", "xml"),
        (b'<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"/>', "html"),
        (b"<!DOCTYPE html><title>t</title>", "html"),
        (b"<div>hi</div>", "html"),
        (b"just text", "html"),
    ],
)
def test_a_body_with_no_type_is_read_as_it_starts(body, kind):
    for content_type in [None, "application/octet-stream"]:
        page = page_of(body, content_type)
        assert page.root.type == kind, (body, content_type)
        assert page._response.markup == kind


def test_recover_repairs_and_warns(caplog):
    broken = "<rss><channel><item><title>a&nbsp;b & c\x0b</title></item></channel></rss><b>PHP"
    with pytest.raises(netweir.XMLError):
        netweir.parse(broken, type="xml")
    with caplog.at_level("WARNING", logger="netweir"):
        root = netweir.parse(broken, type="xml", recover=True)
    assert root.xpath("//title/text()").get() == "a\xa0b & c"
    assert "repaired 4 problems in the XML" in caplog.text
    assert "line 1, column" in caplog.text
    caplog.clear()
    # Nothing repaired, nothing said.
    with caplog.at_level("WARNING", logger="netweir"):
        netweir.parse("<r/>", type="xml", recover=True)
    assert caplog.text == ""


def test_an_empty_document_is_an_error_unless_recovering():
    for empty in ["", "  \n"]:
        with pytest.raises(netweir.XMLError, match="Document is empty"):
            netweir.parse(empty, type="xml")
        assert netweir.parse(empty, type="xml", recover=True).xpath("//*").getall() == []


def test_a_response_parses_in_recover_mode_when_asked(caplog):
    page = page_of(b"<r>&nbsp;</r>", "application/xml")
    with pytest.raises(netweir.XMLError):
        page._response.parse()
    with caplog.at_level("WARNING", logger="netweir"):
        root = page._response.parse(recover=True)
    assert root.xpath("/r/text()").get() == "\xa0"
    assert "https://example.com/f: repaired 1 problem" in caplog.text


def test_page_replace_with_text_keeps_the_declared_encoding_true():
    # No Content-Type: the XML declaration names the encoding, and the
    # new body is encoded in it.
    body = '<?xml version="1.0" encoding="ISO-8859-1"?><r>Zoë</r>'.encode("latin-1")
    page = page_of(body)
    assert page.encoding == "windows-1252"
    new = page.replace(body=page.text.replace("Zoë", "Zoé &amp; Ann"))
    assert new.xpath("/r/text()").get() == "Zoé & Ann"
    # Nor does a UTF-16 page lose its way.
    u16 = page_of('\ufeff<?xml version="1.0" encoding="UTF-16"?><r>a</r>'.encode("utf-16-le"))
    assert u16.replace(body=u16.text.replace(">a<", ">ä<")).xpath("/r/text()").get() == "ä"


def test_utf16_without_a_byte_order_mark_is_read_from_the_declaration():
    text = '<?xml version="1.0" encoding="UTF-16"?><r><t>Zoë – €</t></r>'
    for codec in ["utf-16-le", "utf-16-be", "utf-32-le", "utf-32-be"]:
        page = page_of(text.encode(codec), "application/xml")
        assert page.xpath("//t/text()").get() == "Zoë – €", codec


def test_css_attribute_selectors_find_html_names_on_xml():
    page = page_of(
        b'<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><body>'
        b'<a href="/p/1" title="One">One</a><img src="/i.png" alt="x"/></body></html>',
        "application/xml",
    )
    assert page.css("a[href]::attr(href)").getall() == ["/p/1"]
    assert page.css("img[src]::attr(alt)").getall() == ["x"]
    assert page.css("[title=One]::text").getall() == ["One"]


def test_a_selection_takes_namespaces_too():
    root = netweir.parse(ATOM, type="xml")
    entries = root.xpath("//*[local-name()='entry']")
    entries.register_namespace("a", ATOM_NS)
    assert entries.xpath("a:title/text()").getall() == ["One", "Two"]
    assert entries[0].xpath("a:title/text()").get() == "One"
    with pytest.raises(ValueError):
        entries.register_namespace("no good", ATOM_NS)
    entries.remove_namespaces()
    assert root.xpath("//entry/title/text()").getall() == ["One", "Two"]


def test_a_crawl_can_recover_its_xml(feeds, caplog):
    seen = []

    class Lenient(netweir.Spider):
        settings = netweir.Settings(
            throttle=False, start_delay=0, obey_tdmrep=False, xml_recover=True
        )
        start_urls = [f"{feeds}/nbsp", f"{feeds}/nbsp-rss"]

        def parse(self, page):
            seen.append(page.xpath("//title/text()").get())

    with caplog.at_level("WARNING", logger="netweir"):
        Lenient().run()
    assert seen == ["a\xa0b", "a\xa0b"]
    # One warning each, from the parse page.xpath() asks for.
    assert caplog.text.count("isn't declared; read as HTML's") == 2


@pytest.mark.skipif(sys.platform == "win32", reason="peak memory is read with resource")
def test_sitemaps_are_read_in_little_memory(tmp_path):
    """A 200,000-URL sitemap took 864 MB when it was parsed whole; read in
    one pass it's close to its own size."""
    import subprocess

    script = tmp_path / "rss.py"
    script.write_text(
        "import gzip, resource, sys\n"
        "from netweir._native import _sitemap\n"
        "rows = ''.join(f'<url><loc>https://www.example.com/products/item-{i}-with-a-long-slug</loc>'\n"
        '  f\'<lastmod>2026-01-01</lastmod><xhtml:link rel="alternate" hreflang="de" \'\n'
        "  f'href=\"https://www.example.com/de/item-{i}\"/></url>' for i in range(100_000))\n"
        "body = ('<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" '\n"
        "  'xmlns:xhtml=\"http://www.w3.org/1999/xhtml\">' + rows + '</urlset>').encode()\n"
        "gz = gzip.compress(body); size = len(body); del rows, body\n"
        "before = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss\n"
        "kind, entries, repaired = _sitemap(gz, 1 << 30)\n"
        "grew = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss - before\n"
        "scale = 1 if sys.platform == 'darwin' else 1024\n"
        "print(len(entries), grew * scale, size)\n"
    )
    out = subprocess.run(
        [sys.executable, str(script)], capture_output=True, text=True, check=True
    ).stdout.split()
    entries, grew, size = map(int, out)
    # Half that here, well inside a debug build's 10-second budget.
    assert entries == 100_000
    # The unzipped text, its decoding and the entries (as Python dicts,
    # most of it); the tree was over 15 times the size.
    assert grew < 10 * size, (grew, size)


@pytest.mark.parametrize(
    "broken, ours",
    [
        # Where lxml's recover mode and Netweir's agree.
        ("<r><a><b><c>x</a></r>", None),
        ("<rss><channel><item><title>A</title></item><item><title>B</channel></rss>", None),
        # Where they part: lxml ends the document at an end tag that
        # closes nothing, losing the rest; Netweir ignores the tag.
        ("<r><i>a</i></x><i>b</i></r>", "<r><i>a</i><i>b</i></r>"),
        # lxml drops a `<` that starts no markup; Netweir keeps it.
        ("<r>1 < 2</r>", "<r>1 &lt; 2</r>"),
        # lxml lets </a> close only <b>, and keeps y in <a>; Netweir
        # closes up to <a>, as its name says.
        ("<r><a><b>x</a>y</r>", "<r><a><b>x</b></a>y</r>"),
    ],
)
def test_recover_mode_against_lxmls(broken, ours):
    etree = pytest.importorskip("lxml.etree")
    theirs = etree.tostring(
        etree.fromstring(broken.encode(), etree.XMLParser(recover=True))
    ).decode()
    got = netweir.parse(broken, type="xml", recover=True).html
    assert got == (theirs if ours is None else ours)
    if ours is not None:
        assert got != theirs


def test_a_page_parsed_when_read_keeps_its_crawls_budget(feeds, caplog):
    """An XML page is parsed when the callback first reads it, and that
    parse has the crawl's budget, as the engine's own has: a defence
    against markup made to be slow."""
    counts = []

    class Reader(netweir.Spider):
        start_urls = [f"{feeds}/slow"]

        def parse(self, page):
            assert page._root is None
            counts.append(page.xpath("count(//a)").get())

    Reader.settings = netweir.Settings(
        throttle=False, start_delay=0, obey_tdmrep=False, parse_timeout=0.001
    )
    with caplog.at_level("ERROR", logger="netweir"):
        stats = Reader().run()
    assert counts == [] and stats["callback_errors"] == 1
    assert "ParseTimeout" in caplog.text and "parsing took longer than" in caplog.text
    Reader.settings = FAST
    Reader().run()
    assert counts == ["200000.0"]
    # A page made by hand has no budget, as before.
    page = netweir.Page(
        _response("https://x/", 200, "HTTP/1.1", [("content-type", "text/xml")], b"<r/>")
    )
    assert page._parse == (None, False)


def test_parse_timeout_is_positive_or_none():
    for bad in (-1, 0, 0.0):
        with pytest.raises(ValueError, match="parse_timeout"):
            netweir.Settings(parse_timeout=bad)
    assert netweir.Settings(parse_timeout=None).parse_timeout is None


def test_no_parse_timeout_means_no_limit(feeds):
    counts = []

    class Reader(netweir.Spider):
        start_urls = [f"{feeds}/slow"]
        settings = netweir.Settings(
            throttle=False, start_delay=0, obey_tdmrep=False, parse_timeout=None
        )

        def parse(self, page):
            assert page._parse == (None, False)
            counts.append(page.xpath("count(//a)").get())

    Reader().run()
    assert counts == ["200000.0"]


@pytest.mark.parametrize(
    "text, charset",
    [
        # Python's codecs and the WHATWG decoders disagree on these: Python
        # writes Ё in Big5 as bytes the decoder reads as シ, and 갂 in
        # EUC-KR as an eight-byte sequence of jamo.
        ("<r>Ё ¢ 繁體</r>", "big5"),
        ("<r>갂 한국어</r>", "euc-kr"),
        ("<r>¢ ¬ 日本語</r>", "shift_jis"),
    ],
)
def test_page_replace_encodes_as_the_page_is_decoded(text, charset):
    page = page_of(b"<r>old</r>", f"text/xml; charset={charset}")
    new = page.replace(body=text)
    assert new.text == text
    assert new.xpath("/r/text()").get() == text[3:-4]


def test_threads_reading_a_page_at_once_parse_it_once():
    body = (
        "<rss><channel>" + "".join(f"<item><t>{i}</t></item>" for i in range(20000))
    ).encode() + b"</channel></rss>"
    for _ in range(10):
        page = page_of(body, "application/rss+xml")
        roots = [None] * 8
        barrier = threading.Barrier(8)

        def read(i, page=page, barrier=barrier, roots=roots):
            barrier.wait()
            roots[i] = page.root

        threads = [threading.Thread(target=read, args=(i,)) for i in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        assert all(r is roots[0] for r in roots)


def test_the_document_node_is_no_xpath_result_in_xml():
    # As in parsel: from the root element, or from a node read on its own,
    # there's nothing above.
    root = netweir.parse("<r><a>x</a></r>", type="xml")
    assert root.xpath("/*/..").getall() == []
    assert root.xpath("/").getall() == []
    assert root.xpath("//a/..").getall() == ["<r><a>x</a></r>"]
    assert root.xpath("count(/*/..)").get() == "1.0"
    from netweir._native import _xml_nodes

    (node,) = _xml_nodes("<r><i>1</i></r>", "i", [])
    assert node.xpath("..").getall() == []
    assert node.xpath(".").getall() == ["<i>1</i>"]
    # HTML keeps its document node, as Netweir always gave it.
    assert netweir.parse("<p>x</p>").xpath("/html/..").getall() != []


def test_reading_a_big_feed_or_sitemap_leaves_the_crawl_running(feeds):
    """Sitemaps and iternodes are read on threads of their own: the event
    loop goes on answering while they're read."""
    import asyncio
    import gzip
    import time

    items = "".join(f"<item><t>{'< ' * 3000}{i}</t></item>" for i in range(300))
    Feeds.pages["/bigfeed"] = (
        f"<rss><channel>{items}</channel></rss>".encode(),
        "application/rss+xml",
    )
    rows = "".join(
        f"<url><loc>https://e.invalid/{i}</loc><x>{'< ' * 40}</x></url>" for i in range(20_000)
    )
    Feeds.pages["/bigmap.xml"] = (
        gzip.compress(f"<urlset>{rows}</urlset>".encode()),
        "application/gzip",
    )
    nodes = []

    class Big(netweir.XMLFeedSpider):
        start_urls = [f"{feeds}/bigfeed"]
        sitemap_urls = [f"{feeds}/bigmap.xml"]
        sitemap_rules = []
        recover = True
        settings = netweir.Settings(throttle=False, start_delay=0, obey_tdmrep=False)

        def parse_node(self, page, node):
            nodes.append(1)

    async def main():
        gaps = []
        done = asyncio.Event()

        async def tick():
            last = time.perf_counter()
            while not done.is_set():
                await asyncio.sleep(0.005)
                now = time.perf_counter()
                gaps.append(now - last)
                last = now

        ticker = asyncio.create_task(tick())
        started = time.perf_counter()
        await Big().crawl()
        took = time.perf_counter() - started
        done.set()
        await ticker
        return took, max(gaps)

    took, worst = asyncio.run(main())
    assert len(nodes) == 300
    # The reading took a while, and the loop was never held for long.
    assert worst < max(0.25, took / 4), (took, worst)


def test_a_logging_filter_may_read_the_page_whose_repairs_it_logs():
    """Page.root logged what recovery repaired while it held the page's
    lock, so a filter that read the page waited on itself for ever."""
    import logging

    page = netweir.Page(
        _response(
            "https://example.com/f",
            200,
            "HTTP/1.1",
            [("content-type", "text/xml")],
            b"<r>1 < 2</r>",
        ),
        parse=(None, True),
    )
    seen = []

    class Peek(logging.Filter):
        def filter(self, record):
            seen.append(page.xpath("string(/r)").get())
            return True

    peek = Peek()
    logging.getLogger("netweir").addFilter(peek)
    try:
        reader = threading.Thread(target=lambda: page.root, daemon=True)
        reader.start()
        reader.join(10)
        assert not reader.is_alive(), "Page.root deadlocked on its own lock"
    finally:
        logging.getLogger("netweir").removeFilter(peek)
    assert seen == ["1 < 2"]


def test_a_cancelled_feed_reads_no_more_nodes():
    """A cancelled iternodes crawl read on through the rest of its batch,
    32 nodes of up to 10 seconds each, and the crawl waited for it to
    finish. Now the batch stops at the node it's on. (A callback that
    stops reading leaves no batch running: one is read only when asked.)"""
    import asyncio
    import time

    items = "".join(f"<item><t>{'<' * 300_000}{i}</t></item>" for i in range(40))
    page = page_of(f"<rss><channel>{items}</channel></rss>".encode(), "application/rss+xml")

    class Feed(netweir.XMLFeedSpider):
        recover = True

    spider = Feed()

    async def read(n):
        got = 0
        async for _ in spider._nodes(page):
            got += 1
            if got == n:
                break
        return got

    started = time.perf_counter()
    assert asyncio.run(read(32)) == 32
    batch = time.perf_counter() - started

    async def cancelled():
        task = asyncio.create_task(read(10_000))
        await asyncio.sleep(batch / 16)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        return time.perf_counter()

    # asyncio.run waits for the batch's thread before it returns.
    done = asyncio.run(cancelled())
    waited = time.perf_counter() - done
    # Before, the rest of the batch: most of `batch`. Now a node or so.
    assert waited < batch / 4, (waited, batch)


def test_a_cancelled_sitemap_is_read_no_further():
    """Cancelled, the crawl stopped waiting for a sitemap, but the reader
    read on to its end, up to 10 seconds, and the crawl's shutdown waited
    for it. Now it gives up within a few lines."""
    import asyncio
    import time

    rows = "".join(f"<url><loc>https://e/{i}{'<' * 200}</loc></url>" for i in range(40_000))
    page = page_of(f"<urlset>{rows}</urlset>".encode(), "application/xml")
    spider = netweir.Spider()

    async def read():
        return [r async for r in spider._sitemap(page)]

    started = time.perf_counter()
    assert len(asyncio.run(read())) == 40_000
    whole = time.perf_counter() - started

    async def cancelled():
        task = asyncio.create_task(read())
        await asyncio.sleep(whole / 16)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        return time.perf_counter()

    done = asyncio.run(cancelled())
    waited = time.perf_counter() - done
    assert waited < whole / 4, (waited, whole)


def test_an_id_a_prefix_came_off_is_the_element_s_id():
    # remove_namespaces makes a:id the element's id, as if written so: #p
    # finds it, and [id] reads it rather than the ID beside it.
    for src in (
        '<r xmlns:a="u"><x ID="up" a:id="p">1</x></r>',
        '<r xmlns:a="u"><x a:ID="up" id="p">1</x></r>',
    ):
        doc = netweir.parse(src, type="xml")
        doc.remove_namespaces()
        assert doc.css("#p::text").getall() == ["1"], src
        assert doc.css("[id=p]::text").getall() == ["1"], src
        assert doc.css("[id=up]::text").getall() == [], src
        assert doc.xpath("//x/@ID").getall() == ["up"], src
