import sys

import pytest

import netweir
from netweir._native import _response

DOCS = """<html><head><title>Docs</title></head><body>
<nav><a href="/">Home</a> <a href="/blog">Blog</a></nav>
<main><h1>Guide</h1>
<p>Welcome to the guide. It is long enough to be the main content, with plenty of words in it.</p>
<h2>Install</h2><p>Run the installer and wait for it to finish. Then <a href="/next">go on</a>.</p>
<pre><code class="language-sh">pip install netweir
netweir version</code></pre>
<h2>Use</h2><p>Call get and read the page you get back.</p></main>
<footer>Docs Ltd</footer></body></html>"""


def page(html=DOCS, url="https://docs.example/guide"):
    return netweir.Page(
        _response(url, 200, "HTTP/1.1", [("content-type", "text/html")], html.encode())
    )


def test_a_page_is_cut_at_its_headings_with_the_path_to_each():
    chunks = page().chunks()
    assert [c.headings for c in chunks] == [
        ("Guide",),
        ("Guide", "Install"),
        ("Guide", "Use"),
    ]
    assert chunks[1].text == (
        "## Install\n\nRun the installer and wait for it to finish. Then "
        "[go on](https://docs.example/next).\n\n```sh\npip install netweir\nnetweir version\n```"
    )
    assert [c.index for c in chunks] == [0, 1, 2]
    assert {c.url for c in chunks} == {"https://docs.example/guide"}
    assert "Home" not in "".join(c.text for c in chunks), "the main content only"


def test_every_chunk_is_within_the_limit():
    for max_chars in (40, 80, 200):
        for c in page().chunks(max_chars, overlap=max_chars // 4):
            assert 0 < len(c.text) <= max_chars, c.text


def test_ids_are_the_same_from_crawl_to_crawl_and_differ_by_page():
    first = [c.id for c in page().chunks()]
    assert first == [c.id for c in page().chunks()]
    assert len(set(first)) == 3
    elsewhere = [c.id for c in page(url="https://docs.example/other").chunks()]
    assert not set(first) & set(elsewhere)


def test_a_page_without_main_content_is_cut_whole():
    listing = (
        "<ul>" + "".join(f"<li><a href='/p/{i}'>Item {i}</a></li>" for i in range(30)) + "</ul>"
    )
    chunks = page(f"<html><body>{listing}</body></html>").chunks(100)
    assert chunks
    assert "Item 0" in chunks[0].text
    assert "Item 29" in chunks[-1].text


def test_the_whole_page_when_asked():
    text = "".join(c.text for c in page().chunks(main_only=False))
    assert "Home" in text and "Docs Ltd" in text


def test_sizes_are_checked():
    for bad in ({"max_chars": 0}, {"max_chars": 1.5}, {"max_chars": 10, "overlap": 10}):
        with pytest.raises(ValueError):
            page().chunks(**bad)
    with pytest.raises(ValueError):
        netweir.chunks(overlap=-1)


def test_a_chunk_turns_into_a_dict():
    chunk = page().chunks()[0]
    assert chunk.to_dict() == {
        "id": chunk.id,
        "text": chunk.text,
        "headings": ["Guide"],
        "index": 0,
        "url": "https://docs.example/guide",
    }


def test_an_item_field_holds_the_same_chunks():
    class Doc(netweir.Item):
        title = netweir.css("title::text")
        chunks = netweir.chunks()
        steps = netweir.chunks("pre", main_only=False)
        missing = netweir.chunks("article", default=[])

    item = Doc.extract(page())
    assert item["chunks"] == [c.to_dict() for c in page().chunks()]
    assert [c["text"] for c in item["steps"]] == [
        "```sh\npip install netweir\nnetweir version\n```"
    ]
    assert item["missing"] == []


def test_a_field_needs_an_element():
    with pytest.raises(ValueError, match="made from an element"):
        netweir.chunks("h1::text")


def test_langchain_and_llamaindex_get_the_id_and_metadata():
    chunk = page().chunks()[1]
    try:
        document = chunk.to_langchain()
    except ImportError:
        pass
    else:
        assert (document.id, document.page_content) == (chunk.id, chunk.text)
        assert document.metadata["headings"] == ["Guide", "Install"]
    try:
        node = chunk.to_llamaindex()
    except ImportError:
        pass
    else:
        assert (node.id_, node.text) == (chunk.id, chunk.text)
        assert node.metadata["url"] == "https://docs.example/guide"


def test_a_missing_framework_says_what_to_install(monkeypatch):
    monkeypatch.setitem(sys.modules, "langchain_core.documents", None)
    monkeypatch.setitem(sys.modules, "llama_index.core.schema", None)
    chunk = page().chunks()[0]
    with pytest.raises(ImportError, match="pip install langchain-core"):
        chunk.to_langchain()
    with pytest.raises(ImportError, match="pip install llama-index-core"):
        chunk.to_llamaindex()
