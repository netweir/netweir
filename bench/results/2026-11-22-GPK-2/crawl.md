## A whole crawl, 2026-11-22

Apple M2 Max, 12 cores, 32 GB, macOS 26.7.2 (arm64); load 2.35, 3.63, 4.11 at the start. Python 3.13.15, rustc 1.100.0 (0ef1bc386 2026-11-09), go version go1.27.2 darwin/arm64, Node v22.14.0. Netweir at dcfeef5.

Versions: colly v2.3.0, crawl4ai 0.9.5, crawlee 1.11.1, httpx 0.28.1, netweir 0.8.0, scrapling 0.5.1, scrapy 2.20.0, selectolax 1.0.2, spider-rs 2.53.9.

100 sites of 100 pages, 50 ms per answer, 8 per site. Each round waited for a load below 4.0. Each figure is the median of 3 runs; pages/s gives the slowest and fastest run in brackets. Wrong, missing and twice are the worst run's.

### 100 in flight

| Crawler | Pages/s | Latency p50 | p95 | Client p50 | p95 | Server p50 | CPU/page | CPU use | Peak memory | In flight | MB/in flight | Wrong | Missing | Twice |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| netweir | 1,813 (1,810–1,817) | 58 ms | 64 ms | 6.5 ms | 12.0 ms | 52 ms | 0.33 ms | 0.59 | 102 MB | 100 | 1.02 | 0 | 0 | 0 |

