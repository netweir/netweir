"""The LLM repair plugin: proposals for broken tracked selectors, checked
and reported, never applied."""

import json
import logging
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir
import netweir._track

BEFORE = '<html><body><h1>Blue Kettle</h1><p class="price_color">£24.99</p></body></html>'
# A redesign too big for similarity: new tag, class, wording and place.
AFTER = (
    "<html><body><header><nav>Shop</nav></header><main><section>"
    "<h2>Blue Kettle</h2><div class='amount'>EUR 30</div></section></main></body></html>"
)
SITE = {"html": BEFORE}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        body = SITE["html"] if self.path.startswith("/product") else '<a href="/product/1">p</a>'
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture
def base(tmp_path, monkeypatch):
    monkeypatch.setenv("NETWEIR_HOME", str(tmp_path / "home"))
    netweir._track._reset()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()
    netweir._track._reset()


class Product(netweir.Item):
    price = netweir.css("p.price_color::text", track="price")


def crawl(base, repair):
    items = []

    class Shop(netweir.Spider):
        start_urls = [f"{base}/"]
        settings = netweir.Settings(
            throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False
        )
        rules = [netweir.Follow("a", extract=Product)]
        pipelines = [lambda item: items.append(item) or item]

    Shop.repair = repair
    spider = Shop()
    stats = spider.run()
    return spider, items, stats


def test_a_broken_selector_gets_a_proposal_that_is_checked_but_not_applied(base, tmp_path):
    prompts = []

    def fake_llm(prompt):
        prompts.append(prompt)
        return 'Sure. {"kind": "css", "selector": "div.amount::text", "reason": "price moved"}'

    report = tmp_path / "repairs.jsonl"
    repair = netweir.repair.llm(complete=fake_llm, report=str(report))
    SITE["html"] = BEFORE
    crawl(base, repair)
    assert prompts == [], "nothing to repair while the selector works"

    SITE["html"] = AFTER
    spider, items, stats = crawl(base, repair)
    assert items == [{"price": None}], "a proposal is never applied"
    assert stats["lost"] == 1 and stats["repair_proposals"] == 1
    assert len(prompts) == 1
    assert "p.price_color::text" in prompts[0] and "amount" in prompts[0]
    (proposal,) = spider.repairs
    assert proposal.name == "price" and proposal.selector == "div.amount::text"
    assert proposal.matches == "EUR 30"
    assert proposal.old_query == "p.price_color::text"
    assert 0 <= proposal.score <= 1
    saved = [json.loads(line) for line in report.read_text(encoding="utf-8").splitlines()]
    assert saved[0]["selector"] == "div.amount::text" and saved[0]["site"] == "127.0.0.1"


def test_a_bad_answer_is_logged_and_skipped(base, caplog):
    repair = netweir.repair.llm(complete=lambda prompt: "I can't help with that.")
    SITE["html"] = BEFORE
    crawl(base, repair)
    SITE["html"] = AFTER
    with caplog.at_level(logging.WARNING, logger="netweir"):
        spider, items, stats = crawl(base, repair)
    assert spider.repairs == [] and stats["repair_proposals"] == 0
    assert "no usable selector" in caplog.text


def test_a_proposal_that_matches_nothing_says_so(base):
    repair = netweir.repair.llm(
        complete=lambda p: '{"kind": "xpath", "selector": "//span[@id=\'nope\']", "reason": "x"}'
    )
    SITE["html"] = BEFORE
    crawl(base, repair)
    SITE["html"] = AFTER
    spider, _, _ = crawl(base, repair)
    (proposal,) = spider.repairs
    assert proposal.matches is None and proposal.score is None


def test_the_anthropic_provider_needs_its_package():
    try:
        import anthropic  # noqa: F401
    except ImportError:
        with pytest.raises(ImportError, match="pip install anthropic"):
            netweir.repair.anthropic()
    else:
        assert callable(netweir.repair.anthropic(client=object()))
