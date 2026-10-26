"""Crawl templates: a whole shop, blog or jobs board from a URL, with the
strategies that decide how far the crawl goes."""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir
from netweir._cli import main

MENU = """<header><nav class="main"><a href="/">Home</a><a href="/c/lamps">Lamps</a>
<a href="/c/rugs">Rugs</a><a href="/login">Sign in</a></nav></header>"""


def card(slug):
    return (
        f'<li class="card"><a href="/p/{slug}"><h3>{slug.title()}</h3></a>'
        f'<span class="price">£{len(slug)}.99</span></li>'
    )


def grid(*slugs):
    return '<ul class="grid">' + "".join(card(s) for s in slugs) + "</ul>"


PRODUCT = """<script type="application/ld+json">{{"@context": "https://schema.org",
"@type": "Product", "name": "{name}", "offers": {{"@type": "Offer", "price": "9.99",
"priceCurrency": "GBP"}}}}</script><h1>{name}</h1>"""

ARTICLE = """<script type="application/ld+json">{{"@context": "https://schema.org",
"@type": "BlogPosting", "headline": "{name}", "datePublished": "2026-10-01",
"author": {{"@type": "Person", "name": "Ann"}}}}</script>
<article><h1>{name}</h1><p>{text}</p></article>"""

JOB = """<script type="application/ld+json">{{"@context": "https://schema.org",
"@type": "JobPosting", "title": "{name}", "datePosted": "2026-10-01",
"hiringOrganization": {{"@type": "Organization", "name": "Lumo"}},
"jobLocation": {{"@type": "Place", "address": "Leeds"}}}}</script><h1>{name}</h1>"""

PAGES = {
    "/": MENU
    + '<main><h1>Welcome</h1><p>Light for every room. <a href="/about">About us</a></p></main>',
    "/about": MENU + '<h1>About</h1><p>We sell lamps, and one <a href="/p/hidden">secret</a>.</p>',
    "/c/lamps": MENU
    + '<h1>Lamps</h1><ul class="subcategories"><li><a href="/c/lamps/desk">Desk lamps</a></li></ul>'
    + grid("arc", "dome", "clip")
    + '<a rel="next" href="/c/lamps?page=2">Next</a>',
    "/c/lamps?page=2": MENU + "<h1>Lamps</h1>" + grid("orb", "cone", "drum"),
    "/c/lamps/desk": MENU + "<h1>Desk lamps</h1>" + grid("desk-one", "desk-two", "desk-three"),
    "/c/rugs": MENU + "<h1>Rugs</h1>" + grid("jute", "wool", "silk"),
    "/blog": "<h1>Journal</h1>"
    + "".join(
        f'<article><h2><a href="/blog/{s}">{s.title()}</a></h2>'
        f'<time datetime="2026-10-0{i}">{i} Oct</time></article>'
        for i, s in enumerate(["winter", "rooms", "reading"], 1)
    ),
    "/jobs": '<h1>Jobs</h1><ul class="jobs">'
    + "".join(
        f'<li class="job"><h3><a href="/jobs/{s}">{s.title()}</a></h3><span>Leeds</span>'
        f'<span class="salary">£25,000 a year</span><time datetime="2026-10-0{i}">{i} Oct</time></li>'
        for i, s in enumerate(["baker", "driver", "gardener"], 1)
    )
    + "</ul>",
}


def body(path):
    if path in PAGES:
        return PAGES[path]
    if path.startswith("/p/"):
        return PRODUCT.format(name=path[3:])
    if path.startswith("/blog/"):
        return ARTICLE.format(name=path[6:].title(), text="Words. " * 60)
    if path.startswith("/jobs/"):
        return JOB.format(name=path[6:].title())
    return None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        found = body(self.path)
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


QUIET = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)


def run(template, url, strategy=None, **attrs):
    found = []

    class Crawl(template):
        start_urls = [url]
        pipelines = [lambda i: found.append(i) or i]
        settings = QUIET

    for name, value in attrs.items():
        setattr(Crawl, name, value)
    if strategy is not None:
        Crawl.strategy = strategy
    Crawl().run()
    return found


def names(items, field="name"):
    return sorted(i[field] for i in items)


LAMPS = ["arc", "clip", "cone", "desk-one", "desk-three", "desk-two", "dome", "drum", "orb"]
RUGS = ["jute", "silk", "wool"]


def test_navigation_follows_sub_categories_and_pages(base):
    items = run(netweir.templates.Ecommerce, f"{base}/c/lamps", "navigation")
    assert names(items) == LAMPS
    assert all(i["price"] == "9.99" for i in items)


def test_pagination_only_stays_on_the_start_listing(base):
    items = run(netweir.templates.Ecommerce, f"{base}/c/lamps", "pagination_only")
    assert names(items) == ["arc", "clip", "cone", "dome", "drum", "orb"]


def test_direct_item_reads_only_the_start_urls(base):
    items = run(netweir.templates.Ecommerce, f"{base}/p/arc", "direct_item")
    assert names(items) == ["arc"]


def test_navigation_from_a_home_page_takes_its_menu(base):
    items = run(netweir.templates.Ecommerce, f"{base}/", "navigation")
    assert names(items) == sorted(LAMPS + RUGS)


def test_full_finds_products_off_the_listings_once_each(base):
    items = run(netweir.templates.Ecommerce, f"{base}/", "full")
    assert names(items) == sorted([*LAMPS, *RUGS, "hidden"])


def test_automatic_is_full_from_a_home_page_and_navigation_elsewhere(base):
    assert "hidden" in names(run(netweir.templates.Ecommerce, f"{base}/"))
    assert names(run(netweir.templates.Ecommerce, f"{base}/c/lamps")) == LAMPS


def test_a_subclass_can_pin_a_field(base):
    class Named(netweir.auto.Product):
        name = netweir.css("h1")

    items = run(netweir.templates.Ecommerce, f"{base}/c/rugs", "navigation", item=Named)
    assert names(items) == RUGS
    assert items[0]["metadata"]["sources"]["name"] == "css:h1"


def test_an_unknown_strategy_is_refused():
    class Crawl(netweir.templates.Ecommerce):
        start_urls = ["https://shop.example/"]
        strategy = "everything"

    with pytest.raises(ValueError, match="strategy"):
        Crawl().run()


def test_articles_and_jobs(base):
    articles = run(netweir.templates.Article, f"{base}/blog", "navigation")
    assert names(articles, "headline") == ["Reading", "Rooms", "Winter"]
    jobs = run(netweir.templates.Job, f"{base}/jobs", "navigation")
    assert names(jobs, "job_title") == ["Baker", "Driver", "Gardener"]


def test_crawl_command_runs_a_template(base, tmp_path):
    out = tmp_path / "out.jsonl"
    code = main(
        [
            "crawl",
            "ecommerce",
            "--url",
            f"{base}/c/lamps",
            "--strategy",
            "pagination_only",
            "-o",
            str(out),
            "-s",
            "obey_robots=false",
            "-s",
            "throttle=false",
            "-q",
        ]
    )
    assert code == 0
    rows = [json.loads(line) for line in out.read_text().splitlines()]
    assert sorted(r["name"] for r in rows) == ["arc", "clip", "cone", "dome", "drum", "orb"]


def test_crawl_command_needs_a_url_for_a_template(capsys):
    assert main(["crawl", "ecommerce"]) == 2
    assert "--url" in capsys.readouterr().err


def test_navigation_rules_are_checked_when_written():
    with pytest.raises(TypeError, match="navigation type"):
        netweir.Follow(navigation=netweir.auto.Product, part="items")
    with pytest.raises(ValueError, match="part="):
        netweir.Follow(navigation=netweir.auto.ProductNavigation, part="products")
    with pytest.raises(TypeError, match="one of them"):
        netweir.Follow("a", navigation=netweir.auto.ProductNavigation, part="items")
    with pytest.raises(TypeError, match="Item subclass"):
        netweir.Extract(dict)


def test_parse_shows_a_navigation_rules_links(base):
    from netweir._project import _rule_hits

    page = netweir.get(f"{base}/c/lamps")
    rule = netweir.Follow(navigation=netweir.auto.ProductNavigation, part="sub_categories")
    assert _rule_hits(rule, page) == [f"{base}/c/lamps/desk"]
    assert _rule_hits(netweir.Extract(netweir.auto.Product), page) == []
