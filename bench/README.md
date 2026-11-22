# Benchmarks

Every number Netweir publishes about speed comes from a script in this
folder, and you can run each one yourself. This page covers the crawl
benchmark, `bench/crawl.py`, from a machine with nothing on it to the
tables on the website. The other scripts say how to run them in their
docstrings: `parse.py`, `query.py`, `items.py` and `content.py` time the
parser and queries, `boss.py` throws the final boss pages at the parsers,
and `realworld/run.py` checks the parser against production pages.

## What the crawl benchmark measures

A local server plays 100 sites of 100 pages and answers every request 50
ms late. Each crawler starts at every site's front page, follows every
link and reads a title and a price from each of the 10,000 pages, with
the same limits: a number of requests in flight, 8 per site, no
robots.txt and no throttling. Each crawler runs in a process of its own,
so its CPU time and memory are its own.

For each crawler and setting, a run gives:

| Figure | What it is |
|---|---|
| Pages/s | Pages read, over the wall time of the crawler's process, start-up included. |
| Latency p50, p95 | Per page, from the request arriving at the server to the crawler holding its title and price. |
| Server p50 | Per request, from arrival to the answer written: the 50 ms, plus whatever the server itself adds. |
| Client p50, p95 | Per page, from the answer written to the crawler holding the item: reading it, waiting its turn, parsing and extracting. |
| CPU/page, CPU use | User plus system time, per page and over the wall time (1.00 is one core busy throughout). |
| Peak memory, MB/in flight | The process's peak resident set, and that divided by the most requests the server saw in flight at once. |
| In flight | The most requests the server was answering at once, which shows whether a crawler kept to its limit. |
| Wrong, Missing, Twice | Pages whose title or price didn't match the page, pages never read, and pages read more than once. |

The latency figures need two clocks to agree, and they do: the server
stamps each request with the wall clock when it arrives and when its
answer is written, and every crawler stamps each item with the same wall
clock when it has it, on the same machine. Client time is the crawler's
whole share of a page, measured from outside, so it puts network and
parsing on either side of one line for every crawler alike. It can't see
inside a crawler: for one with a queue, client time is mostly the queue.
For Netweir, `crawl.json` splits it further with Netweir's own
`response_received` signal: the time until the response reached Python
(`to_python_ms_*`), and the callback's queries and the item after
(`parse_ms_*`).

Two things the figures don't measure: the time a crawler spends before
its request reaches the server (connecting, queueing on its side), which
only shows in pages a second; and anything Firecrawl does, which comes
back at the end of its crawl, so it has the server's figures and no
latency.

## Set up a machine

These steps start from a macOS or Linux machine with only a shell. They
install the exact versions the published table used. Python 3.13 is the
one to use: Scrapling doesn't install on 3.14 yet.

1. Install [uv](https://docs.astral.sh/uv/getting-started/installation/),
   [Rust](https://rustup.rs/) (stable), [Go](https://go.dev/dl/) 1.27 or
   later, and [Node.js](https://nodejs.org/) 22 with npm. Docker is only
   for Firecrawl.

2. Clone Netweir and install the Python crawlers. The `crawl` group in
   `pyproject.toml` pins each one, and `uv.lock` holds their hashes, so
   `uv sync` installs those exact files:

   ```sh
   git clone --recurse-submodules https://github.com/netweir/netweir
   cd netweir
   uv sync --python 3.13 --group crawl
   uv run maturin develop --uv --release
   ```

3. Build the crawlers that aren't Python. Each has a lockfile, and each
   is skipped until it's built:

   ```sh
   (cd bench/others/spider && cargo build --release --locked)
   (cd bench/others/colly && go build -o bench-colly .)
   (cd bench/others/crawlee-node && npm ci)
   ```

4. For the final boss run only, clone Parsec, which generates the pages
   from a seed, and fetch its real pages (fixed Internet Archive
   snapshots, checked against their SHA-256):

   ```sh
   git clone --recurse-submodules https://github.com/parsec-html/parsec ../parsec
   ../parsec/bench/fetch-pages.sh
   ```

5. For Firecrawl, see the docstring of `bench/crawl.py`. Without it, the
   run skips Firecrawl and says so.

## Run it

Wait for a quiet machine. The script waits too, before each round of
crawlers at a setting, until the one-minute load average is below 3, and
records the load each run started at; a run at a load of 12 came out a fifth to a quarter slower for
every crawler.

```sh
uv run python bench/crawl.py --sweep            # 10, 50, 100, 200 and 500 in flight
uv run python bench/crawl.py                    # 100 in flight only
uv run python bench/crawl.py --boss --parsec ../parsec
uv run python bench/crawl.py --only netweir --only scrapy
```

Each crawler runs three times at each setting (`--repeats` changes that),
a different one first each round. A crawl still going after 600 seconds
(`--timeout`) is killed and counted as failed. On a shared machine whose
load won't settle below 3, `--quiet 4` waits for 4 instead; the results
say which. A sweep of Netweir, Scrapy, Colly, spider-rs and Crawlee for
Python took 40 minutes on an M2 Max, waits included. Crawlee for Node.js,
httpx + selectolax, Scrapling and Crawl4AI stay near or under 200 pages a
second at every setting, so each adds 12 to 35 minutes more.

`--boss` serves the final boss pages from `bench/boss.py` instead, 10 a
site: 900 tiny broken fragments, 90 real pages as served or re-encoded
(Shift_JIS, GBK, EUC-KR, windows-1251, UTF-16 with a BOM, a `<meta>` that
lies, a charset only the HTTP header gives) and 10 giants of 10 to 30 MB.
The server puts the crawl's title, price and links at the start of each
page's `<body>`, in the page's own encoding, or after its BOM where it
has no `<body>` tag, and sends the HTTP charset where the page has one.
The price's pound sign is `&pound;` there, so it reads the same in every
encoding. A crawler that misreads a page's encoding gets its title wrong
and never sees its links, so the pages only those links lead to go
missing as well. `--parsec` builds the pages the first time, and the run
stops unless their fingerprint is Parsec's, `de2480edafa2baa9`.

## Read the results

Each run writes to `bench/results/<date>-<host>/`:

- `crawl.json` or `boss.json`: the machine (CPU, cores, memory, OS, load),
  the Python, Rust, Go and Node versions, every crawler's version, the
  Netweir commit, every run's figures and the end of its output, and for
  each crawler and setting the median, min and max of every figure.
- `crawl.md` or `boss.md`: the tables, one per setting, with the slowest
  and fastest run beside each median of pages a second, then pages a
  second, p95 latency and peak memory by requests in flight.
- `raw/`: every run's items, each stamped, and every request the server
  answered, gzipped. These are too big for the repository (16 MB for a
  sweep of five crawlers) and stay out of it; CI keeps its own as the
  `bench-crawl-results` artifact.

A rerun on the same day and machine replaces that day's file.

## Reproduce the published table

The published table names the machine it ran on, and gives the slowest
and fastest of its runs beside each median. Three runs make a narrow
spread, narrower than the difference between one day's runs and the
next, so judge a rerun by how far it lands from the median, not only by
whether it lands inside the brackets.

Here's how far one rerun landed. It ran from a clean clone of the branch,
set up by the steps above in 3 minutes 20 seconds (`uv sync`, a release
build of Netweir from nothing, the three programs), once at each
setting, on the same M2 Max at loads of 3 to 8, against the
`2026-11-22-GPK` results:

| In flight | Netweir | Colly | Scrapy |
|--:|--:|--:|--:|
| 10 | 192, inside | 187, inside | 166, 3% under |
| 50 | 948, 0.3% over | 929, 0.3% under | 703, 6% under |
| 100 | 1,777, 4% under | 1,837, 0.8% under | 1,018, 12% under |
| 200 | 3,465, inside | 3,596, 1% under | 1,299, 0.2% over |
| 500 | 7,283, inside | 8,089, 3% under | 1,236, inside |

The crawlers that wait on the server came within 4% of the published
median everywhere. Scrapy, which waits on its own CPU from 100 in flight
up, moved most, and most where the load was highest. For a closer
comparison, run with `--repeats 5` or more on a machine doing nothing
else.

A different machine moves every crawler that's limited by its own CPU,
so compare the order and the ratios there, not the counts. The ones
held by the server's 50 ms (every crawler at 10 in flight, and each one
until it runs out of CPU) should come out close to the same on any
machine.

`bench/results/` holds the results of the published runs, the JSON and
Markdown without `raw/`.
