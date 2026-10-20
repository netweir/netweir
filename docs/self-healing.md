# Self-healing

Long crawls meet three kinds of trouble: sites that block them, crashes,
and redesigns. Netweir deals with each.

## Blocks

Every response is classified before your code sees it:

| `page.outcome` | Means |
|---|---|
| `ok` | a page |
| `blocked` | a bot-protection page; `page.blocked` names the vendor |
| `throttled` | a 429, or a 503 with Retry-After |
| `payment_required` | a 402; its `crawler-price` header is reported |
| `http_error` | any other 4xx or 5xx |

Block pages are recognised for Cloudflare, Akamai, DataDome, HUMAN, Kasada,
Imperva and AWS WAF, from headers first, then cookies, then the page.
Outside a crawl, `netweir.get()` raises `netweir.Blocked` (with `vendor`,
`kind` and `page`); pass `raise_on_block=False` to get the page instead.

In a crawl, a request that fails climbs a ladder:

1. Server errors (408, 500, 502, 503, 504, 522, 524) and network errors are
   retried after a random wait that grows each time (`backoff_base`,
   `backoff_max`), up to `retries` times. Other 4xx and 402 aren't retried.
2. A block replaces the site's session: an empty cookie jar, and the next of
   `proxies` if you gave any. A jar you named with `meta["cookiejar"]` is
   kept (see [Sessions and cookies](sessions.md)). [Proxies](#proxies)
   has the other ways to use them.
3. Blocks and 429s slow the site down; Retry-After is honoured.
4. A site that blocked more than `breaker_ratio` of its last
   `breaker_window` responses is paused for `breaker_pause` seconds, with
   one warning. It judges after 10 responses, so a site that blocks from
   the start pauses quickly.
5. With `browser="on_block"`, a request still blocked after its retries is
   loaded once in Chrome, which has time to pass a challenge. If it gets
   through, the callback gets the page, and the cookies Chrome earned go to
   the site's HTTP session, so the next requests don't need Chrome.
6. A request still blocked after all that goes to your spider's
   `on_block(request, page)`, which logs it by default and may yield items
   or requests like a callback.

The stats count `retries`, `blocked`, `throttled`, `sessions_replaced`,
`breaker_trips`, `browser_fetches` and `browser_unblocked`.

## Proxies

Give a crawl a list of proxies and `proxy_rotation` says how it uses them:

```python
class Shop(netweir.Spider):
    settings = netweir.Settings(
        proxies=["http://ann:secret@10.0.0.1:8080", "http://10.0.0.2:8080"],
        proxy_rotation="per_request",
    )
```

- `"on_block"`, the default, keeps the pool in reserve. Requests go out
  through `proxy` (or straight to the site) until a site blocks one; that
  site's new session then takes the next proxy, with an empty cookie jar.
- `"per_request"` sends every request through the next proxy in turn, so
  no one address carries your crawl. The cookies stay the crawl's,
  whichever proxy carries them, as they would for a browser behind a
  rotating gateway. A block is answered by the rotation: the blocked
  site's cookies are dropped, the ones its parent domains set included
  (a login cookie for `.example.com` goes when `www.example.com` blocks
  you), and the retry goes out through another proxy.
- `"per_host"` gives each site a proxy of its own the first time it's
  fetched, and keeps it there, cookies and all, so the site sees one
  steady visitor. A block moves the site to the next proxy with a fresh
  session.

Pick `"per_host"` for sites that log you in or track a session, and
`"per_request"` for wide crawls where any one address would soon be
noticed. Either one needs `proxies`, and refuses `proxy` as well, since
it wouldn't be clear which wins.

A session costs memory: with `"per_host"`, each site keeps its own
client and cookie jar, about 40 KB, for the whole crawl. A thousand sites
is 40 MB; a crawl across a million is better off with `"per_request"`,
which shares one jar between every site.

A proxy with a login takes it in the URL, percent-encoded:
`http://ann:p%40ss@10.0.0.1:8080` for the password `p@ss`. A `/`, `?`,
`#` or `@` left as it is would move where the address seems to start, so
Netweir refuses it rather than send your requests somewhere else.

### When a proxy stops working

A proxy fails a request when Netweir can't connect to the proxy, when
the proxy wants a login (a 407, or a tunnel to an https:// site refused
with one), when it drops the tunnel half way, or when the answer is a
block page. Everything else is the site's doing, not the proxy's:

- a 404 or a 500, which is the site talking;
- a 502, 503 or 504, even from a gateway saying it couldn't reach the
  site, since the site can send the same codes;
- a tunnel the proxy wouldn't open, which is mostly the proxy saying it
  couldn't reach the site, and which Netweir can't tell from a refusal
  for the proxy's own reasons;
- a TLS error with the site at the far end of a tunnel;
- a proxy hanging up on a plain `http://` request before it answers,
  which reads exactly like an idle connection it let go of; on the way
  to an `https://` site, the same hang-up breaks the tunnel, and counts;
- a timeout, which looks the same whether the proxy or the site was the
  slow one.

Those are retried as any failure is, but they never bench a proxy, so a
crawl full of dead sites doesn't empty your pool. The price is that a
proxy which accepts connections and then never answers isn't benched:
each request through it waits out `timeout` and is retried, under
`"per_request"` through another proxy.

Three failures in a row (`proxy_failures`) and the proxy is benched: left out of the rotation for a minute (`proxy_cooldown`). If it
fails again when it's back, it sits out two minutes, then four, and so on
up to 30. One request that gets through clears its record.

What does a dead proxy cost you, then? Three requests that have to be
tried again, and a minute later one more to see whether it's back.

robots.txt and tdmrep.json go the way the first request waiting for
them goes: through its own proxy, direct, or through the rotation. A
`"per_host"` site only gets a proxy of the pool when one of its requests
takes the rotation, so a site whose requests all name their own proxy
never ties one up.

They're also different in one way: one answer decides what a whole site
allows. So when a proxy of the pool fails one, it's
fetched again through each other healthy proxy in turn until one gets an
answer, whatever `retries` says, and with `"per_host"` or after a block
the site moves to the proxy that answered. While there's a healthy proxy
left, a dead one can't shut you out of a site. If none answers, the file
counts as unreachable, as it would without a proxy: robots.txt keeps the
site off limits for this crawl, and tdmrep.json counts as none. A 407 is
the proxy talking, not the site, so it's treated the same way, never as
the site's "no robots.txt here".

When every proxy is benched, the crawl keeps going through the one due
back soonest, and says once that the pool is exhausted.

Each benching is a warning in the log, with any login in the proxy's
address hidden (`http://***@10.0.0.1:8080`), and the stats count them as
`proxies_benched`. After a run, `spider.proxy_stats` has each proxy's
numbers:

```python
spider.run()
spider.proxy_stats["http://10.0.0.2:8080"]
# {"requests": 412, "failures": 3, "benched": 1}
```

`proxy_stats` is keyed by each proxy exactly as you gave it, login and
all, so you can look one up; it's yours, and never logged. Proxies a
request named that aren't in `proxies` get a row each up to 1,000; after
that, the rest share one row, `"other"`, so a gateway that takes a new
session name in every login doesn't grow the table without end.

### One request, one proxy

`Request(url, proxy="http://10.0.0.9:8080")` sends that request through
that proxy, whatever the rotation. So does `meta={"proxy": ...}`, as in
Scrapy, and as in Scrapy, `meta={"proxy": None}` (or `""`) sends it
straight to the site, through no proxy at all, even under `"per_request"`
or `"per_host"`. `Request(proxy=None)`, the default, leaves the request
to the rotation; `proxy=""` goes direct too. If the proxy is one of
`proxies`, the request counts toward its health, and either way it shows
up in `proxy_stats`. With a checkpoint, the proxy, or the choice to go
direct, is saved with the request.

### Chrome

Chrome starts with one proxy for every page it loads, so rotation stops
at its door. With `"on_block"` it uses `proxy`; with `"per_request"` or
`"per_host"`, it takes the next healthy proxy of the pool when it starts,
passing over SOCKS proxies with a login, which Chrome can't log in to.
If that leaves none, Chrome starts without a proxy and the log says so,
once. Chrome keeps the proxy it started with, even if that proxy is
benched later: starting it again would close the pages your callbacks
are still using.
A request's own `proxy` doesn't reach a page loaded in Chrome either, and
pages loaded in Chrome aren't counted in `proxy_stats` or toward a
proxy's health.

## Checkpoints

```python
class Books(netweir.Spider):
    settings = netweir.Settings(checkpoint="crawls/books")
```

or `-s checkpoint=crawls/books` on the command line. The crawl's state lives
in `crawls/books/crawl.sqlite3`. Run the same crawl again after it stops,
however it stopped, and it resumes:

- requests it hadn't finished are fetched; ones it had aren't;
- pages it had seen stay seen;
- items already written aren't written again. Each carries an `_id` that is
  the same on every run (as a key in dict items; dataclass items are
  deduplicated by it without showing it).

JSONL and CSV output is exact across a crash: a resumed run cuts the file
back to the last point recorded and adds to it, so every item is in the file
once. A Parquet file is readable only once closed: it's written as
`books.parquet.partial` and renamed when its items are on record, and a
resumed run writes the next part beside it (`books.1.parquet`). A crawl
that can't start leaves its output files as they were.

With a checkpoint, callbacks are saved by name, so they must be methods of
the spider, and `meta` must be JSON. Delete the directory to start the crawl
over.

If a commit to the checkpoint fails (a full disk, or on Windows a file
another program holds for a moment), the crawl warns once and carries on,
and each commit after it tries the failed writes again; a second warning
says when they've gone in, and `checkpoint_failures` in the stats counts
the failed commits. Until then, a crawl that stops resumes from the last
commit that worked. If commits keep failing until a million writes are
waiting, the crawl stops with an `OSError` saying why, rather than run
out of memory holding them.

## Tracked selectors

```python
price = page.css(".price_color::text", track="price")
price.relocated   # True when the selector missed and the element was found by similarity
price.score       # 1.0 for a match, the similarity when relocated
```

Each time the selector matches, Netweir saves what the element looks like:
its tag, text, attributes, the elements around it and the text just before
it. When a redesign breaks the selector, every element of the same kind is
scored against that, and the best one is used if it scores at least
`track_threshold` (0.75). A warning says so, once per site and name in each
crawl. Below the threshold, nothing is returned, and `score` says how close
the nearest candidate came.

Tracking is careful not to invent data:

- It learns, as the crawl goes, whether an element's text and the label
  before it change from page to page (a product's price and name do), and
  doesn't hold those differences against a candidate.
- A label that never changed and now reads differently rules a candidate
  out: if the Total row is gone, the Shipping cell isn't used instead.
- If several elements still look exactly like the remembered one (the
  prices in a list), the selector missed one item, not a redesign, and
  nothing is relocated.

So track elements that should always be there. A tracked query is one
selector that finds an element, optionally with an ending such as
`::text`.

`track=` also works on `xpath()` and on Item fields. The stats count
`relocated` and `lost`. Fingerprints are kept per site in the checkpoint
file, or in `~/.netweir/tracks.db` (`$NETWEIR_HOME/tracks.db`) otherwise.

## Repair proposals

```python
class Books(netweir.Spider):
    repair = netweir.repair.llm(report="repairs.jsonl")
```

When a tracked selector breaks, the repair plugin sends the old selector,
what the element looked like and the page to a language model, and asks for
a new selector. Each proposal is checked on the page (does it parse, what
does it find, how similar is that to the old element), logged, kept on
`spider.repairs` and appended to the report. **Nothing is applied**; you
decide.

By default it uses Anthropic's API (`pip install anthropic`, with
`ANTHROPIC_API_KEY` set). Pass `complete=` any function that takes a prompt
and returns text to use another model.

The model is sent the page's HTML without scripts, styles and comments (up
to 40,000 characters), its URL, the old selector and what the element looked
like. If pages hold things you'd rather not send, such as form tokens or
personal data, use a `complete` that redacts them or calls a local model.

## Traps

`max_depth` stops following links that far from a start page,
`max_pages_per_domain` caps the requests for one site, and a URL whose path
repeats the same run of segments three times in a row (`/a/b/a/b/a/b`), or
one segment four times (`/a/a/a/a`), is refused.
