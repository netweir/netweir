## A whole crawl, 2026-11-22

Apple M2 Max, 12 cores, 32 GB, macOS 26.7.2 (arm64); load 2.48, 3.78, 4.19 at the start. Python 3.13.15, rustc 1.100.0 (0ef1bc386 2026-11-09), go version go1.27.2 darwin/arm64, Node v22.14.0. Netweir at b089aa4.

Versions: colly v2.3.0, crawl4ai 0.9.5, crawlee 1.11.1, crawlee-node 3.18.2, httpx 0.28.1, netweir 0.8.0, scrapling 0.5.1, scrapy 2.20.0, selectolax 1.0.2, spider-rs 2.53.9.

100 sites of 100 pages, 50 ms per answer, 8 per site. Each round waited for a load below 4.0. Each figure is the median of 3 runs; pages/s gives the slowest and fastest run in brackets. Wrong, missing and twice are the worst run's.

### 10 in flight

| Crawler | Pages/s | Latency p50 | p95 | Client p50 | p95 | Server p50 | CPU/page | CPU use | Peak memory | In flight | MB/in flight | Wrong | Missing | Twice |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| netweir | 191 (191–192) | 53 ms | 56 ms | 2.2 ms | 4.5 ms | 51 ms | 0.63 ms | 0.12 | 87 MB | 10 | 8.70 | 0 | 0 | 0 |
| scrapy | 174 (171–174) | 55 ms | 58 ms | 4.3 ms | 7.4 ms | 51 ms | 1.33 ms | 0.23 | 114 MB | 10 | 11.41 | 0 | 0 | 0 |
| colly | 188 (187–189) | 51 ms | 53 ms | 0.3 ms | 1.5 ms | 51 ms | 0.89 ms | 0.17 | 96 MB | 10 | 9.56 | 0 | 0 | 0 |
| spider-rs | 119 (116–120) | 52 ms | 53 ms | 0.7 ms | 1.6 ms | 51 ms | 0.48 ms | 0.06 | 286 MB | 8 | 35.70 | 0 | 0 | 0 |
| crawlee | 176 (170–179) | 53 ms | 58 ms | 2.4 ms | 7.6 ms | 50 ms | 1.39 ms | 0.24 | 210 MB | 10 | 21.03 | 0 | 0 | 0 |

### 50 in flight

| Crawler | Pages/s | Latency p50 | p95 | Client p50 | p95 | Server p50 | CPU/page | CPU use | Peak memory | In flight | MB/in flight | Wrong | Missing | Twice |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| netweir | 944 (942–945) | 55 ms | 60 ms | 4.3 ms | 8.7 ms | 51 ms | 0.41 ms | 0.38 | 94 MB | 50 | 1.89 | 0 | 0 | 0 |
| scrapy | 748 (747–778) | 59 ms | 69 ms | 8.7 ms | 17.9 ms | 51 ms | 0.82 ms | 0.62 | 118 MB | 50 | 2.35 | 0 | 0 | 0 |
| colly | 935 (932–936) | 51 ms | 52 ms | 0.2 ms | 1.4 ms | 50 ms | 0.72 ms | 0.68 | 99 MB | 50 | 1.97 | 0 | 0 | 1 |
| spider-rs | 690 (688–690) | 52 ms | 55 ms | 0.7 ms | 2.9 ms | 51 ms | 0.37 ms | 0.26 | 298 MB | 48 | 6.20 | 0 | 0 | 0 |
| crawlee | 839 (818–859) | 52 ms | 58 ms | 1.5 ms | 7.3 ms | 50 ms | 0.93 ms | 0.77 | 302 MB | 50 | 6.03 | 0 | 0 | 0 |

### 100 in flight

| Crawler | Pages/s | Latency p50 | p95 | Client p50 | p95 | Server p50 | CPU/page | CPU use | Peak memory | In flight | MB/in flight | Wrong | Missing | Twice |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| netweir | 1,853 (1,850–1,862) | 55 ms | 60 ms | 4.2 ms | 9.1 ms | 51 ms | 0.29 ms | 0.54 | 97 MB | 100 | 0.97 | 0 | 0 | 0 |
| scrapy | 1,216 (1,161–1,252) | 65 ms | 91 ms | 14.8 ms | 40.4 ms | 51 ms | 0.76 ms | 0.92 | 121 MB | 100 | 1.21 | 0 | 0 | 0 |
| colly | 1,851 (1,851–1,859) | 50 ms | 52 ms | 0.2 ms | 1.1 ms | 50 ms | 0.52 ms | 0.96 | 105 MB | 100 | 1.05 | 0 | 0 | 0 |
| spider-rs | 1,313 (1,288–1,314) | 51 ms | 55 ms | 0.5 ms | 3.4 ms | 51 ms | 0.30 ms | 0.39 | 309 MB | 96 | 3.22 | 0 | 0 | 0 |
| crawlee | 1,499 (1,367–1,539) | 53 ms | 69 ms | 3.0 ms | 18.1 ms | 50 ms | 0.83 ms | 1.24 | 313 MB | 100 | 3.13 | 0 | 0 | 0 |

### 200 in flight

| Crawler | Pages/s | Latency p50 | p95 | Client p50 | p95 | Server p50 | CPU/page | CPU use | Peak memory | In flight | MB/in flight | Wrong | Missing | Twice |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| netweir | 3,339 (3,318–3,526) | 53 ms | 61 ms | 2.9 ms | 10.0 ms | 50 ms | 0.31 ms | 1.03 | 107 MB | 200 | 0.53 | 0 | 0 | 0 |
| scrapy | 1,258 (1,188–1,296) | 112 ms | 151 ms | 61.7 ms | 100.8 ms | 51 ms | 0.78 ms | 0.98 | 128 MB | 200 | 0.64 | 0 | 0 | 0 |
| colly | 3,634 (3,631–3,643) | 50 ms | 52 ms | 0.1 ms | 0.9 ms | 50 ms | 0.42 ms | 1.54 | 116 MB | 200 | 0.58 | 0 | 0 | 0 |
| spider-rs | 2,920 (2,901–2,927) | 51 ms | 54 ms | 0.4 ms | 2.2 ms | 51 ms | 0.30 ms | 0.88 | 333 MB | 200 | 1.66 | 0 | 0 | 0 |
| crawlee | 1,656 (1,623–1,694) | 74 ms | 100 ms | 24.0 ms | 49.4 ms | 50 ms | 0.85 ms | 1.42 | 295 MB | 200 | 1.47 | 0 | 0 | 0 |

### 500 in flight

| Crawler | Pages/s | Latency p50 | p95 | Client p50 | p95 | Server p50 | CPU/page | CPU use | Peak memory | In flight | MB/in flight | Wrong | Missing | Twice |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| netweir | 7,357 (6,852–7,464) | 56 ms | 62 ms | 5.7 ms | 11.8 ms | 50 ms | 0.25 ms | 1.87 | 146 MB | 500 | 0.29 | 0 | 0 | 0 |
| scrapy | 1,247 (1,220–1,249) | 280 ms | 345 ms | 229.6 ms | 294.2 ms | 51 ms | 0.79 ms | 0.99 | 149 MB | 322 | 0.46 | 0 | 0 | 0 |
| colly | 8,449 (8,373–8,562) | 50 ms | 56 ms | 0.2 ms | 4.3 ms | 50 ms | 0.28 ms | 2.38 | 131 MB | 500 | 0.26 | 0 | 0 | 1 |
| spider-rs | 5,844 (5,842–5,850) | 51 ms | 53 ms | 0.3 ms | 0.9 ms | 50 ms | 0.24 ms | 1.39 | 396 MB | 496 | 0.80 | 0 | 0 | 0 |
| crawlee | 1,796 (1,774–1,798) | 150 ms | 194 ms | 100.4 ms | 143.9 ms | 50 ms | 0.75 ms | 1.33 | 298 MB | 447 | 0.67 | 0 | 0 | 0 |

### Pages a second, by requests in flight

| Crawler | 10 | 50 | 100 | 200 | 500 |
|---|--:|--:|--:|--:|--:|
| netweir | 191 | 944 | 1,853 | 3,339 | 7,357 |
| scrapy | 174 | 748 | 1,216 | 1,258 | 1,247 |
| colly | 188 | 935 | 1,851 | 3,634 | 8,449 |
| spider-rs | 119 | 690 | 1,313 | 2,920 | 5,844 |
| crawlee | 176 | 839 | 1,499 | 1,656 | 1,796 |

### Latency p95, by requests in flight

| Crawler | 10 | 50 | 100 | 200 | 500 |
|---|--:|--:|--:|--:|--:|
| netweir | 56 ms | 60 ms | 60 ms | 61 ms | 62 ms |
| scrapy | 58 ms | 69 ms | 91 ms | 151 ms | 345 ms |
| colly | 53 ms | 52 ms | 52 ms | 52 ms | 56 ms |
| spider-rs | 53 ms | 55 ms | 55 ms | 54 ms | 53 ms |
| crawlee | 58 ms | 58 ms | 69 ms | 100 ms | 194 ms |

### Peak memory, by requests in flight

| Crawler | 10 | 50 | 100 | 200 | 500 |
|---|--:|--:|--:|--:|--:|
| netweir | 87 MB | 94 MB | 97 MB | 107 MB | 146 MB |
| scrapy | 114 MB | 118 MB | 121 MB | 128 MB | 149 MB |
| colly | 96 MB | 99 MB | 105 MB | 116 MB | 131 MB |
| spider-rs | 286 MB | 298 MB | 309 MB | 333 MB | 396 MB |
| crawlee | 210 MB | 302 MB | 313 MB | 295 MB | 298 MB |

