"""bench/crawl.py's own logic: the answers it checks crawlers against, the
final boss pages it serves, and the figures it takes from a run."""

import importlib.util
import re
from pathlib import Path

_path = Path(__file__).resolve().parent.parent / "bench" / "crawl.py"
_spec = importlib.util.spec_from_file_location("bench_crawl", _path)
crawl = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(crawl)


def test_the_answers_are_what_the_pages_hold():
    for site, n in [(0, 0), (7, 33), (99, 99)]:
        html = crawl.page(site, n).decode()
        title, price = crawl.answer(site, n)
        assert f"<h1 class='title'>{title}</h1>" in html
        assert f"<p class='price'>{price}</p>" in html


def test_boss_pages_get_the_crawl_at_the_start_of_their_body():
    body = b"<!doctype html><HTML><head><meta charset=shift_jis></head><BODY class=x>\x82\xa0"
    out = crawl.boss_page(body, 3, 4)
    head, rest = out.split(b"<BODY class=x>")
    assert head == body.split(b"<BODY")[0]
    assert rest.startswith(b"<h1 class='title'>Site 3 page 4</h1><p class='price'>&pound;")
    assert rest.endswith(b"\x82\xa0")
    assert b'href="/p/' in rest


def test_boss_pages_without_a_body_get_it_after_the_bom_in_their_encoding():
    body = "﻿<p>текст".encode("utf-16-le")
    out = crawl.boss_page(body, 1, 2)
    assert out.startswith(b"\xff\xfe<\x00h\x001\x00")
    text = out.decode("utf-16")
    assert text.startswith("<h1 class='title'>Site 1 page 2</h1>")
    assert text.endswith("<p>текст")


def test_a_utf16_body_tag_is_found_only_on_a_character():
    # "㰀" is 00 3C in UTF-16BE: a "<" byte that isn't a "<".
    body = "﻿㰀body><body>x".encode("utf-16-be")
    text = crawl.boss_page(body, 0, 0).decode("utf-16")
    assert re.fullmatch(r"㰀body><body><h1 .*</ul>x", text)


def test_urls_name_their_site_by_host_or_by_port():
    ports = [5000 + s for s in range(crawl.SITES)]
    assert crawl.url_key("http://site7.localhost:4000/p/33", ports) == (7, 33)
    assert crawl.url_key("http://site7.localhost:4000/", ports) == (7, 0)
    assert crawl.url_key("http://host.docker.internal:5009/p/1", ports) == (9, 1)
    assert crawl.url_key("http://site7.localhost:4000/robots.txt", ports) is None
    assert crawl.url_key("http://site700.localhost:4000/p/1", ports) is None


def test_percentiles_are_nearest_rank():
    values = list(range(1, 101))
    assert crawl.percentile(values, 50) == 50
    assert crawl.percentile(values, 95) == 95
    assert crawl.percentile([3.0], 95) == 3.0
    assert crawl.percentile([], 50) is None


def test_in_flight_counts_overlapping_requests():
    seen = [(0, 0, 0.0, 1.0), (0, 1, 0.5, 1.5), (0, 2, 1.0, 2.0), (0, 3, 3.0, 4.0)]
    assert crawl.most_in_flight(seen) == 2


def test_figures_count_wrong_missing_and_twice():
    ports = list(range(crawl.SITES))
    seen = [(1, 2, 10.0, 10.05), (1, 3, 10.0, 10.05), (1, 3, 11.0, 11.05)]
    title, price = crawl.answer(1, 2)
    items = [
        {"url": "http://site1.localhost:1/p/2", "title": f" {title}\n", "price": price, "t": 10.06},
        {"url": "http://site1.localhost:1/p/3", "title": "wrong", "price": price, "t": 11.07},
        {"url": "http://site1.localhost:1/p/2", "title": title, "price": price, "t": 12.0},
    ]
    run = {"items": items, "seen": seen, "count": 3, "wall_s": 2.0, "cpu_s": 1.0}
    f = crawl.figures({**run, "peak_rss_mb": 50.0}, ports)
    assert (f["wrong"], f["twice"]) == (1, 1)
    assert f["missing"] == crawl.SITES * crawl.PAGES - 2
    assert f["peak_in_flight"] == 2 and f["mb_per_in_flight"] == 25.0
    assert f["cpu_util"] == 0.5
    # The second item matches the second answer for its page, not the first.
    assert round(f["latency_ms_p50"]) == 70 and round(f["client_ms_p50"]) == 20
