// bench/crawl.py's crawl with Colly: every site from its first page, every
// page's title and price, 100 requests in flight in all and 8 per site.
// Prints the number of items, and writes each one, with the time it was
// extracted, as a JSON line to the file BENCH_ITEMS names.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"strconv"
	"sync"
	"sync/atomic"
	"time"

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

	type item struct {
		URL   string  `json:"url"`
		Title string  `json:"title"`
		Price string  `json:"price"`
		T     float64 `json:"t"`
	}
	var (
		items atomic.Int64
		mu    sync.Mutex
		found []item
	)
	c.OnHTML("html", func(e *colly.HTMLElement) {
		title, price := e.ChildText("h1.title"), e.ChildText("p.price")
		t := float64(time.Now().UnixNano()) / 1e9
		mu.Lock()
		found = append(found, item{e.Request.URL.String(), title, price, t})
		mu.Unlock()
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
	if path := os.Getenv("BENCH_ITEMS"); path != "" {
		f, err := os.Create(path)
		if err != nil {
			panic(err)
		}
		w := bufio.NewWriter(f)
		enc := json.NewEncoder(w)
		for _, it := range found {
			if err := enc.Encode(it); err != nil {
				panic(err)
			}
		}
		if err := w.Flush(); err != nil {
			panic(err)
		}
		if err := f.Close(); err != nil {
			panic(err)
		}
	}
	fmt.Println(items.Load())
}
