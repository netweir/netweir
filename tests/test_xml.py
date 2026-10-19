"""XML documents from Python: parsing, namespaces, the type a response is
read as, and the limits on hostile XML. Answers are checked against
parsel's, which Scrapy's selectors are."""

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
        (None, "html"),
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
    assert new.headers["content-type"] == "text/xml; charset=utf-8"
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
