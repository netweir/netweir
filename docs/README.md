# Netweir documentation

- [Getting started](getting-started.md): install, fetch a page, write and run a spider.
- [Fetching pages](fetching.md): query strings, logins, redirects, timeouts and certificates.
- [Selecting](selecting.md): CSS, XPath and Beautiful Soup style queries; changing the page; elements like one you found; JMESPath over JSON; structured data, articles and Markdown, prices, dates and the data in scripts.
- [XML, feeds and CSV](xml.md): XML responses, namespaces, the limits on hostile XML, XMLFeedSpider and CSVFeedSpider.
- [Item loaders](loaders.md): filling items as Scrapy's loaders do, with input and output processors.
- [Trying a page](shell.md): the page Netweir gets, on stdout, in your browser or at a Python prompt.
- [Projects](project.md): `startproject`, `genspider`, `list`, `parse`, `check` (Scrapy's contracts) and `version`.
- [Forms, logins and APIs](forms.md): POST and other methods, form submission, JSON APIs.
- [Sessions and cookies](sessions.md): a client's cookies, storage state files, logging in with Chrome and crawling over HTTP, several users in one crawl.
- [Crawling](crawling.md): spiders, requests, pipelines, downloading files and images, middlewares, signals, watching a running crawl, exporters and the command line.
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
- [design/dialogs.md](design/dialogs.md): a Chrome page's dialogs, popups and downloads.
- [design/requests.md](design/requests.md): requests other than GETs.
- [design/fetch-policy.md](design/fetch-policy.md): timeouts, redirects, logins and certificates, per request and per crawl.
- [design/workers.md](design/workers.md): callbacks in worker processes.
- [design/cache.md](design/cache.md): the development cache.
- [design/shell.md](design/shell.md): the fetch, view and shell commands.
- [design/hooks.md](design/hooks.md): middlewares, spider middlewares, signals, from_crawler and pipeline lifecycles.
- [design/exports.md](design/exports.md): JSON, XML, cloud storage and batches.
- [design/files.md](design/files.md): downloading the files and images items link to, through the crawl.
- [design/monitoring.md](design/monitoring.md): item validation and end-of-crawl checks.
- [design/metrics.md](design/metrics.md): live stats, the progress line and the metrics endpoint.
- [design/proxies.md](design/proxies.md): proxy pools, rotation and health.
- [design/sessions.md](design/sessions.md): the cookie jar, named jars, storage state and handing cookies between Chrome and HTTP.
- [design/scrapy.md](design/scrapy.md): Netweir as Scrapy's download handler.
- [design/archive.md](design/archive.md): WARC and HAR files of what a crawl fetched.
- [design/helpers.md](design/helpers.md): prices, dates and the data in a page's scripts.
- [design/selecting.md](design/selecting.md): editing the tree safely, `find_similar()` and JMESPath.
- [design/xml.md](design/xml.md): XML in the same tree and XPath engine as HTML, namespaces, and what hostile XML may cost.
- [design/loaders.md](design/loaders.md): item loaders, and why they're Netweir's own rather than the itemloaders package.
- [design/threads.md](design/threads.md): what kept threads from scaling on free-threaded Python, and what replaced it.
- [design/content.md](design/content.md): finding a page's main content, reading it as an article, and Markdown.
- [design/distributed.md](design/distributed.md): one crawl shared by many machines through Redis.
- [design/serve.md](design/serve.md): the job service.
- [design/project.md](design/project.md): projects, generated spiders, contracts and the other Scrapy-style commands.
