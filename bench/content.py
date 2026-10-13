"""How long finding the main content, and writing Markdown and text, take
on large pages.

    uv run python bench/content.py

Two 1 MB pages: a long article inside a page with navigation, a sidebar
and a thousand comments, and the shop listing the parse benchmark uses,
which has no single main content. The figure is the best of N runs, in
milliseconds, after the page is parsed.
"""

import argparse
import random
import time

from pages import _words, page

import netweir


def article_page(target_bytes: int, seed: int = 7) -> str:
    rng = random.Random(seed)
    nav = "".join(f'<li><a href="/s/{i}">{_words(rng, 2)}</a></li>' for i in range(40))
    paragraphs = []
    for i in range(400):
        if i % 12 == 0:
            paragraphs.append(f"<h2>{_words(rng, 4)}</h2>")
        link = f'<a href="/ref/{i}">{_words(rng, 3)}</a>'
        paragraphs.append(
            f"<p>{_words(rng, 30)}, {link} <em>{_words(rng, 4)}</em>. {_words(rng, 25)}.</p>"
        )
        if i % 40 == 0:
            paragraphs.append(f'<div class="ad-slot"><p>{_words(rng, 20)}</p></div>')
    head = (
        "<!doctype html><html lang=en><head><title>Long read</title></head><body>"
        f'<header><nav><ul>{nav}</ul></nav></header><div class="layout"><main>'
        f'<article><h1>{_words(rng, 6)}</h1>{"".join(paragraphs)}</article><section id="comments">'
    )
    tail = (
        f'</section></main><aside class="sidebar"><ul>{nav}</ul></aside></div>'
        f"<footer><p>{_words(rng, 30)}</p></footer></body></html>"
    )
    comments = []
    size = len(head) + len(tail)
    while size < target_bytes:
        c = f'<div class="comment"><a href="/u/{len(comments)}">user</a><p>{_words(rng, 40)}</p></div>'
        comments.append(c)
        size += len(c)
    return head + "".join(comments) + tail


TASKS = {
    "main_content()": lambda doc: doc.main_content(),
    "markdown(main_only=True)": lambda doc: doc.markdown("https://example.com/", main_only=True),
    "markdown()": lambda doc: doc.markdown("https://example.com/"),
    "clean_text(main_only=True)": lambda doc: doc.clean_text(main_only=True),
    "article()": lambda doc: doc.article("https://example.com/"),
    "parse (for scale)": None,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=15)
    args = ap.parse_args()
    for label, html in [
        ("1 MB article with comments", article_page(1_000_000)),
        ("1 MB listing", page(1_000_000)),
    ]:
        doc = netweir.parse(html)
        print(f"\n{label} ({len(html.encode()) // 1000} KB)")
        for name, task in TASKS.items():
            best = float("inf")
            for _ in range(args.runs):
                start = time.perf_counter()
                if task is None:
                    netweir.parse(html)
                else:
                    task(doc)
                best = min(best, time.perf_counter() - start)
            print(f"  {name:28} {best * 1000:8.2f} ms")


if __name__ == "__main__":
    main()
