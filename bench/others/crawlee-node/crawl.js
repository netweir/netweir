// bench/crawl.py's crawl with Crawlee for Node.js: every site from its
// first page, every page's title and price, 100 requests in flight. Crawlee
// has no per-site limit. Prints the number of items, and writes each one,
// with the time it was extracted, as a JSON line to the file BENCH_ITEMS
// names.
import { writeFileSync } from 'node:fs';
import { CheerioCrawler, Configuration, log, LogLevel } from 'crawlee';

const [port, sites = '100', concurrency = '100'] = process.argv.slice(2);
log.setLevel(LogLevel.OFF);

let items = 0;
const found = [];
const crawler = new CheerioCrawler(
    {
        minConcurrency: Number(concurrency),
        maxConcurrency: Number(concurrency),
        maxRequestRetries: 0,
        async requestHandler({ request, $, enqueueLinks }) {
            const [title, price] = ['h1.title', 'p.price'].map((q) => {
                const found = $(q).first();
                return found.length ? found.text() : null;
            });
            const t = (performance.timeOrigin + performance.now()) / 1000;
            found.push(JSON.stringify({ url: request.loadedUrl ?? request.url, title, price, t }));
            items += 1;
            await enqueueLinks({ selector: 'a.next' });
        },
    },
    new Configuration({ persistStorage: false }),
);
const starts = Array.from({ length: Number(sites) }, (_, s) => `http://site${s}.localhost:${port}/p/0`);
await crawler.run(starts);
if (process.env.BENCH_ITEMS) writeFileSync(process.env.BENCH_ITEMS, found.map((l) => l + '\n').join(''));
console.log(items);
