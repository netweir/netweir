"""JSON and XML files, cloud storage through fsspec, and batches."""

import datetime
import decimal
import json
import pathlib
import re
import sys
import threading
import xml.etree.ElementTree as ET
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir
from netweir import export

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        body = b"<a href='/2'>2</a><a href='/3'>3</a>" if self.path == "/1" else b"<p>x</p>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


def three_pages(base, **attrs):
    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/1"]

        def parse(self, page):
            yield {"url": page.url}
            yield from page.follow_all(page.css("a::attr(href)"))

    for name, value in attrs.items():
        setattr(Site, name, value)
    return Site


def write(exporter, items):
    for item in items:
        exporter(item)
    exporter.close()


# --- JSON ----------------------------------------------------------------------


def test_json_writes_one_array_an_item_a_line(tmp_path):
    path = tmp_path / "items.json"
    items = [
        {"title": "A", "price": decimal.Decimal("1.50"), "on": datetime.date(2026, 10, 12)},
        {"title": "B", "tags": ["x", "y"], "n": None},
    ]
    write(export.json(str(path)), items)
    text = path.read_text()
    assert json.loads(text) == [
        {"title": "A", "price": "1.50", "on": "2026-10-12"},
        {"title": "B", "tags": ["x", "y"], "n": None},
    ]
    assert text.splitlines()[0] == "[" and text.splitlines()[-1] == "]"
    assert len(text.splitlines()) == 4


def test_an_empty_json_file_is_an_empty_array(tmp_path):
    path = tmp_path / "items.json"
    export.json(str(path)).close()
    assert json.loads(path.read_text()) == []


@pytest.mark.parametrize("closed", [True, False])
def test_json_resumes_after_the_last_item(tmp_path, closed):
    path = tmp_path / "items.json"
    first = export.json(str(path))
    first.start()
    first({"n": 1})
    first({"n": 2})
    if closed:
        first.close()  # recorded with its closing ]
    else:
        first.flush()  # recorded mid-crawl, without one
        first._writer = None
    again = export.json(str(path))
    again.start(append=True)
    again({"n": 3})
    again.close()
    assert json.loads(path.read_text()) == [{"n": 1}, {"n": 2}, {"n": 3}]


def test_json_resumes_an_empty_array(tmp_path):
    path = tmp_path / "items.json"
    export.json(str(path)).close()
    again = export.json(str(path))
    again.start(append=True)
    again({"n": 1})
    again.close()
    assert json.loads(path.read_text()) == [{"n": 1}]


def test_a_crawl_writes_json_with_o(base, tmp_path):
    out = tmp_path / "items.json"
    three_pages(base)().run(output=str(out))
    assert sorted(i["url"] for i in json.loads(out.read_text())) == [
        base + "/1",
        base + "/2",
        base + "/3",
    ]


def test_a_resumed_crawl_keeps_the_json_valid(base, tmp_path):
    out = tmp_path / "items.json"
    Site = three_pages(base)
    Site.settings = netweir.Settings(**{**vars(FAST), "checkpoint": str(tmp_path / "state")})
    Site.settings = netweir.Settings(**{**vars(Site.settings), "max_items": 1})
    Site().run(output=str(out))
    Site.settings = netweir.Settings(**{**vars(Site.settings), "max_items": None})
    Site().run(output=str(out))
    assert len(json.loads(out.read_text())) == 3


# --- XML -----------------------------------------------------------------------


def test_xml_writes_scrapy_s_layout(tmp_path):
    path = tmp_path / "items.xml"
    write(
        export.xml(str(path)),
        [{"title": "A & B <1>", "tags": ["x", "y"], "n": None, "ok": True, "price": 1.5}],
    )
    text = path.read_text(encoding="utf-8")
    assert text.startswith('<?xml version="1.0" encoding="utf-8"?>\n<items>\n<item>')
    assert text.endswith("</item>\n</items>\n")
    root = ET.fromstring(text.encode())
    [item] = root
    assert item.tag == "item"
    assert item.find("title").text == "A & B <1>"
    assert [v.text for v in item.find("tags")] == ["x", "y"]
    assert [v.tag for v in item.find("tags")] == ["value", "value"]
    assert item.find("n").text is None
    assert item.find("ok").text == "true"
    assert item.find("price").text == "1.5"


def test_xml_nests_dicts_and_names_its_elements(tmp_path):
    path = tmp_path / "books.xml"
    write(
        export.xml(str(path), root="books", item="book"),
        [{"unit price": 3, "9lives": 1, "seller": {"name": "N", "rating": 4}}],
    )
    root = ET.fromstring(path.read_bytes())
    assert root.tag == "books"
    [book] = root
    assert book.tag == "book"
    assert book.find("unit_price").text == "3"
    assert book.find("_9lives").text == "1"
    assert book.find("seller/name").text == "N"


def test_xml_drops_characters_xml_can_t_hold(tmp_path):
    path = tmp_path / "items.xml"
    write(export.xml(str(path)), [{"text": "a\x00b\x1bc\td"}])
    assert ET.fromstring(path.read_bytes())[0].find("text").text == "abc\td"


def test_an_empty_xml_file_is_an_empty_root(tmp_path):
    path = tmp_path / "items.xml"
    export.xml(str(path)).close()
    assert len(ET.fromstring(path.read_bytes())) == 0


@pytest.mark.parametrize("closed", [True, False])
def test_xml_resumes_after_the_last_item(tmp_path, closed):
    path = tmp_path / "items.xml"
    first = export.xml(str(path))
    first.start()
    first({"n": 1})
    if closed:
        first.close()
    else:
        first.flush()
        first._writer = None
    again = export.xml(str(path))
    again.start(append=True)
    again({"n": 2})
    again.close()
    assert [i.find("n").text for i in ET.fromstring(path.read_bytes())] == ["1", "2"]


def test_to_path_knows_json_and_xml(tmp_path):
    assert isinstance(export.to_path(str(tmp_path / "a.json")), export.json)
    assert isinstance(export.to_path(str(tmp_path / "a.XML")), export.xml)
    with pytest.raises(ValueError, match=r"\.json.*\.xml"):
        export.to_path(str(tmp_path / "a.txt"))


# --- batches -------------------------------------------------------------------


def test_batches_start_a_new_file_every_n_items(tmp_path):
    exporter = export.jsonl(str(tmp_path / "items-{batch}.jsonl"), batch=2)
    write(exporter, [{"n": n} for n in range(5)])
    files = sorted(p.name for p in tmp_path.iterdir())
    assert files == ["items-1.jsonl", "items-2.jsonl", "items-3.jsonl"]
    assert (tmp_path / "items-3.jsonl").read_text() == '{"n":4}\n'


def test_each_csv_batch_has_its_header(tmp_path):
    write(export.csv(str(tmp_path / "b{batch}.csv"), batch=1), [{"a": 1}, {"a": 2}])
    assert (tmp_path / "b1.csv").read_text().splitlines() == ["a", "1"]
    assert (tmp_path / "b2.csv").read_text().splitlines() == ["a", "2"]


def test_parquet_and_json_batches(tmp_path):
    pq = pytest.importorskip("pyarrow.parquet")
    write(export.parquet(str(tmp_path / "p{batch}.parquet"), batch=2), [{"a": i} for i in range(3)])
    assert pq.read_table(tmp_path / "p1.parquet").num_rows == 2
    assert pq.read_table(tmp_path / "p2.parquet").num_rows == 1
    write(export.json(str(tmp_path / "j{batch}.json"), batch=2), [{"a": i} for i in range(3)])
    assert json.loads((tmp_path / "j2.json").read_text()) == [{"a": 2}]


def test_batch_time_is_safe_in_a_file_name(tmp_path):
    write(export.jsonl(str(tmp_path / "items-{batch}-{time}.jsonl"), batch=10), [{"n": 1}])
    [path] = tmp_path.iterdir()
    assert re.fullmatch(r"items-1-\d{4}-\d\d-\d\dT\d\d-\d\d-\d\dZ\.jsonl", path.name)


def test_an_empty_crawl_still_writes_the_first_batch(tmp_path):
    export.jsonl(str(tmp_path / "items-{batch}.jsonl"), batch=10).close()
    assert (tmp_path / "items-1.jsonl").read_text() == ""


def test_batch_needs_a_placeholder_in_the_path(tmp_path):
    with pytest.raises(ValueError, match=r"\{batch\}"):
        export.jsonl(str(tmp_path / "items.jsonl"), batch=10)
    # {time} alone isn't enough: two batches in the same second would share
    # a name, and the second would overwrite the first.
    with pytest.raises(ValueError, match=r"\{batch\}"):
        export.jsonl(str(tmp_path / "items-{time}.jsonl"), batch=10)
    with pytest.raises(ValueError, match="braces"):
        export.jsonl(str(tmp_path / "data{x}-{batch}.jsonl"), batch=10)
    # Doubled braces are kept as they are.
    write(export.jsonl(str(tmp_path / "{{raw}}-{batch}.jsonl"), batch=1), [{"n": 1}])
    assert (tmp_path / "{raw}-1.jsonl").exists()
    with pytest.raises(ValueError, match="batch"):
        export.jsonl(str(tmp_path / "items-{batch}.jsonl"), batch=0)


def test_batches_and_checkpoints_don_t_mix(base, tmp_path):
    Site = three_pages(base)
    Site.settings = netweir.Settings(**{**vars(FAST), "checkpoint": str(tmp_path / "state")})
    exporter = export.jsonl(str(tmp_path / "items-{batch}.jsonl"), batch=2)
    with pytest.raises(ValueError, match="checkpoint"):
        Site().run(output=exporter)
    assert not list(tmp_path.glob("items-*"))


def test_a_crawl_with_batches_from_the_command_line(base, tmp_path):
    from netweir import _cli

    spider = tmp_path / "site.py"
    spider.write_text(
        "import netweir\n"
        "class Site(netweir.Spider):\n"
        f"    start_urls = [{base + '/1'!r}]\n"
        "    def parse(self, page):\n"
        "        yield {'url': page.url}\n"
        "        yield from page.follow_all(page.css('a::attr(href)'))\n"
    )
    out = str(tmp_path / "items-{batch}.jsonl")
    argv = ["crawl", str(spider), "-q", "-o", out, "--batch", "2"]
    argv += ["-s", "throttle=false", "-s", "start_delay=0", "-s", "obey_robots=false"]
    assert _cli.main(argv) == 0
    assert sorted(p.name for p in tmp_path.glob("items-*")) == ["items-1.jsonl", "items-2.jsonl"]


# --- cloud storage -------------------------------------------------------------


@pytest.fixture
def memory():
    fsspec = pytest.importorskip("fsspec")
    fs = fsspec.filesystem("memory")
    fs.store.clear()
    yield fs
    fs.store.clear()


def test_a_remote_path_is_uploaded_when_closed(memory, tmp_path):
    exporter = export.jsonl("memory://bucket/items.jsonl")
    exporter({"n": 1})
    assert not memory.exists("/bucket/items.jsonl")  # not until it's finished
    exporter.close()
    assert memory.cat_file("/bucket/items.jsonl") == b'{"n":1}\n'


def test_remote_batches_and_parquet(memory):
    pq = pytest.importorskip("pyarrow.parquet")
    write(export.jsonl("memory://b/i-{batch}.jsonl", batch=1), [{"n": 1}, {"n": 2}])
    assert memory.cat_file("/b/i-2.jsonl") == b'{"n":2}\n'
    write(export.parquet("memory://b/p.parquet"), [{"n": 1}])
    with memory.open("/b/p.parquet", "rb") as f:
        assert pq.read_table(f).num_rows == 1


def test_a_crawl_writes_to_a_remote_path(base, memory):
    three_pages(base)().run(output="memory://b/items.json")
    assert len(json.loads(memory.cat_file("/b/items.json"))) == 3


def test_remote_paths_and_checkpoints_don_t_mix(base, memory, tmp_path):
    Site = three_pages(base)
    Site.settings = netweir.Settings(**{**vars(FAST), "checkpoint": str(tmp_path / "state")})
    with pytest.raises(ValueError, match="checkpoint"):
        Site().run(output="memory://b/items.jsonl")


def test_without_fsspec_a_remote_path_says_what_to_install(monkeypatch):
    monkeypatch.setitem(sys.modules, "fsspec", None)
    with pytest.raises(ImportError, match="pip install fsspec"):
        export.jsonl("s3://bucket/items.jsonl")


# --- second look ---------------------------------------------------------------


def test_xml_names_follow_xml_s_own_rules(tmp_path):
    path = tmp_path / "items.xml"
    keys = ["µg", "ºC", "x²", "¼", "\u0345start", "Ⅻ", "a:b", "", "ok-name.1", "Prix €"]
    write(export.xml(str(path)), [{k: i for i, k in enumerate(keys)}])
    # ElementTree's parser, expat, keeps to XML's older, stricter rules for
    # names: that it parses means every parser will.
    [item] = ET.fromstring(path.read_bytes())
    assert [child.text for child in item] == [str(i) for i in range(len(keys))]
    tags = [child.tag for child in item]
    assert tags[6] == "a_b" and tags[7] == "_" and tags[8] == "ok-name.1"
    assert tags[9] == "Prix__"  # space and € aren't name characters
    assert "é" in [c.tag for c in ET.fromstring(_xml_of(tmp_path, {"é": 1}))[0]]


def _xml_of(tmp_path, item):
    path = tmp_path / "one.xml"
    write(export.xml(str(path)), [item])
    return path.read_bytes()


def test_xml_root_and_item_must_differ(tmp_path):
    with pytest.raises(ValueError, match="different names"):
        export.xml(str(tmp_path / "a.xml"), root="item", item="item").close()


def test_resuming_someone_else_s_file_is_refused(tmp_path):
    path = tmp_path / "items.xml"
    write(export.xml(str(path), root="books", item="book"), [{"n": 1}])
    other = export.xml(str(path), root="items")
    with pytest.raises(ValueError, match="doesn't start"):
        other.start(append=True)
    jsonl_file = tmp_path / "items.json"
    jsonl_file.write_text('{"n": 1}\n')
    with pytest.raises(ValueError, match="doesn't start"):
        export.json(str(jsonl_file)).start(append=True)


def test_a_json_list_item_isn_t_taken_for_the_end(tmp_path):
    path = tmp_path / "items.json"
    first = export.json(str(path))
    first.start()
    first([1])  # ends with ], as the file's closing does
    first.flush()
    first._writer = None  # recorded mid-crawl
    again = export.json(str(path))
    again.start(append=True)
    again([2])
    again.close()
    assert json.loads(path.read_text()) == [[1], [2]]


def test_closing_twice_does_nothing(tmp_path):
    path = tmp_path / "items.jsonl"
    exporter = export.jsonl(str(path))
    exporter({"n": 1})
    exporter.close()
    exporter.close()
    assert path.read_text() == '{"n":1}\n'


def test_a_bad_scheme_or_missing_package_fails_at_once():
    pytest.importorskip("fsspec")
    with pytest.raises(ValueError, match="nosuch"):
        export.jsonl("nosuch://bucket/items.jsonl")
    try:
        import s3fs  # noqa: F401
    except ImportError:
        with pytest.raises(ImportError, match="s3"):
            export.jsonl("s3://bucket/items.jsonl")


def test_a_failed_upload_keeps_the_file_and_closes_the_rest(base, memory, tmp_path, monkeypatch):
    local_out = tmp_path / "local.jsonl"
    closed = []

    class Store:
        def process_item(self, item, spider):
            return item

        def close_spider(self, spider):
            closed.append(True)

    remote = export.jsonl("memory://b/items.jsonl")
    fsspec = pytest.importorskip("fsspec")

    def refuse(*args, **kwargs):
        raise PermissionError("bucket says no")

    monkeypatch.setattr(fsspec, "open", refuse)
    Site = three_pages(base, pipelines=[remote, export.jsonl(str(local_out)), Store()])
    with pytest.raises(OSError, match="bucket says no") as caught:
        Site().run()
    kept = pathlib.Path(re.search(r"the file is at (\S+)$", str(caught.value)).group(1))
    assert len(kept.read_text().splitlines()) == 3
    kept.unlink()
    assert len(local_out.read_text().splitlines()) == 3
    assert closed == [True]


def test_any_key_at_all_gives_a_name_every_parser_takes(tmp_path):
    keys = {}
    for cp in range(0x10000):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        keys[chr(cp) + "a"] = 1
        keys["a" + chr(cp)] = 1
    ET.fromstring(_xml_of(tmp_path, keys))  # one bad name and this fails


# --- third look ----------------------------------------------------------------


def test_a_failed_upload_isn_t_swallowed_when_run_is_called_in_an_except(base, memory, monkeypatch):
    fsspec = pytest.importorskip("fsspec")
    monkeypatch.setattr(fsspec, "open", lambda *a, **k: (_ for _ in ()).throw(OSError("no")))
    Site = three_pages(base, pipelines=[export.jsonl("memory://b/items.jsonl")])
    try:
        raise RuntimeError("an earlier attempt failed")
    except RuntimeError:
        # A retry loop: the caller is handling an exception of its own.
        with pytest.raises(OSError, match="the file is at") as caught:
            Site().run()
    pathlib.Path(re.search(r"the file is at (\S+)$", str(caught.value)).group(1)).unlink()


def test_checkpoint_conflicts_are_one_line_on_the_command_line(base, tmp_path, capsys):
    from netweir import _cli

    spider = tmp_path / "site.py"
    spider.write_text(
        "import netweir\n"
        "class Site(netweir.Spider):\n"
        f"    start_urls = [{base + '/1'!r}]\n"
        "    def parse(self, page):\n"
        "        yield {'url': page.url}\n"
    )
    state = tmp_path / "cp"
    argv = ["crawl", str(spider), "-o", str(tmp_path / "o{batch}.jsonl"), "--batch", "2"]
    assert _cli.main([*argv, "-s", f"checkpoint={state}"]) == 2
    err = capsys.readouterr().err
    assert err.startswith("netweir: ") and err.count("\n") == 1 and "checkpoint" in err
    assert not state.exists()  # nothing left behind


def test_a_doubled_batch_brace_isn_t_the_placeholder(tmp_path):
    with pytest.raises(ValueError, match=r"\{batch\}"):
        export.jsonl(str(tmp_path / "o{{batch}}.jsonl"), batch=2)
    # A format spec is fine.
    write(export.jsonl(str(tmp_path / "o{batch:03}.jsonl"), batch=1), [{"n": 1}])
    assert (tmp_path / "o001.jsonl").exists()


def test_close_twice_then_start_again(tmp_path):
    path = tmp_path / "items.jsonl"
    exporter = export.jsonl(str(path))
    exporter.start()
    exporter({"n": 1})
    exporter.close()
    exporter.close()
    exporter.start()
    exporter({"n": 2})
    exporter.close()
    assert path.read_text() == '{"n":2}\n'


def test_resume_refuses_an_output_file_changed_since(base, tmp_path):
    out = tmp_path / "items.json"
    Site = three_pages(base)
    Site.settings = netweir.Settings(
        **{**vars(FAST), "checkpoint": str(tmp_path / "state"), "max_items": 1}
    )
    Site().run(output=str(out))
    out.write_text("[")  # shorter than recorded: cutting it back would pad it
    with pytest.raises(ValueError, match="shorter"):
        Site().run(output=str(out))
    out.write_text("not this crawl's file, and longer than it was" * 3)
    with pytest.raises(ValueError, match="replaced"):
        Site().run(output=str(out))
    assert out.read_text().startswith("not this crawl's")  # left as it was
