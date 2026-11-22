## The final boss pages, 2026-11-22

Apple M2 Max, 12 cores, 32 GB, macOS 26.7.2 (arm64); load 2.07, 2.76, 4.84 at the start. Python 3.13.15, rustc 1.100.0 (0ef1bc386 2026-11-09), go version go1.27.2 darwin/arm64, Node v22.14.0. Netweir at b089aa4.

Versions: colly v2.3.0, crawl4ai 0.9.5, crawlee 1.11.1, crawlee-node 3.18.2, httpx 0.28.1, netweir 0.8.0, scrapling 0.5.1, scrapy 2.20.0, selectolax 1.0.2, spider-rs 2.53.9.

100 sites of 10 pages, 50 ms per answer, 8 per site. Each round waited for a load below 4.0. Each figure is the median of 3 runs; pages/s gives the slowest and fastest run in brackets. Wrong, missing and twice are the worst run's.

### 100 in flight

| Crawler | Pages/s | Latency p50 | p95 | Client p50 | p95 | Server p50 | CPU/page | CPU use | Peak memory | In flight | MB/in flight | Wrong | Missing | Twice |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| netweir | 633 (619–659) | 469 ms | 743 ms | 418.5 ms | 692.7 ms | 50 ms | 1.76 ms | 1.12 | 662 MB | 100 | 6.62 | 0 | 0 | 0 |
| scrapy | 187 (176–187) | 116 ms | 209 ms | 64.3 ms | 157.1 ms | 51 ms | 5.28 ms | 0.99 | 928 MB | 97 | 9.87 | 0 | 13 | 0 |
| colly | 1,148 (1,019–1,228) | 51 ms | 70 ms | 0.7 ms | 19.4 ms | 50 ms | 4.36 ms | 4.80 | 1093 MB | 100 | 10.93 | 8 | 92 | 0 |
| spider-rs | failed in 3 of 3 runs |
| crawlee | 219 (215–234) | 112 ms | 1439 ms | 60.6 ms | 1388.6 ms | 51 ms | 7.67 ms | 1.68 | 2044 MB | 100 | 20.44 | 0 | 0 | 0 |
| httpx+selectolax | 265 (255–272) | 97 ms | 239 ms | 47.2 ms | 188.5 ms | 50 ms | 3.89 ms | 1.03 | 2918 MB | 100 | 29.18 | 8 | 9 | 0 |

