"""How calls into netweir scale across threads on free-threaded Python.

    uv run python bench/threads.py
    uv run python bench/threads.py --check   # exit 1 if 4 threads scale badly

Each case runs the same loop on 1, 2, 4 and 8 threads at once, each thread
on its own copy of the page ("separate", as a crawler parses its own pages)
or all on one ("shared", as loaders reading one page). The figure is calls
per second per thread, and efficiency is that over the one-thread figure:
1.00 is perfect scaling. With the GIL, threads take turns, so efficiency
falls as 1/n whatever netweir does; the check only means something on a
free-threaded build with 4 cores or more, and is skipped elsewhere.

The check's floor is low on purpose: before docs/design/threads.md's
fixes, node.tag kept 0.09 to 0.16 of its one-thread rate on 4 threads, and
after them every case keeps 0.55 or more on a quiet 12-core Mac. CI doesn't
run it: its bench job uses the GIL build, and shared 4-core runners would
make the figures noise.
"""

import argparse
import os
import sys
import sysconfig
import threading
import time

from pages import page

import netweir

SMALL = page(20_000)


def case_tag(root):
    node = root.css("h3 a")[0]
    return lambda: node.tag


def case_css_get(root):
    return lambda: root.css("p.price_color::text").get()


def case_parse(_root):
    return lambda: netweir.parse(SMALL)


def case_text(root):
    node = root.css("article")[0]
    return lambda: node.text


CASES = {
    "node.tag": case_tag,
    "css().get()": case_css_get,
    "parse()": case_parse,
    "node.text": case_text,
}


def rate(case, threads: int, shared: bool, seconds: float) -> float:
    """Calls per second per thread."""
    one = netweir.parse(SMALL) if shared else None
    counts = [0] * threads
    start = threading.Barrier(threads + 1)
    stop = threading.Event()

    def work(i):
        # Built on the thread that uses it: free-threaded Python counts
        # references to an object made on another thread with atomic
        # operations, which would be timed here instead of netweir.
        call = case(one or netweir.parse(SMALL))
        n = 0
        start.wait()
        while not stop.is_set():
            for _ in range(50):
                call()
            n += 50
        counts[i] = n

    pool = [threading.Thread(target=work, args=(i,)) for i in range(threads)]
    for t in pool:
        t.start()
    start.wait()
    began = time.perf_counter()
    time.sleep(seconds)
    stop.set()
    for t in pool:
        t.join()
    return sum(counts) / (time.perf_counter() - began) / threads


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="fail if 4 threads scale badly")
    ap.add_argument("--seconds", type=float, default=1.0, help="per measurement")
    ap.add_argument(
        "--floor",
        type=float,
        default=0.35,
        help="the 4-thread efficiency --check requires (default 0.35)",
    )
    args = ap.parse_args()

    free = not sys._is_gil_enabled() if hasattr(sys, "_is_gil_enabled") else False
    cores = os.cpu_count() or 1
    counts = [1, 2, 4, 8]
    pool_off = netweir._native._reference_pool_disabled
    print(
        f"python {sysconfig.get_python_version()}, free-threaded: {free}, cores: {cores}, "
        f"PyO3 reference pool: {'off' if pool_off else 'ON (a RUSTFLAGS without the cfg?)'}"
    )
    print(f"{'case':<14}{'page':<10}" + "".join(f"{n:>8} thr" for n in counts) + "   eff@4")
    worst = []
    for name, case in CASES.items():
        for shared in (False, True):
            if name == "parse()" and shared:
                continue
            rates = [rate(case, n, shared, args.seconds) for n in counts]
            eff = rates[2] / rates[0]
            worst.append((eff, name, "shared" if shared else "separate"))
            cells = "".join(f"{r / 1000:>8.0f}k/s" for r in rates)
            print(f"{name:<14}{'shared' if shared else 'separate':<10}{cells}   {eff:.2f}")

    if not args.check:
        return
    if not free or cores < 4:
        print("--check skipped: needs free-threaded Python and at least 4 cores")
        return
    eff, name, kind = min(worst)
    if eff < args.floor:
        print(f"FAIL: {name} ({kind}) runs at {eff:.2f} of its one-thread rate on 4 threads")
        sys.exit(1)
    print(f"ok: every case keeps at least {args.floor:.2f} of its one-thread rate on 4 threads")


if __name__ == "__main__":
    main()
