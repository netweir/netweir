// bench/crawl.py's crawl with Crawlee for Node.js: every site from its
// first page, every page's title and price, 100 requests in flight. Crawlee
// has no per-site limit. Prints the number of items.
import { CheerioCrawler, Configuration, log, LogLevel } from 'crawlee';

const [port, sites = '100', concurrency = '100'] = process.argv.slice(2);
log.setLevel(LogLevel.OFF);

let items = 0;
const crawler = new CheerioCrawler(
    {
        minConcurrency: Number(concurrency),
        maxConcurrency: Number(concurrency),
        maxRequestRetries: 0,
        async requestHandler({ $, enqueueLinks }) {
            void [$('h1.title').text(), $('p.price').text()];
            items += 1;
            await enqueueLinks({ selector: 'a.next' });
        },
    },
    new Configuration({ persistStorage: false }),
);
const starts = Array.from({ length: Number(sites) }, (_, s) => `http://site${s}.localhost:${port}/p/0`);
await crawler.run(starts);
console.log(items);
