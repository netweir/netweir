# Settings

`netweir.Settings` holds every option a crawl has, each with a default that
is safe to start from. Set them on a spider:

```python
class Books(netweir.Spider):
    settings = netweir.Settings(concurrency=16, max_depth=3)
```

or from the shell with `-s name=value` (`-s max_depth=none` for `None`,
and commas between the proxies of `-s proxies=...`). A misspelt name is an
error, not ignored. Times are in seconds.

## Fetching

| Setting | Default | Does |
|---|---|---|
| `profile` | `"chrome"` | the browser every request looks like: `"chrome"`, `"firefox"` or `"safari"` (the newest of each), or a version such as `"safari-27-macos"` |
| `proxy` | `None` | an `http://`, `https://` or `socks5://` proxy for every request (`socks5h://` to have the proxy look names up); any other scheme is an error |
| `proxies` | `()` | proxies a site moves through each time it blocks a session |
| `timeout` | `30.0` | the limit for one whole request |
| `max_response_size` | `67108864` | the most bytes of a response body (64 MiB), counted after decompression, so a small compressed answer that unpacks to gigabytes is stopped too; a larger one fails with `FetchError(kind="too_large")`, and `None` reads any size |

## Pace and politeness

| Setting | Default | Does |
|---|---|---|
| `concurrency` | `64` | requests in flight across all sites |
| `per_domain` | `8` | requests in flight to one site |
| `obey_robots` | `True` | read robots.txt and stay out of what it disallows |
| `robots_agent` | `"netweir"` | the name matched against robots.txt's User-agent lines |
| `obey_tdmrep` | `True` | skip pages whose owners reserve text and data mining rights |
| `throttle` | `True` | adapt each site's delay to how fast it answers |
| `start_delay` | `1.0` | each site's delay before its first answer is timed |
| `min_delay` | `0.0` | the shortest delay the throttle sets |
| `max_delay` | `60.0` | the longest delay, from the throttle, blocks or Crawl-delay |
| `target_concurrency` | `1.0` | requests the throttle aims to have in flight to one site |

## Limits

| Setting | Default | Does |
|---|---|---|
| `max_depth` | `None` | links from a start page beyond which requests are dropped |
| `max_pages_per_domain` | `None` | requests accepted for one site beyond which more are dropped |
| `max_items` | `None` | stop the crawl once it has delivered this many items |
| `max_pages` | `None` | send no more than this many requests, a retry counting as one, then stop |
| `max_errors` | `None` | stop the crawl after this many errors in callbacks and pipelines |
| `max_time` | `None` | stop the crawl after this many seconds |

## Recovery

| Setting | Default | Does |
|---|---|---|
| `retries` | `3` | further tries after a server error, a network error, throttling or a block |
| `backoff_base` | `1.0` | retry n waits a random time up to `backoff_base` × 2ⁿ |
| `backoff_max` | `60.0` | the most any retry waits |
| `breaker_window` | `50` | how many of a site's last responses the circuit breaker looks at |
| `breaker_ratio` | `0.3` | the share of those that were blocks above which the site pauses |
| `breaker_pause` | `300.0` | how long the site pauses |

## Browser

| Setting | Default | Does |
|---|---|---|
| `browser` | `"off"` | which requests go through Chrome: `"off"` (those with `browser=True`), `"on_block"` (also any still blocked after its retries) or `"always"` |
| `browser_pages` | `4` | Chrome pages open at once |

## State

| Setting | Default | Does |
|---|---|---|
| `checkpoint` | `None` | a directory to keep the crawl's state in, so it resumes after a crash |
| `cache` | `None` | a directory to keep responses in, so a rerun gets its pages from there rather than the site; see [Crawling](crawling.md#caching-while-you-develop) |
| `cache_expiry` | `None` | seconds after which a cached response is fetched again; `None` keeps it for good |
| `track_threshold` | `0.75` | how similar an element must be to count as a tracked selector's element |
| `fail_fast` | `False` | stop the crawl at the first exception in a callback or pipeline |
| `workers` | `1` | processes to run callbacks in; see [Crawling](crawling.md#callbacks-in-worker-processes) |
