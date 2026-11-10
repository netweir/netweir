// bench/browser.py's job with Puppeteer: render every URL on stdin, four
// pages at a time in one browser, and pull each item's name and price.
// Prints one JSON line per page, then the count. With "visible" after the
// page count, each page gets its own context, so its own window, at
// netweir's window size: drawn in full, as netweir's pages are, instead of
// as a background tab.
import puppeteer from 'puppeteer';
import { readFileSync } from 'node:fs';

const [browser, executablePath, pages = '4', visible] = process.argv.slice(2);
const urls = readFileSync(0, 'utf8').split('\n').filter(Boolean);

const instance = await puppeteer.launch({ browser, executablePath, headless: true });
let next = 0;
const found = [];
async function worker() {
    const page = visible
        ? await (await instance.createBrowserContext()).newPage()
        : await instance.newPage();
    if (visible) await page.setViewport({ width: 1280, height: 807 });
    while (next < urls.length) {
        const url = urls[next++];
        await page.goto(url, { waitUntil: 'load' });
        await page.waitForSelector('article.item h2.name');
        found.push(
            await page.$eval('article.item', (a) => ({
                url: location.href,
                name: a.querySelector('h2.name').textContent,
                price: a.querySelector('p.price').textContent,
            })),
        );
    }
    await page.close();
}
await Promise.all(Array.from({ length: Number(pages) }, worker));
await instance.close();
for (const item of found) console.log(JSON.stringify(item));
console.log(found.length);
