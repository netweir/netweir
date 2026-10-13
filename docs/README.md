# Netweir documentation

- [Getting started](getting-started.md): install, fetch a page, write and run a spider.
- [Selecting](selecting.md): CSS, XPath and Beautiful Soup style queries; structured data, prices, dates and the data in scripts.
- [Trying a page](shell.md): the page Netweir gets, on stdout, in your browser or at a Python prompt.
- [Forms, logins and APIs](forms.md): POST and other methods, form submission, JSON APIs.
- [Crawling](crawling.md): spiders, requests, pipelines, middlewares, signals, exporters and the command line.
- [Declarative spiders](declarative.md): items and rules that run in Rust.
- [Self-healing](self-healing.md): blocks, checkpoints, tracked selectors and repair proposals.
- [One crawl, many machines](distributed.md): processes on several machines sharing one queue in Redis.
- [Browser](browser.md): drive Chrome for pages that need JavaScript.
- [Running spiders as a service](serve.md): `netweir serve`, jobs scheduled, watched and cancelled over HTTP, as with scrapyd.
- [Settings](settings.md): every setting and its default.

The designs behind it, for anyone working on Netweir:

- [design/v0.1.md](design/v0.1.md): the engine, the parser and the first release.
- [design/browser.md](design/browser.md): the Chrome driver.
- [design/network.md](design/network.md): reading what a Chrome page fetches, and blocking or changing its requests.
- [design/requests.md](design/requests.md): requests other than GETs.
- [design/workers.md](design/workers.md): callbacks in worker processes.
- [design/cache.md](design/cache.md): the development cache.
- [design/shell.md](design/shell.md): the fetch, view and shell commands.
- [design/hooks.md](design/hooks.md): middlewares, signals and pipeline lifecycles.
- [design/exports.md](design/exports.md): JSON, XML, cloud storage and batches.
- [design/monitoring.md](design/monitoring.md): item validation and end-of-crawl checks.
- [design/proxies.md](design/proxies.md): proxy pools, rotation and health.
- [design/scrapy.md](design/scrapy.md): Netweir as Scrapy's download handler.
- [design/archive.md](design/archive.md): WARC and HAR files of what a crawl fetched.
- [design/helpers.md](design/helpers.md): prices, dates and the data in a page's scripts.
- [design/distributed.md](design/distributed.md): one crawl shared by many machines through Redis.
- [design/serve.md](design/serve.md): the job service.
