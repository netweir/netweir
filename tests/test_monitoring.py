"""Validating items, field coverage, and checks at the end of a crawl."""

import dataclasses
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/":
            body = "".join(f"<a href='/{n}'>{n}</a>" for n in range(1, 5)).encode()
        elif self.path == "/broken":
            self.send_response(500)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        else:
            body = f"<p>{self.path}</p>".encode()
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


def shop(base, make_item, **attrs):
    """Five pages; each page's item is make_item(n), n = 0 for the start."""

    class Shop(netweir.Spider):
        settings = FAST
        start_urls = [base + "/"]

        def parse(self, page):
            n = int(page.url.rsplit("/", 1)[1] or 0)
            yield make_item(n)
            yield from page.follow_all(page.css("a::attr(href)"))

    for name, value in attrs.items():
        setattr(Shop, name, value)
    return Shop


def keep(into):
    """A pipeline stage that collects items and passes them on."""

    def stage(item):
        into.append(item)
        return item

    return stage


SCHEMA = {
    "type": "object",
    "required": ["title", "price"],
    "properties": {"title": {"type": "string"}, "price": {"type": "number"}},
}


# --- validation ----------------------------------------------------------------


def test_json_schema_drops_invalid_items_and_counts_them(base, caplog):
    pytest.importorskip("jsonschema")
    items = []

    def item(n):
        if n == 1:
            return {"title": "one"}  # no price
        if n == 2:
            return {"title": "two", "price": "cheap"}
        return {"title": str(n), "price": n}

    Shop = shop(base, item, pipelines=[netweir.validate(SCHEMA), keep(items)])
    spider = Shop()
    stats = spider.run()
    assert sorted(i["title"] for i in items) == ["0", "3", "4"]
    assert (stats["items"], stats["items_invalid"], stats["items_dropped"]) == (3, 2, 0)
    assert spider.invalid_fields == {"price": 2}
    # Logged once for the field, with whichever bad page came first.
    [logged] = [r.getMessage() for r in caplog.records if "invalid item" in r.getMessage()]
    assert logged.startswith(
        (f"invalid item from {base}/1: price", f"invalid item from {base}/2: price")
    )


@dataclasses.dataclass
class Book:
    title: str
    price: float
    stock: int | None = None
    tags: list[str] = dataclasses.field(default_factory=list)


def test_a_dataclass_is_a_schema(base):
    def item(n):
        return [
            {"title": "ok", "price": 3},  # an int is a float too
            {"title": "no price"},
            {"title": "bad stock", "price": 1.0, "stock": "many"},
            {"title": "bad tag", "price": 1.0, "tags": ["a", 2]},
            {"title": None, "price": 1.0},
        ][n]

    spider = shop(base, item, pipelines=[netweir.validate(Book)])()
    stats = spider.run()
    assert stats["items"] == 1
    assert spider.invalid_fields == {"price": 1, "stock": 1, "tags.*": 1, "title": 1}


def test_invalid_items_can_be_kept_with_their_errors(base):
    kept = []

    def item(n):
        return {"title": "x"} if n == 1 else {"title": "y", "price": 1.0}

    pipelines = [netweir.validate(Book, drop=False), keep(kept)]
    stats = shop(base, item, pipelines=pipelines)().run()
    assert stats["items_invalid"] == 1 and stats["items"] == 5
    [bad] = [i for i in kept if "_errors" in i]
    assert bad["_errors"] == ["price: required"]


def test_validate_works_on_its_own():
    check = netweir.validate(Book)
    assert check({"title": "t", "price": 1}) == {"title": "t", "price": 1}
    assert check({"title": "t"}) is None
    assert netweir.validate(Book).errors({"title": 5}) == [
        "title: expected str, got int",
        "price: required",
    ]


def test_without_jsonschema_a_dict_schema_says_what_to_install(monkeypatch):
    monkeypatch.setitem(sys.modules, "jsonschema", None)
    with pytest.raises(ImportError, match="pip install jsonschema"):
        netweir.validate(SCHEMA)


def test_validate_refuses_what_isn_t_a_schema():
    with pytest.raises(TypeError, match="JSON Schema"):
        netweir.validate("title")


# --- coverage --------------------------------------------------------------------


def test_coverage_counts_fields_that_aren_t_empty(base):
    def item(n):
        return {"title": f"t{n}", "price": None if n % 2 else n, "tags": [] if n else ["a"]}

    spider = shop(base, item)()
    spider.run()
    assert spider.coverage == {"title": 5, "price": 3, "tags": 1}


def test_coverage_of_dataclass_items(base):
    spider = shop(base, lambda n: Book(title=f"t{n}", price=n, stock=n or None))()
    spider.run()
    assert spider.coverage == {"title": 5, "price": 5, "stock": 4}


# --- checks ------------------------------------------------------------------------


def test_checks_that_pass(base, caplog):
    caplog.set_level("INFO")
    checks = [
        netweir.Checks(
            min_items=5,
            max_error_rate=0.0,
            coverage={"title": 1.0},
            finish_reasons=("finished",),
        )
    ]
    spider = shop(base, lambda n: {"title": str(n)}, checks=checks)()
    stats = spider.run()
    assert spider.check_failures == [] and stats["checks_failed"] == 0
    assert "checks passed" in caplog.text


def test_checks_that_fail_say_why(base, caplog):
    checks = [
        netweir.Checks(
            min_items=10,
            coverage={"price": 0.9, "title": 0.5},
            finish_reasons=("finished",),
        )
    ]

    def item(n):
        return {"title": "t", "price": 1} if n < 2 else {"title": "t"}

    Shop = shop(base, item, checks=checks)
    Shop.settings = netweir.Settings(**{**vars(FAST), "max_items": 4})
    spider = Shop()
    stats = spider.run()
    assert stats["checks_failed"] == 3
    joined = " | ".join(spider.check_failures)
    assert "4 items, fewer than 10" in joined
    assert "price in 50.0% of items, less than 90.0%" in joined
    assert "max_items" in joined
    assert caplog.text.count("check failed") == 3


def test_error_rate_counts_failed_fetches_and_callback_errors(base):
    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "retries": 0})
        start_urls = [base + "/1", base + "/2", base + "/3", "http://127.0.0.1:1/"]
        checks = [netweir.Checks(max_error_rate=0.25)]

        def parse(self, page):
            if page.url.endswith("/3"):
                raise RuntimeError("bad page")
            yield {"url": page.url}

    spider = Site()
    spider.run()
    # 1 failed fetch + 1 callback error, out of 3 fetched + 1 failed.
    assert spider.check_failures == ["error rate 50.0%, more than 25.0%"]


def test_max_invalid(base):
    pytest.importorskip("jsonschema")
    pipelines = [netweir.validate(SCHEMA)]
    checks = [netweir.Checks(max_invalid=0.1)]
    spider = shop(base, lambda n: {"title": "t"}, pipelines=pipelines, checks=checks)()
    spider.run()
    assert spider.check_failures == ["100.0% of items invalid, more than 10.0%"]


def test_a_function_is_a_check_too(base):
    def has_cheap(stats, spider):
        return None if stats["items"] > 0 else "nothing at all"

    def complains(stats, spider):
        return ["one", "two"]

    def breaks(stats, spider):
        raise ValueError("can't tell")

    spider = shop(base, lambda n: {"n": n}, checks=[has_cheap, complains, breaks])()
    stats = spider.run()
    assert spider.check_failures[:2] == ["one", "two"]
    assert "can't tell" in spider.check_failures[2] and stats["checks_failed"] == 3


def test_closed_sees_the_results(base):
    seen = []

    class Shop(netweir.Spider):
        settings = FAST
        start_urls = [base + "/1"]
        checks = [netweir.Checks(min_items=2)]

        def parse(self, page):
            yield {"n": 1}

        def closed(self, reason):
            seen.append(list(self.check_failures))

    Shop().run()
    assert seen == [["1 items, fewer than 2"]]


def test_netweir_crawl_exits_1_when_a_check_fails(base, tmp_path):
    from netweir import _cli

    spider = tmp_path / "site.py"
    spider.write_text(
        "import netweir\n"
        "class Site(netweir.Spider):\n"
        f"    start_urls = [{base + '/1'!r}]\n"
        "    checks = [netweir.Checks(min_items=5)]\n"
        "    def parse(self, page):\n"
        "        yield {'url': page.url}\n"
    )
    flags = ["-q", "-s", "throttle=false", "-s", "start_delay=0", "-s", "obey_robots=false"]
    assert _cli.main(["crawl", str(spider), *flags]) == 1


# --- review --------------------------------------------------------------------


def test_checks_that_return_true_false_or_odd_things(base):
    def predicate_ok(stats, spider):
        return stats["items"] > 0

    def predicate_bad(stats, spider):
        return False

    def odd(stats, spider):
        return 3

    def generator(stats, spider):
        yield "first"
        raise RuntimeError("then broke")

    def mixed(stats, spider):
        return [None, {"k": 1}]

    checks = [predicate_ok, predicate_bad, odd, generator, mixed]
    reasons = []
    Shop = shop(base, lambda n: {"n": n}, checks=checks, closed=lambda self, r: reasons.append(r))
    spider = Shop()
    spider.run()
    assert reasons == ["finished"]  # the crawl itself didn't fail
    assert spider.check_failures == [
        "predicate_bad returned False",
        "odd returned 3",
        "generator raised RuntimeError: then broke",
        "None",
        "{'k': 1}",
    ]


def test_validation_ignores_the_checkpoint_s_id(base, tmp_path):
    pytest.importorskip("jsonschema")
    strict = {**SCHEMA, "additionalProperties": False}
    Shop = shop(base, lambda n: {"title": "t", "price": n}, pipelines=[netweir.validate(strict)])
    Shop.settings = netweir.Settings(**{**vars(FAST), "checkpoint": str(tmp_path / "state")})
    stats = Shop().run()
    assert (stats["items"], stats["items_invalid"]) == (5, 0)


def test_an_invalid_item_is_heard_of_as_dropped(base):
    dropped = []

    class Listener:
        def item_dropped(self, item, exception):
            dropped.append((item["title"], exception))

    def item(n):
        return {"title": "bad"} if n == 1 else {"title": "ok", "price": 1.0}

    shop(base, item, pipelines=[netweir.validate(Book)], middlewares=[Listener()])().run()
    assert dropped == [("bad", None)]


def test_other_json_schema_drafts(base):
    pytest.importorskip("jsonschema")
    draft3 = {
        "$schema": "http://json-schema.org/draft-03/schema#",
        "type": "object",
        "properties": {"price": {"type": "number", "required": True}},
    }
    check = netweir.validate(draft3)
    assert check.errors({"title": "t"}) and check({"price": 1}) == {"price": 1}
    draft7 = {**SCHEMA, "$schema": "http://json-schema.org/draft-07/schema#"}
    assert netweir.validate(draft7).errors({"title": "t"}) == ["price: required"]


def test_drop_false_adds_to_errors_and_keeps_dataclasses(base):
    kept = []

    def item(n):
        if n == 1:
            return {"title": "x", "_errors": ["upstream"]}
        if n == 2:
            return Book(title="t", price="free")
        return {"title": "y", "price": 1.0}

    stats = shop(base, item, pipelines=[netweir.validate(Book, drop=False), keep(kept)])().run()
    assert stats["items_invalid"] == 2 and stats["items"] == 5
    [tagged] = [i for i in kept if isinstance(i, dict) and "_errors" in i]
    assert tagged["_errors"] == ["upstream", "price: required"]
    assert any(isinstance(i, Book) for i in kept)  # kept, with nowhere to put errors


def test_type_messages_look_inside_optional_containers():
    import typing

    @dataclasses.dataclass
    class Listing:
        sizes: list[int] | None = None
        names: typing.Optional[typing.List[str]] = None  # noqa: UP006, UP045

    check = netweir.validate(Listing)
    assert check.errors({"sizes": [1, "x"], "names": ["a", 2]}) == [
        "sizes.1: expected int, got str",
        "names.1: expected str, got int",
    ]


def test_an_annotation_that_can_t_be_resolved_isn_t_checked():
    namespace = {}
    exec(
        "from __future__ import annotations\n"
        "import dataclasses\n"
        "@dataclasses.dataclass\n"
        "class Odd:\n"
        "    title: str\n"
        "    thing: NotImportedHere = None\n",
        namespace,
    )
    check = netweir.validate(namespace["Odd"])
    assert check({"title": "t", "thing": object()}) is not None


def test_two_validate_stages_count_an_item_once(base):
    pytest.importorskip("jsonschema")
    pipelines = [netweir.validate(Book), netweir.validate(SCHEMA)]
    stats = shop(base, lambda n: {"title": "t"}, pipelines=pipelines)().run()
    assert (stats["items_validated"], stats["items_invalid"]) == (5, 5)


def test_array_errors_count_as_one_field(base):
    pytest.importorskip("jsonschema")
    schema = {
        "type": "object",
        "properties": {
            "offers": {"type": "array", "items": {"type": "object", "required": ["price"]}}
        },
    }
    item = {"offers": [{}, {}, {}]}
    spider = shop(base, lambda n: item, pipelines=[netweir.validate(schema)])()
    spider.run()
    assert spider.invalid_fields == {"offers.*.price": 15}


def test_finish_reasons_as_one_string(base):
    checks = [netweir.Checks(finish_reasons="max_items")]
    assert shop(base, lambda n: {"n": n}, checks=checks)().run()["checks_failed"] == 1
    assert netweir.Checks(finish_reasons="finished").finish_reasons == ("finished",)


def test_coverage_with_no_items_says_so(base):
    checks = [netweir.Checks(coverage={"title": 0.5})]
    spider = shop(base, lambda n: None, checks=checks)()
    spider.run()
    assert spider.check_failures == ["title: no items to find it in"]


def test_spider_closed_sees_the_results_and_the_cli_exits_0_when_they_pass(base, tmp_path):
    seen = []

    class Listener:
        def spider_closed(self, spider):
            seen.append(list(spider.check_failures))

    checks = [netweir.Checks(min_items=50)]
    shop(base, lambda n: {"n": n}, checks=checks, middlewares=[Listener()])().run()
    assert seen == [["5 items, fewer than 50"]]

    from netweir import _cli

    spider = tmp_path / "site.py"
    spider.write_text(
        "import netweir\n"
        "class Site(netweir.Spider):\n"
        f"    start_urls = [{base + '/1'!r}]\n"
        "    checks = [netweir.Checks(min_items=1)]\n"
        "    def parse(self, page):\n"
        "        yield {'url': page.url}\n"
    )
    flags = ["-q", "-s", "throttle=false", "-s", "start_delay=0", "-s", "obey_robots=false"]
    assert _cli.main(["crawl", str(spider), *flags]) == 0


def test_checks_don_t_run_when_the_crawl_raises(base):
    ran = []

    def check(stats, spider):
        ran.append(True)

    class Broken(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "fail_fast": True})
        start_urls = [base + "/1"]
        checks = [check]

        def parse(self, page):
            raise RuntimeError("boom")

    spider = Broken()
    with pytest.raises(RuntimeError):
        spider.run()
    assert ran == [] and spider.check_failures == []


# --- second review -------------------------------------------------------------


class Strange:
    """A value whose == raises, as a numpy array's truth or pandas' NA does."""

    def __eq__(self, other):
        raise TypeError("ambiguous")

    __hash__ = object.__hash__


def test_coverage_never_costs_an_item(base):
    spider = shop(base, lambda n: {"odd": Strange(), "n": n})()
    stats = spider.run()
    assert stats["items"] == 5 and stats["callback_errors"] == 0
    assert spider.coverage == {"odd": 5, "n": 5}


def test_one_check_without_a_list(base):
    spider = shop(base, lambda n: {"n": n}, checks=netweir.Checks(min_items=50))()
    stats = spider.run()
    assert stats["checks_failed"] == 1


def test_a_check_can_read_the_finished_output_file(base, tmp_path):
    out = tmp_path / "items.jsonl"

    def five_lines(stats, spider):
        lines = out.read_text().splitlines()
        return None if len(lines) == 5 else f"{len(lines)} lines"

    spider = shop(base, lambda n: {"n": n}, checks=[five_lines])()
    spider.run(output=str(out))
    assert spider.check_failures == []


def test_async_checks_are_awaited(base):
    async def later(stats, spider):
        return "async says no"

    spider = shop(base, lambda n: {"n": n}, checks=[later])()
    spider.run()
    assert spider.check_failures == ["async says no"]


def test_iterators_and_empty_messages(base):
    def iterator(stats, spider):
        return iter(["from an iterator"])

    def blank(stats, spider):
        return ""

    spider = shop(base, lambda n: {"n": n}, checks=[iterator, blank])()
    spider.run()
    assert spider.check_failures == ["from an iterator", "blank failed"]


def test_none_is_fine_where_the_type_allows_it():
    import typing

    @dataclasses.dataclass
    class Listing:
        stock: int | None
        note: typing.Any
        title: str

    check = netweir.validate(Listing)
    assert check.errors({"stock": None, "note": None, "title": "t"}) == []
    assert check.errors({"title": None}) == ["stock: required", "note: required", "title: required"]


def test_each_field_s_type_is_resolved_on_its_own(caplog):
    namespace = {}
    exec(
        "from __future__ import annotations\n"
        "import dataclasses\n"
        "@dataclasses.dataclass\n"
        "class Odd:\n"
        "    title: str\n"
        "    price: NotImportedHere\n"
        "    n: int = 0\n",
        namespace,
    )
    check = netweir.validate(namespace["Odd"])
    assert "price" in caplog.text  # said once, when made
    assert check.errors({"title": 5, "price": object(), "n": "x"}) == [
        "title: expected str, got int",
        "n: expected int, got str",
    ]
