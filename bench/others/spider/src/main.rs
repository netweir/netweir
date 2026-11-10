//! bench/crawl.py's crawl with spider-rs: every site from its first page,
//! every page's title and price, 8 requests in flight per site and about
//! 100 in all. Prints the number of items.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use scraper::{Html, Selector};
use spider::website::Website;
use tokio::sync::{Semaphore, broadcast::error::RecvError};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().expect("port").parse().expect("port");
    let sites: usize = args.next().map_or(100, |s| s.parse().expect("sites"));
    let concurrency: usize = args.next().map_or(100, |s| s.parse().expect("concurrency"));
    let per_site: usize = args.next().map_or(8, |s| s.parse().expect("per site"));

    // spider's limit is per website, and a semaphore shared through
    // with_concurrency_semaphore ends crawls early (sites stop after a
    // quarter of their pages). So each site keeps its own limit and only
    // concurrency / per_site sites crawl at once: 96 in flight for 100.
    let gate = Arc::new(Semaphore::new((concurrency / per_site).max(1)));
    let items = Arc::new(AtomicUsize::new(0));
    let lost = Arc::new(AtomicUsize::new(0));
    let title = Selector::parse("h1.title").unwrap();
    let price = Selector::parse("p.price").unwrap();

    let mut crawls = Vec::new();
    for s in 0..sites {
        let mut website = Website::new(&format!("http://site{s}.localhost:{port}/p/0"));
        website
            .with_respect_robots_txt(false)
            .with_delay(0)
            .with_concurrency_limit(Some(per_site));
        let mut rx = website.subscribe(4096);
        let (items, lost, title, price) =
            (items.clone(), lost.clone(), title.clone(), price.clone());
        let reader = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(page) => {
                        let doc = Html::parse_document(&page.get_html());
                        let _ = (
                            doc.select(&title)
                                .next()
                                .map(|e| e.text().collect::<String>()),
                            doc.select(&price)
                                .next()
                                .map(|e| e.text().collect::<String>()),
                        );
                        items.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(RecvError::Lagged(n)) => {
                        lost.fetch_add(n as usize, Ordering::Relaxed);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        let gate = gate.clone();
        crawls.push(tokio::spawn(async move {
            let _turn = gate.acquire_owned().await.unwrap();
            website.crawl().await;
            website.unsubscribe();
            reader.await.unwrap();
        }));
    }
    for crawl in crawls {
        crawl.await.unwrap();
    }
    let lost = lost.load(Ordering::Relaxed);
    if lost > 0 {
        eprintln!("{lost} pages crawled but dropped before they were read");
    }
    println!("{}", items.load(Ordering::Relaxed));
}
