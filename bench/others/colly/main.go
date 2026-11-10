// bench/crawl.py's crawl with Colly: every site from its first page, every
// page's title and price, 100 requests in flight in all and 8 per site.
// Prints the number of items.
package main

import (
	"fmt"
	"net/http"
	"os"
	"strconv"
	"sync/atomic"

	"github.com/gocolly/colly/v2"
)

// gated caps requests in flight across every site, which Colly's limit
// rules don't: each rule's limit is its own.
type gated struct {
	next http.RoundTripper
	gate chan struct{}
}

func (g gated) RoundTrip(r *http.Request) (*http.Response, error) {
	g.gate <- struct{}{}
	defer func() { <-g.gate }()
	return g.next.RoundTrip(r)
}

func main() {
	port := os.Args[1]
	arg := func(i, def int) int {
		if len(os.Args) > i {
			n, err := strconv.Atoi(os.Args[i])
			if err != nil {
				panic(err)
			}
			return n
		}
		return def
	}
	sites, concurrency, perSite := arg(2, 100), arg(3, 100), arg(4, 8)

	c := colly.NewCollector(colly.Async(true), colly.IgnoreRobotsTxt(), colly.MaxDepth(0))
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.MaxIdleConnsPerHost = perSite
	transport.MaxIdleConns = concurrency
	c.WithTransport(gated{transport, make(chan struct{}, concurrency)})
	for s := 0; s < sites; s++ {
		// One rule per site, so each site has its own limit.
		if err := c.Limit(&colly.LimitRule{DomainGlob: fmt.Sprintf("site%d.localhost*", s), Parallelism: perSite}); err != nil {
			panic(err)
		}
	}

	var items atomic.Int64
	c.OnHTML("html", func(e *colly.HTMLElement) {
		_, _ = e.ChildText("h1.title"), e.ChildText("p.price")
		items.Add(1)
		e.ForEach("a.next", func(_ int, a *colly.HTMLElement) {
			_ = a.Request.Visit(a.Attr("href"))
		})
	})
	c.OnError(func(r *colly.Response, err error) {
		fmt.Fprintln(os.Stderr, r.Request.URL, err)
	})
	for s := 0; s < sites; s++ {
		if err := c.Visit(fmt.Sprintf("http://site%d.localhost:%s/p/0", s, port)); err != nil {
			panic(err)
		}
	}
	c.Wait()
	fmt.Println(items.Load())
}
