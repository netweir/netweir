## The final boss pages, 2026-11-22

Apple M2 Max, 12 cores, 32 GB, macOS 26.7.2 (arm64); load 8.3, 6.14, 3.86 at the start. Python 3.13.15, rustc 1.100.0 (0ef1bc386 2026-11-09), go version go1.27.2 darwin/arm64, Node v22.14.0. Netweir at dcfeef5.

Versions: colly v2.3.0, crawl4ai 0.9.5, crawlee 1.11.1, httpx 0.28.1, netweir 0.8.0, scrapling 0.5.1, scrapy 2.20.0, selectolax 1.0.2, spider-rs 2.53.9.

100 sites of 10 pages, 50 ms per answer, 8 per site. Each round waited for a load below 4.0. Each figure is the median of 3 runs; pages/s gives the slowest and fastest run in brackets. Wrong, missing and twice are the worst run's.

### 100 in flight

| Crawler | Pages/s | Latency p50 | p95 | Client p50 | p95 | Server p50 | CPU/page | CPU use | Peak memory | In flight | MB/in flight | Wrong | Missing | Twice |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| netweir | 1,188 (1,174–1,259) | 59 ms | 99 ms | 6.7 ms | 48.4 ms | 50 ms | 1.83 ms | 2.20 | 914 MB | 100 | 9.14 | 0 | 0 | 0 |
| colly | 1,015 (665–1,056) | 52 ms | 70 ms | 0.8 ms | 18.9 ms | 50 ms | 4.55 ms | 4.42 | 923 MB | 100 | 9.23 | 8 | 92 | 0 |
| httpx+selectolax | 254 (222–261) | 97 ms | 265 ms | 46.5 ms | 213.1 ms | 50 ms | 3.95 ms | 1.01 | 2760 MB | 100 | 27.60 | 8 | 9 | 0 |
| crawlee | 209 (199–224) | 110 ms | 908 ms | 59.7 ms | 857.7 ms | 50 ms | 8.01 ms | 1.68 | 1987 MB | 100 | 19.87 | 0 | 0 | 0 |
| scrapy | 182 (142–190) | 104 ms | 195 ms | 52.9 ms | 141.1 ms | 51 ms | 5.38 ms | 0.98 | 835 MB | 97 | 8.46 | 0 | 12 | 0 |
| spider-rs | failed in 3 of 3 runs |

