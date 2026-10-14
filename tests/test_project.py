"""startproject, genspider, list, check, parse, version and crawl by name:
Scrapy's project commands, for Netweir spiders."""

import ast
import json
import subprocess
import sys
import textwrap
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import netweir
from netweir import _cli

FAST = ["-s", "throttle=false", "-s", "start_delay=0"]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send(self, status, body, ctype="text/html; charset=utf-8"):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        p = self.path
        if p == "/robots.txt":
            self.send(200, "User-agent: *\nDisallow: /private\n", "text/plain")
        elif p == "/":
            self.send(
                200,
                "<title>Home</title><a href='/category/a'>a</a>"
                "<a href='/item/1'>1</a><a href='/item/2'>2</a>",
            )
        elif p == "/category/a":
            self.send(200, "<title>A</title><a href='/item/3'>3</a>")
        elif p.startswith("/item/"):
            n = p.rsplit("/", 1)[1]
            self.send(200, f"<title>Item {n}</title><h1>Item {n}</h1><a href='/'>home</a>")
        elif p == "/echo":
            self.send(200, json.dumps(dict(self.headers.items())), "application/json")
        else:
            self.send(404, "<h1>missing</h1>")


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


def netweir_cmd(*args, cwd):
    return subprocess.run(
        [sys.executable, "-m", "netweir", *args],
        capture_output=True,
        text=True,
        timeout=120,
        cwd=cwd,
    )


def ruff(path, root):
    """ruff's defaults with this repository's rules, run from the project's
    root as its owner would run it."""
    for check in (["check", "--select", "E,F,I,B,UP,SIM"], ["format", "--check"]):
        done = subprocess.run(
            [sys.executable, "-m", "ruff", *check, "--isolated", str(path)],
            capture_output=True,
            text=True,
            cwd=root,
        )
        assert done.returncode == 0, done.stdout + done.stderr


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(text), encoding="utf-8")
    return path


# --- startproject and genspider ----------------------------------------------


def test_startproject_makes_a_project_and_never_overwrites(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    assert _cli.main(["startproject", "books"]) == 0
    root = tmp_path / "books"
    assert (root / "spiders" / "settings.py").is_file()
    readme = (root / "README.md").read_text(encoding="utf-8")
    assert "netweir crawl" in readme and "netweir serve --project books=spiders/" in readme
    out = capsys.readouterr().out
    assert "netweir genspider" in out
    ruff(root / "spiders" / "settings.py", root)

    # Again: it's there, so nothing is written.
    (root / "README.md").write_text("mine", encoding="utf-8")
    assert _cli.main(["startproject", "books"]) == 2
    assert "README.md" in capsys.readouterr().err
    assert (root / "README.md").read_text(encoding="utf-8") == "mine"

    # Into a directory of one's choosing, and a name serve couldn't take.
    assert _cli.main(["startproject", "shop", "here"]) == 0
    assert (tmp_path / "here" / "spiders" / "settings.py").is_file()
    for bad in ("no good", "..", ".", "my-shop", "9lives"):
        assert _cli.main(["startproject", bad]) == 2


def test_startproject_and_genspider_never_write_through_a_symlink(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    (tmp_path / "shop").mkdir()
    target = tmp_path / "elsewhere.md"
    (tmp_path / "shop" / "README.md").symlink_to(target)
    assert _cli.main(["startproject", "shop"]) == 2
    assert not target.exists() and not (tmp_path / "shop" / "spiders").exists()

    (tmp_path / "books.py").symlink_to(tmp_path / "elsewhere.py")
    assert _cli.main(["genspider", "books", "example.com"]) == 2
    assert not (tmp_path / "elsewhere.py").exists()


def test_genspider_refuses_names_of_modules(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    for name in ("asyncio", "json", "netweir", "settings", "pytest"):
        assert _cli.main(["genspider", name, "example.com"]) == 2, name
        assert not (tmp_path / f"{name}.py").exists()
    assert "module" in capsys.readouterr().err


@pytest.mark.parametrize("template", ["basic", "crawl", "sitemap"])
def test_genspider_templates_pass_ruff_and_load(template, tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    assert _cli.main(["startproject", "shop"]) == 0
    monkeypatch.chdir(tmp_path / "shop")
    assert _cli.main(["genspider", "-t", template, "books", "books.example.com"]) == 0
    path = tmp_path / "shop" / "spiders" / "books.py"
    assert path.is_file()
    assert str(Path("spiders") / "books.py") in capsys.readouterr().out
    ruff(path, tmp_path / "shop")
    text = path.read_text(encoding="utf-8")
    assert "from settings import SETTINGS" in text
    assert '"books.example.com"' in text
    urls = {
        target.id: ast.literal_eval(node.value)
        for node in ast.walk(ast.parse(text))
        if isinstance(node, ast.Assign)
        for target in node.targets
        if isinstance(target, ast.Name) and target.id in ("start_urls", "sitemap_urls")
    }
    if template == "sitemap":
        assert urls == {"sitemap_urls": ["https://books.example.com/robots.txt"]}
    else:
        assert urls == {"start_urls": ["https://books.example.com/"]}


def test_genspider_outside_a_project_takes_a_url(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    assert _cli.main(["genspider", "quotes", "http://127.0.0.1:8000/start"]) == 0
    text = (tmp_path / "quotes.py").read_text(encoding="utf-8")
    assert "settings" not in text.lower().replace("netweir.settings", "")
    assert '"127.0.0.1:8000"' in text
    assert '"http://127.0.0.1:8000/start"' in text
    ruff(tmp_path / "quotes.py", tmp_path)
    (cls,) = _cli._spiders_in(str(tmp_path / "quotes.py"))
    assert cls.name == "quotes" and cls.__name__ == "QuotesSpider"

    # The file is there: refused, unless --force.
    assert _cli.main(["genspider", "quotes", "example.com"]) == 2
    assert "quotes.py" in capsys.readouterr().err
    assert _cli.main(["genspider", "--force", "quotes", "example.com"]) == 0
    assert '"example.com"' in (tmp_path / "quotes.py").read_text(encoding="utf-8")

    assert _cli.main(["genspider", "-t", "xmlfeed", "x", "example.com"]) == 2
    assert _cli.main(["genspider", "bad-name!", "example.com"]) == 2
    assert _cli.main(["genspider", "x", "not a domain"]) == 2
    capsys.readouterr()
    assert _cli.main(["genspider", "-l"]) == 0
    listed = capsys.readouterr().out.splitlines()
    assert [line.split()[0] for line in listed] == ["basic", "crawl", "sitemap"]
    assert all(len(line.split()) > 2 for line in listed)


def test_genspider_refuses_a_name_the_project_has(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    write(
        tmp_path / "spiders" / "shop.py",
        """
        import netweir

        class Shop(netweir.Spider):
            name = "books"
        """,
    )
    assert _cli.main(["genspider", "books", "example.com"]) == 2
    assert "shop.py" in capsys.readouterr().err
    assert not (tmp_path / "spiders" / "books.py").exists()


def test_a_generated_project_crawls_checks_and_lists(base, tmp_path):
    """From nothing to a crawl, in a fresh process each time, as a user
    would type it: the spider imports the project's settings."""
    done = netweir_cmd("startproject", "site", cwd=tmp_path)
    assert done.returncode == 0, done.stderr
    project = tmp_path / "site"
    for template, name in [("basic", "pages"), ("crawl", "items")]:
        done = netweir_cmd("genspider", "-t", template, name, base + "/", cwd=project)
        assert done.returncode == 0, done.stderr
    listed = netweir_cmd("list", cwd=project / "spiders")
    assert listed.returncode == 0, listed.stderr
    assert listed.stdout.split() == ["items", "pages"]

    out = tmp_path / "pages.jsonl"
    done = netweir_cmd("crawl", "pages", "-o", str(out), *FAST, cwd=project)
    assert done.returncode == 0, done.stderr
    assert json.loads(out.read_text(encoding="utf-8")) == {"title": "Home", "url": base + "/"}

    # From the project's root, by path, as serve runs it and as runspider.
    out = tmp_path / "items.jsonl"
    done = netweir_cmd("runspider", "spiders/items.py", "-o", str(out), *FAST, cwd=project)
    assert done.returncode == 0, done.stderr
    titles = sorted(json.loads(line)["title"] for line in out.read_text().splitlines())
    assert titles == ["Item 1", "Item 2", "Item 3"]

    checked = netweir_cmd("check", *FAST, cwd=project)
    assert checked.returncode == 0, checked.stderr
    # basic's parse, and crawl's check_entry: @returns and @scrapes each.
    assert "Ran 4 contracts" in checked.stderr and "OK" in checked.stderr


# --- list and crawl by name --------------------------------------------------


def test_list_and_its_mistakes(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    assert _cli.main(["list"]) == 2
    assert "spiders" in capsys.readouterr().err

    write(
        tmp_path / "spiders" / "a.py",
        """
        import netweir

        class Zebra(netweir.Spider):
            pass

        class Apple(netweir.Spider):
            name = "apple"
        """,
    )
    write(tmp_path / "spiders" / "helpers" / "ignored.py", "raise SystemExit('not read')\n")
    (tmp_path / "deep").mkdir()
    monkeypatch.chdir(tmp_path / "deep")
    assert _cli.main(["list"]) == 0
    assert capsys.readouterr().out.split() == ["Zebra", "apple"]

    write(
        tmp_path / "spiders" / "b.py",
        "import netweir\nclass Twin(netweir.Spider):\n    name = 'apple'\n",
    )
    assert _cli.main(["list"]) == 0
    captured = capsys.readouterr()
    assert captured.out.split() == ["Zebra", "apple"]
    assert "a.py" in captured.err and "b.py" in captured.err

    write(tmp_path / "spiders" / "broken.py", "import nowhere_to_be_found\n")
    assert _cli.main(["list"]) == 1
    assert "broken.py" in capsys.readouterr().err


def test_crawl_by_name_needs_one_spider_of_that_name(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    assert _cli.main(["crawl", "nothing"]) == 2
    assert "nothing" in capsys.readouterr().err
    write(tmp_path / "spiders" / "a.py", "import netweir\nclass A(netweir.Spider):\n    pass\n")
    assert _cli.main(["crawl", "missing"]) == 2
    assert "A" in capsys.readouterr().err


# --- check -------------------------------------------------------------------

CONTRACTED = """
import json

import netweir
from netweir.contracts import Contract, ContractFail


class HasHeader(Contract):
    \"\"\"@has_header NAME: the request is sent with that header.\"\"\"

    name = "has_header"

    def adjust_request_args(self, args):
        args["headers"] = {{self.args[0]: "yes"}}
        return args

    def pre_process(self, page):
        if json.loads(page.text).get(self.args[0]) != "yes":
            raise ContractFail(f"{{self.args[0]}} wasn't sent")


class NotEmpty(Contract):
    name = "not_empty"

    def post_process(self, output):
        if not output:
            raise ContractFail("gave nothing")


class Shop(netweir.Spider):
    name = "shop"
    contracts = [HasHeader, NotEmpty]

    def parse(self, page):
        \"\"\"The home page: three links and a title.

        @url {base}/
        @returns items 1 1
        @returns requests 3
        @scrapes title
        \"\"\"
        yield {{"title": page.css("title::text").get()}}
        for href in page.css("a::attr(href)").getall():
            yield page.follow(href)

    def item(self, page, number=None):
        \"\"\"@url {base}/item/7
        @cb_kwargs {{"number": "7"}}
        @meta {{"kind": "item"}}
        @returns items 1
        @scrapes title number kind
        \"\"\"
        yield {{"title": page.css("h1::text").get(), "number": number, "kind": page.meta["kind"]}}

    def echo(self, page):
        \"\"\"@url {base}/echo
        @has_header X-Check
        @not_empty
        \"\"\"
        yield json.loads(page.text)

    def unchecked(self, page):
        \"\"\"No contracts here.\"\"\"
"""

FAILING = """
import netweir


class Broken(netweir.Spider):
    name = "broken"

    def parse(self, page):
        \"\"\"@url {base}/
        @returns items 2
        @scrapes title price
        \"\"\"
        yield {{"title": "x"}}

    def raises(self, page):
        \"\"\"@url {base}/
        @returns items 0 0
        \"\"\"
        raise RuntimeError("callback went wrong")

    def no_url(self, page):
        \"\"\"@returns items 1\"\"\"

    def unknown(self, page):
        \"\"\"@url {base}/
        @nonsense 1
        \"\"\"

    def kept_out(self, page):
        \"\"\"@url {base}/private/x
        @returns items 0
        \"\"\"
"""


def test_check_runs_contracts_custom_ones_too(base, tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    write(tmp_path / "spiders" / "shop.py", CONTRACTED.format(base=base))
    assert _cli.main(["check", *FAST]) == 0, capsys.readouterr().err
    err = capsys.readouterr().err
    # parse: 2 returns, scrapes; item: returns, scrapes; echo: pre and post.
    assert "Ran 7 contracts" in err
    assert err.rstrip().endswith("OK")

    assert _cli.main(["check", "-v", "shop", *FAST]) == 0
    err = capsys.readouterr().err
    assert "[shop] echo (@has_header pre-hook) ... ok" in err
    assert "[shop] parse (@returns post-hook) ... ok" in err

    assert _cli.main(["check", "-l"]) == 0
    assert capsys.readouterr().out.split() == ["shop", "*", "echo", "*", "item", "*", "parse"]


def test_check_reports_failures_and_errors(base, tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    write(tmp_path / "spiders" / "broken.py", FAILING.format(base=base))
    assert _cli.main(["check", "broken", *FAST]) == 1
    err = capsys.readouterr().err
    assert "FAIL: [broken] parse (@returns post-hook)" in err
    assert "Returned 1 items, expected 2..inf" in err
    assert "Missing fields: price" in err
    assert "ERROR: [broken] raises (callback)" in err and "callback went wrong" in err
    assert "ERROR: [broken] no_url (contract)" in err and "@url" in err
    assert "ERROR: [broken] unknown (contract)" in err and "nonsense" in err
    assert "ERROR: [broken] kept_out (callback)" in err and "never reached" in err
    assert "FAILED (failures=2, errors=4)" in err

    assert _cli.main(["check", "nobody"]) == 2


def test_check_takes_a_file(base, tmp_path, capsys):
    path = write(tmp_path / "shop.py", CONTRACTED.format(base=base))
    assert _cli.main(["check", str(path), *FAST]) == 0
    assert "Ran 7 contracts" in capsys.readouterr().err


def test_check_reads_from_the_cache(tmp_path, capsys):
    """With the cache on, a second check runs with the site gone."""
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    site = f"http://127.0.0.1:{server.server_port}"
    spider = write(
        tmp_path / "one.py",
        f"""
        import netweir

        class One(netweir.Spider):
            def parse(self, page):
                \"\"\"@url {site}/item/1
                @returns items 1
                \"\"\"
                yield {{"h": page.css("h1::text").get()}}
        """,
    )
    argv = ["check", str(spider), *FAST, "-s", f"cache={tmp_path / 'cache'}", "-s", "retries=0"]
    assert _cli.main(argv) == 0
    server.shutdown()
    server.server_close()
    assert _cli.main(argv) == 0
    assert "Ran 1 contract " in capsys.readouterr().err


# --- parse -------------------------------------------------------------------

PARSED = """
import netweir


class Entry(netweir.Item):
    heading = netweir.css("h1::text")


class Shop(netweir.Spider):
    name = "shop"
    allowed_domains = ["{host}"]
    start_urls = ["{base}/"]

    def parse(self, page):
        yield {{"title": page.css("title::text").get(), "meta": page.meta.get("m")}}
        for href in page.css("a::attr(href)").getall():
            yield page.follow(href, callback="item")

    def item(self, page, extra="none"):
        yield {{"item": page.css("h1::text").get(), "extra": extra}}


class Fenced(netweir.Spider):
    name = "fenced"
    allowed_domains = ["elsewhere.example"]
    rules = [netweir.Follow("a")]


class Ruled(netweir.Spider):
    name = "ruled"
    rules = [
        netweir.Follow("a", allow=[r"/category/"]),
        netweir.Follow("a", allow=[r"/item/"], extract=Entry),
    ]
"""


@pytest.fixture
def shop(base, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    host = base.removeprefix("http://")
    return write(tmp_path / "spiders" / "shop.py", PARSED.format(base=base, host=host))


def test_parse_finds_the_spider_by_domain(base, shop, capsys):
    assert _cli.main(["parse", base + "/", *FAST, "-m", '{"m": 1}']) == 0
    out = capsys.readouterr().out
    assert ">>> STATUS DEPTH LEVEL 1 <<<" in out
    assert "# Scraped Items" in out and "{'meta': 1, 'title': 'Home'}" in out
    assert f"<GET {base}/item/1>" in out and f"<GET {base}/category/a>" in out


def test_parse_with_a_callback_and_depth(base, shop, capsys):
    argv = ["parse", base + "/", "--spider", "shop", "-d", "2", *FAST]
    assert _cli.main(argv) == 0
    out = capsys.readouterr().out
    assert ">>> STATUS DEPTH LEVEL 2 <<<" in out
    # Every level's items; the last level's requests (none: item gives none).
    assert "'title': 'Home'" in out and "{'extra': 'none', 'item': 'Item 1'}" in out
    assert "<GET" not in out

    assert _cli.main([*argv, "-v"]) == 0
    out = capsys.readouterr().out
    assert ">>> DEPTH LEVEL: 1 <<<" in out and ">>> DEPTH LEVEL: 2 <<<" in out
    assert f"<GET {base}/item/2>" in out

    argv = ["parse", base + "/item/9", "--spider", "shop", "-c", "item", *FAST]
    assert _cli.main([*argv, "--cbkwargs", '{"extra": "given"}', "--nolinks"]) == 0
    out = capsys.readouterr().out
    assert "{'extra': 'given', 'item': 'Item 9'}" in out and "# Requests" not in out


def test_parse_with_rules(base, shop, capsys):
    argv = ["parse", base + "/", "--spider", "ruled", *FAST]
    # A start page: the rules give its links.
    assert _cli.main(argv) == 0
    out = capsys.readouterr().out
    assert f"<GET {base}/category/a>" in out and f"<GET {base}/item/1>" in out
    # --rules: the rule that admits the URL extracts its item.
    assert _cli.main(["parse", base + "/item/4", "--spider", "ruled", "--rules", *FAST]) == 0
    assert "{'heading': 'Item 4'}" in capsys.readouterr().out
    # Two levels: the listing, then the items its rules lead to.
    assert _cli.main([*argv, "-d", "3"]) == 0
    out = capsys.readouterr().out
    assert "{'heading': 'Item 3'}" in out and "{'heading': 'Item 1'}" in out


def test_parse_rules_keep_to_allowed_domains_and_fall_back_to_parse(base, shop, capsys):
    # The URL itself is fetched, as asked; the links off-site are dropped.
    assert _cli.main(["parse", base + "/", "--spider", "fenced", *FAST]) == 0
    out = capsys.readouterr().out
    assert "<GET" not in out
    # A rule that only follows links: parse, which Ruled doesn't write.
    argv = ["parse", base + "/category/a", "--spider", "ruled", "--rules", *FAST]
    assert _cli.main(argv) == 1
    assert "parse" in capsys.readouterr().err


def test_parse_pipelines_and_mistakes(base, shop, tmp_path, capsys):
    seen = tmp_path / "seen.txt"
    path = write(
        tmp_path / "piped.py",
        f"""
        import netweir

        def keep(item):
            with open({str(seen)!r}, "a") as f:
                f.write(item["title"] + "\\n")
            return item

        class Piped(netweir.Spider):
            pipelines = [keep]

            def parse(self, page):
                yield {{"title": page.css("title::text").get()}}
        """,
    )
    assert _cli.main(["parse", base + "/", "--spider", str(path), *FAST]) == 0
    assert not seen.exists()
    assert _cli.main(["parse", base + "/", "--spider", str(path), "--pipelines", *FAST]) == 0
    assert seen.read_text() == "Home\n"
    capsys.readouterr()

    _cli._forget_loaded()  # the file above is from another folder
    # No spider covers the host; a callback that isn't there; bad JSON.
    assert _cli.main(["parse", "http://elsewhere.test/", *FAST]) == 2
    assert _cli.main(["parse", base + "/", "--spider", "shop", "-c", "nope"]) == 2
    assert _cli.main(["parse", base + "/", "--spider", "shop", "-m", "{"]) == 2
    # --rules on a spider without rules, or a URL no rule admits.
    assert _cli.main(["parse", base + "/", "--spider", "shop", "--rules"]) == 2
    assert _cli.main(["parse", base + "/nowhere", "--spider", "ruled", "--rules"]) == 2
    capsys.readouterr()
    # A page that can't be fetched, and a callback that raises.
    dead = "http://127.0.0.1:9/"
    assert _cli.main(["parse", dead, "--spider", "shop", *FAST, "-s", "retries=0"]) == 1
    assert "127.0.0.1:9" in capsys.readouterr().err
    assert _cli.main(["parse", base + "/", "--spider", "ruled", "-c", "parse", *FAST]) == 1


# --- version -----------------------------------------------------------------


def test_returns_counts_only_items():
    from netweir.contracts import ContractFail, ReturnsContract

    contract = ReturnsContract(None, "items", "0", "0")
    contract.post_process([None, netweir.Request("https://e.com/")])
    with pytest.raises(ContractFail):
        contract.post_process([{"a": 1}])


def test_spiders_made_with_arguments_and_their_own_settings(base, tmp_path, capsys):
    path = write(
        tmp_path / "own.py",
        f"""
        import netweir

        class Own(netweir.Spider):
            def __init__(self):
                # robots.txt keeps /private out, unless the spider says.
                self.settings = netweir.Settings(obey_robots=False)

            def parse(self, page):
                \"\"\"@url {base}/private/x
                @returns items 1
                \"\"\"
                yield {{"status": page.status}}

        class Needy(netweir.Spider):
            def __init__(self, category):
                self.category = category
        """,
    )
    assert _cli.main(["check", str(path), *FAST]) == 2
    assert "Needy" in capsys.readouterr().err
    path.write_text(path.read_text().split("class Needy")[0])
    assert _cli.main(["check", str(path), *FAST]) == 0, capsys.readouterr().err


def _clashing_project(root, name, depth, base):
    """A project whose settings.py, helpers.py and consts.py have the same
    names as every other's, and a spider that imports a helper late."""
    spiders = root / name / "spiders"
    write(spiders / "consts.py", f"DEPTH = {depth}\n")
    write(
        spiders / "settings.py",
        "import netweir\nfrom consts import DEPTH\nSETTINGS = netweir.Settings(max_depth=DEPTH)\n",
    )
    write(spiders / "helpers.py", f"WHO = {name!r}\n")
    write(
        spiders / f"s{name}.py",
        f"""
        import netweir
        from settings import SETTINGS

        class S{name}(netweir.Spider):
            settings = SETTINGS
            start_urls = ["{base}/item/1"]

            def parse(self, page):
                \"\"\"@url {base}/item/1
                @returns items 1
                @scrapes who depth
                \"\"\"
                import helpers  # at crawl time, not load time

                yield {{"who": helpers.WHO, "depth": self.settings.max_depth}}

            def check_who(self, page):
                \"\"\"@url {base}/item/2
                @is_{name}
                \"\"\"
                return self.parse(page)
        """,
    )
    # A contract that sees whose helpers and settings each project got.
    with open(spiders / f"s{name}.py", "a", encoding="utf-8") as f:
        f.write(
            textwrap.dedent(
                f"""
                from netweir.contracts import Contract, ContractFail

                class Is{name.upper()}(Contract):
                    name = "is_{name}"

                    def post_process(self, output):
                        want = {{"who": {name!r}, "depth": {depth}}}
                        if output != [want]:
                            raise ContractFail(f"{{output}} isn't {{want}}")

                S{name}.contracts = [Is{name.upper()}]
                """
            )
        )
    return spiders / f"s{name}.py"


def test_each_project_gets_its_own_modules(base, tmp_path):
    """Two projects whose settings.py, helpers.py and consts.py share names:
    checked together, each folder in a process of its own."""
    a = _clashing_project(tmp_path, "a", 1, base)
    b = _clashing_project(tmp_path, "b", 2, base)
    write(tmp_path / "settings.py", "raise SystemExit('the wrong settings.py')\n")
    done = netweir_cmd("check", str(a), str(b), *FAST, cwd=tmp_path)
    assert done.returncode == 0, done.stderr
    assert "Ran 6 contracts" in done.stderr and done.stderr.rstrip().endswith("OK")

    # The worst of the two decides: break b's helper.
    (b.parent / "helpers.py").write_text("WHO = 'not b'\n")
    done = netweir_cmd("check", str(a), str(b), *FAST, cwd=tmp_path)
    assert done.returncode == 1
    assert "FAIL: [Sb] check_who (@is_b post-hook)" in done.stderr
    assert "FAILED (failures=1)" in done.stderr

    # -l from both.
    done = netweir_cmd("check", "-l", str(a), str(b), cwd=tmp_path)
    assert done.stdout.split() == ["Sa", "*", "check_who", "*", "parse"] + [
        "Sb", "*", "check_who", "*", "parse"
    ]  # fmt: skip


@pytest.mark.parametrize("workers", [1, 2])
def test_a_helper_imported_in_a_callback(base, tmp_path, workers):
    a = _clashing_project(tmp_path, "a", 1, base)
    out = tmp_path / "out.jsonl"
    argv = ["crawl", str(a), "-o", str(out), *FAST, "-s", f"workers={workers}"]
    done = netweir_cmd(*argv, cwd=tmp_path)
    assert done.returncode == 0, done.stderr
    assert json.loads(out.read_text(encoding="utf-8")) == {"who": "a", "depth": 1}


def test_a_second_folder_in_one_process_is_refused(base, tmp_path):
    a = _clashing_project(tmp_path, "a", 1, base)
    b = _clashing_project(tmp_path, "b", 2, base)
    (cls,) = _cli._spiders_in(str(a))
    assert cls.settings.max_depth == 1
    with pytest.raises(_cli._Usage, match="one folder of spiders per process"):
        _cli._spiders_in(str(b))


def test_version(capsys):
    assert _cli.main(["version"]) == 0
    assert capsys.readouterr().out == f"Netweir {netweir.__version__}\n"
    assert _cli.main(["version", "-v"]) == 0
    out = capsys.readouterr().out
    for name in ("Netweir", "Python", "Platform", "Rust", "lexbor", "Profiles"):
        assert name in out
    assert "chrome-" in out and "rustc " in out
