"""bench/realworld/run.py's own logic: the URLs it sends, the CI sample
it takes, and what its report counts as a failure."""

import importlib.util
from pathlib import Path

_path = Path(__file__).resolve().parent.parent / "bench" / "realworld" / "run.py"
_spec = importlib.util.spec_from_file_location("realworld_run", _path)
run = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(run)


def test_urls_go_out_as_a_browser_sends_them():
    assert run.to_uri("https://news.ycombinator.com/item?id=1") == (
        "https://news.ycombinator.com/item?id=1"
    )
    assert run.to_uri("https://bücher.de/a%20b") == "https://xn--bcher-kva.de/a%20b"
    assert run.to_uri("https://ar.wikipedia.org/wiki/لغة") == (
        "https://ar.wikipedia.org/wiki/%D9%84%D8%BA%D8%A9"
    )


def test_the_sample_spreads_over_the_list_and_moves_each_week():
    urls = list(range(3000))
    week1, week2 = run.sample(urls, 300, 1), run.sample(urls, 300, 2)
    assert len(week1) == 300 and len(set(week1)) == 300
    assert week1[0] < 10 and week1[-1] > 2980
    assert set(week1).isdisjoint(week2)
    assert run.sample(urls[:100], 300, 1) == urls[:100]


def test_the_report_counts_failures_and_not_allowlisted_differences():
    page = {"source": "tranco:1", "sha256": "0" * 64, "bytes": 5000, "netweir_ms": 1.0}
    pages = [
        {**page, "url": "https://a.example/", "encoding": "UTF-8"},
        {**page, "url": "https://b.example/", "allowed": ["processing-instructions"]},
        {**page, "url": "https://c.example/", "failure": {"kind": "hang", "detail": "x"}},
        {
            **page,
            "url": "https://d.example/",
            "difference": {"line": 3, "netweir": "", "html5ever": ""},
        },
        {
            "url": "https://e.example/",
            "source": "-",
            "error": "robots.txt disallows",
            "robots": True,
        },
    ]
    data, md = run.report(pages, 5, "now")
    h = data["headline"]
    assert (h["parsed"], h["hangs"], h["trees_differ"], h["robots_disallowed"]) == (4, 1, 1, 1)
    assert h["trees_differ_allowlisted"] == {"processing-instructions": 1}
    assert "hang in ?: https://c.example/" in md
    assert "--check hang" in md and "--check html5ever" in md
