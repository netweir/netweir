"""netweir mcp: the tools an agent gets, driven through an in-memory MCP
client against a local site."""

import asyncio
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

pytest.importorskip("mcp")

from mcp import Client  # noqa: E402

from netweir import _mcp  # noqa: E402

PRODUCT = """<html><head><title>Arc lamp</title>
<script type="application/ld+json">{"@context": "https://schema.org", "@type": "Product",
"name": "Arc lamp", "offers": {"@type": "Offer", "price": "49.00", "priceCurrency": "GBP"}}</script>
</head><body><nav><a href="/">Home</a> <a href="/lamps">Lamps</a> <a href="/lamps">Lamps</a>
<a href="mailto:hi@lumo.example">Mail</a></nav>
<main><h1>Arc lamp</h1><p class="lead">A tall lamp that leans over a sofa or a desk, made
of brushed steel, with a linen shade and a dimmer on the cord.</p>
<h2>Care</h2><p>Dust the shade with a soft brush. Wipe the steel with a dry cloth, and never
use polish on it, as polish clouds the brushed finish over time.</p></main>
<footer>Lumo Ltd</footer></body></html>"""

PAGES = {"/arc": PRODUCT}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        found = PAGES.get(self.path)
        data = (found or "missing").encode()
        self.send_response(200 if found else 404)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


def call(tool, **arguments):
    async def go():
        async with Client(_mcp.server(timeout=10)) as client:
            return await client.call_tool(tool, arguments)

    return asyncio.run(go())


def ok(tool, **arguments):
    result = call(tool, **arguments)
    assert not result.is_error, result.content
    return result.structured_content


def test_the_tools_an_agent_sees():
    async def go():
        async with Client(_mcp.server()) as client:
            return await client.list_tools()

    tools = {t.name: t for t in asyncio.run(go()).tools}
    assert set(tools) == {"read_page", "chunks", "extract", "select", "links"}
    assert all(t.description for t in tools.values())


def test_read_page_gives_the_main_content_as_markdown(base):
    page = ok("read_page", url=f"{base}/arc")
    assert page["title"] == "Arc lamp"
    assert page["page_type"] == "product"
    assert page["markdown"].startswith("# Arc lamp")
    assert "Lumo Ltd" not in page["markdown"]
    assert page["truncated"] is False
    whole = ok("read_page", url=f"{base}/arc", main_only=False, max_chars=40)
    assert len(whole["markdown"]) == 40 and whole["truncated"] is True


def test_chunks_cut_the_page_at_its_headings(base):
    found = ok("chunks", url=f"{base}/arc", max_chars=200)["result"]
    assert [c["headings"] for c in found] == [["Arc lamp"], ["Arc lamp", "Care"]]
    assert all(len(c["text"]) <= 200 for c in found)


def test_extract_reads_what_the_page_is(base):
    got = ok("extract", url=f"{base}/arc")
    assert got["type"] == "product"
    assert (got["item"]["name"], got["item"]["price"]) == ("Arc lamp", "49.00")
    assert call("extract", url=f"{base}/arc", type="recipe").is_error


def test_select_and_links(base):
    assert ok("select", url=f"{base}/arc", css="h2::text")["result"] == ["Care"]
    assert ok("select", url=f"{base}/arc", css="p.lead", max_results=1)["result"][0].startswith(
        "A tall lamp"
    )
    links = ok("links", url=f"{base}/arc")["result"]
    assert [link["url"] for link in links] == [f"{base}/", f"{base}/lamps"]
    assert links[1]["text"] == "Lamps"


def test_a_page_that_isnt_there_is_a_tool_error(base):
    result = call("read_page", url=f"{base}/nowhere")
    assert result.is_error
    assert "404" in result.content[0].text
    assert call("select", url=f"{base}/arc", css="p[").is_error


def test_without_the_sdk_it_says_what_to_install(monkeypatch):
    monkeypatch.setitem(sys.modules, "mcp.server", None)
    with pytest.raises(ImportError, match=r"netweir\[mcp\]"):
        _mcp.server()
