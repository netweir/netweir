//! Reading sitemaps: the XML `urlset` and `sitemapindex` formats of
//! sitemaps.org, gzipped or not, and plain-text sitemaps of one URL per
//! line.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use netweir_dom::xml::{XmlOptions, XmlVisitor, walk_xml};

/// What a sitemap lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Pages.
    Urlset,
    /// More sitemaps.
    Index,
}

/// One `<url>` or `<sitemap>` entry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entry {
    pub loc: String,
    pub lastmod: Option<String>,
    pub changefreq: Option<String>,
    pub priority: Option<f64>,
    /// `<xhtml:link rel="alternate" href=...>`: the page in other languages.
    pub alternates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sitemap {
    pub kind: Kind,
    pub entries: Vec<Entry>,
    /// What recovering from a broken sitemap repaired, for a warning;
    /// None when nothing needed it.
    pub repaired: Option<String>,
}

/// Parses `body`, unzipping it first if it's gzip, to at most `max` bytes.
///
/// XML is read with netweir-dom's limits on any XML: nothing is ever
/// fetched, entities a sitemap declares are expanded only so far (a
/// "billion laughs" sitemap is refused rather than filling memory), and
/// reading takes at most [`BUDGET`].
pub fn parse(body: &[u8], max: usize) -> Result<Sitemap, String> {
    parse_until(body, max, &AtomicBool::new(false))
}

/// [`parse`], giving up as soon as it can once `stop` is set: for a
/// caller that no longer wants the answer.
pub fn parse_until(body: &[u8], max: usize, stop: &AtomicBool) -> Result<Sitemap, String> {
    let data = if body.starts_with(&[0x1f, 0x8b]) {
        gunzip(body, max)?
    } else if body.len() > max {
        return Err(format!(
            "the sitemap is larger than the limit of {max} bytes"
        ));
    } else {
        body.to_vec()
    };
    let text = String::from_utf8_lossy(&data);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if text.starts_with('<') {
        xml(text, stop)
    } else {
        plain(text)
    }
}

fn gunzip(body: &[u8], max: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("the sitemap isn't valid gzip: {e}"))?;
    if out.len() > max {
        return Err(format!(
            "the sitemap unzips to more than the limit of {max} bytes"
        ));
    }
    Ok(out)
}

/// One http(s) URL per line; anything else is skipped.
fn plain(text: &str) -> Result<Sitemap, String> {
    let entries: Vec<Entry> = text
        .lines()
        .map(str::trim)
        .filter(|l| url::Url::parse(l).is_ok_and(|u| matches!(u.scheme(), "http" | "https")))
        .map(|l| Entry {
            loc: l.to_string(),
            ..Entry::default()
        })
        .collect();
    if entries.is_empty() {
        return Err("not a sitemap: neither XML nor a list of URLs".into());
    }
    Ok(Sitemap {
        kind: Kind::Urlset,
        entries,
        repaired: None,
    })
}

/// How long reading one sitemap may take.
pub const BUDGET: Duration = Duration::from_secs(10);

/// Read in one pass with netweir-dom's XML reader, building no tree, so a
/// sitemap of hundreds of thousands of URLs costs little more memory than
/// its entries. Sitemaps in the wild are often a little broken, so it's
/// read in recover mode: what recovery repaired is in `Sitemap::repaired`.
/// Names are matched by their local part, in any case and any namespace,
/// as sitemaps vary in both.
fn xml(text: &str, stop: &AtomicBool) -> Result<Sitemap, String> {
    let options = XmlOptions {
        recover: true,
        budget: BUDGET,
    };
    let mut walk = Walk {
        stop: Some(stop),
        ..Walk::default()
    };
    let repairs = walk_xml(text, options, &mut walk).map_err(|e| {
        if stop.load(Ordering::Relaxed) {
            "the sitemap was given up on".to_string()
        } else {
            format!("the sitemap isn't valid XML: {e}")
        }
    })?;
    let kind = walk
        .kind
        .ok_or("not a sitemap: no <urlset> or <sitemapindex>")?;
    Ok(Sitemap {
        kind,
        entries: walk.entries,
        repaired: (!repairs.is_empty()).then(|| repairs.summary()),
    })
}

/// What a sitemap's events add up to.
#[derive(Default)]
struct Walk<'a> {
    /// Set when the caller has gone.
    stop: Option<&'a AtomicBool>,
    /// Elements open, the root counting 1.
    depth: usize,
    /// The root's kind; None until it's read, and for any other root.
    kind: Option<Kind>,
    entry: Option<Entry>,
    /// The field of the entry being read, lowercased, and its text.
    field: Option<String>,
    text: String,
    entries: Vec<Entry>,
}

fn local(qname: &str) -> String {
    qname
        .split_once(':')
        .map_or(qname, |(_, l)| l)
        .to_ascii_lowercase()
}

impl XmlVisitor for Walk<'_> {
    fn stopped(&self) -> bool {
        self.stop.is_some_and(|s| s.load(Ordering::Relaxed))
    }

    fn start(&mut self, qname: &str, attrs: &[(String, String)]) {
        let name = local(qname);
        match self.depth {
            0 => {
                self.kind = match name.as_str() {
                    "urlset" => Some(Kind::Urlset),
                    "sitemapindex" => Some(Kind::Index),
                    _ => None,
                }
            }
            1 if self.kind.is_some() && (name == "url" || name == "sitemap") => {
                self.entry = Some(Entry::default());
            }
            2 if self.entry.is_some() => {
                let attr = |wanted: &str| {
                    attrs
                        .iter()
                        .find(|(k, _)| local(k) == wanted)
                        .map(|(_, v)| v.as_str())
                };
                // <xhtml:link rel="alternate" href="...">: the page in
                // another language.
                if name == "link"
                    && attr("rel").is_some_and(|r| r.eq_ignore_ascii_case("alternate"))
                    && let (Some(href), Some(entry)) = (attr("href"), self.entry.as_mut())
                {
                    entry.alternates.push(href.trim().to_string());
                }
                self.field = Some(name);
                self.text.clear();
            }
            _ => {}
        }
        self.depth += 1;
    }

    fn text(&mut self, text: &str) {
        if self.field.is_some() && self.depth == 3 {
            self.text.push_str(text);
        }
    }

    fn end(&mut self, _qname: &str) {
        self.depth -= 1;
        match self.depth {
            2 => {
                if let (Some(field), Some(entry)) = (self.field.take(), self.entry.as_mut()) {
                    let value = self.text.trim().to_string();
                    match field.as_str() {
                        "loc" => entry.loc = value,
                        "lastmod" => entry.lastmod = Some(value),
                        "changefreq" => entry.changefreq = Some(value),
                        "priority" => entry.priority = value.parse().ok(),
                        _ => {}
                    }
                }
            }
            1 => {
                if let Some(entry) = self.entry.take().filter(|e| !e.loc.is_empty()) {
                    self.entries.push(entry);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 1 << 20;

    #[test]
    fn a_sitemap_no_one_waits_for_is_given_up_on() {
        let rows: String = (0..200_000)
            .map(|i| format!("<url><loc>https://e/{i}</loc></url>"))
            .collect();
        let xml = format!("<urlset>{rows}</urlset>");
        let stop = AtomicBool::new(false);
        assert_eq!(
            parse_until(xml.as_bytes(), 1 << 30, &stop)
                .unwrap()
                .entries
                .len(),
            200_000
        );
        // Once it is set, reading stops within a few lines.
        stop.store(true, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let err = parse_until(xml.as_bytes(), 1 << 30, &stop).unwrap_err();
        assert_eq!(err, "the sitemap was given up on");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_urlset_gives_each_page_with_what_it_says_about_it() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
        xmlns:xhtml="http://www.w3.org/1999/xhtml"
        xmlns:image="http://www.google.com/schemas/sitemap-image/1.1">
  <url>
    <loc> https://example.com/a?x=1&amp;y=2 </loc>
    <lastmod>2026-10-01</lastmod>
    <changefreq>daily</changefreq>
    <priority>0.8</priority>
    <xhtml:link rel="alternate" hreflang="de" href="https://example.com/de/a"/>
    <image:image><image:loc>https://cdn.example.com/a.jpg</image:loc></image:image>
  </url>
  <url><loc><![CDATA[https://example.com/b]]></loc></url>
</urlset>"#;
        let map = parse(xml, MAX).unwrap();
        assert_eq!(map.kind, Kind::Urlset);
        assert_eq!(
            map.entries,
            [
                Entry {
                    loc: "https://example.com/a?x=1&y=2".into(),
                    lastmod: Some("2026-10-01".into()),
                    changefreq: Some("daily".into()),
                    priority: Some(0.8),
                    alternates: vec!["https://example.com/de/a".into()],
                },
                Entry {
                    loc: "https://example.com/b".into(),
                    ..Entry::default()
                },
            ]
        );
    }

    #[test]
    fn an_index_lists_more_sitemaps() {
        let xml = br#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://example.com/s1.xml.gz</loc><lastmod>2026-09-30</lastmod></sitemap>
  <sitemap><loc>https://example.com/s2.xml</loc></sitemap>
</sitemapindex>"#;
        let map = parse(xml, MAX).unwrap();
        assert_eq!(map.kind, Kind::Index);
        let locs: Vec<&str> = map.entries.iter().map(|e| e.loc.as_str()).collect();
        assert_eq!(
            locs,
            [
                "https://example.com/s1.xml.gz",
                "https://example.com/s2.xml"
            ]
        );
        assert_eq!(map.entries[0].lastmod.as_deref(), Some("2026-09-30"));
    }

    #[test]
    fn a_gzipped_sitemap_is_unzipped() {
        use std::io::Write;
        let xml = b"<urlset><url><loc>https://example.com/z</loc></url></urlset>";
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(xml).unwrap();
        let map = parse(&gz.finish().unwrap(), MAX).unwrap();
        assert_eq!(map.entries[0].loc, "https://example.com/z");
    }

    #[test]
    fn a_gzipped_sitemap_is_unzipped_no_further_than_the_limit() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&vec![b' '; 10_000_000]).unwrap();
        let err = parse(&gz.finish().unwrap(), 100_000).unwrap_err();
        assert!(err.contains("100000"), "{err}");
    }

    #[test]
    fn a_text_sitemap_is_one_url_per_line() {
        let text = b"https://example.com/1\n\n  https://example.com/2\r\nnot a url\n";
        let map = parse(text, MAX).unwrap();
        assert_eq!(map.kind, Kind::Urlset);
        let locs: Vec<&str> = map.entries.iter().map(|e| e.loc.as_str()).collect();
        assert_eq!(locs, ["https://example.com/1", "https://example.com/2"]);
    }

    #[test]
    fn entities_a_document_declares_are_never_expanded() {
        // The "billion laughs": each entity ten of the one before.
        let mut xml = String::from("<?xml version=\"1.0\"?><!DOCTYPE urlset [<!ENTITY a0 \"ha\">");
        for i in 1..10 {
            let prev = format!("&a{};", i - 1).repeat(10);
            xml.push_str(&format!("<!ENTITY a{i} \"{prev}\">"));
        }
        xml.push_str("]><urlset><url><loc>https://example.com/&a9;</loc></url></urlset>");
        let result = parse(xml.as_bytes(), MAX);
        // Refused, or kept unexpanded; never a billion "ha"s.
        if let Ok(map) = result {
            assert!(map.entries.iter().all(|e| e.loc.len() < 100), "{map:?}");
        }
    }

    #[test]
    fn names_are_matched_in_any_case_and_namespace() {
        let xml = br#"<?xml version="1.0"?>
<!DOCTYPE s [<!ENTITY host "https://example.com">]>
<s:SitemapIndex xmlns:s="http://www.google.com/schemas/sitemap/0.84">
  <s:Sitemap><s:Loc>&host;/one.xml</s:Loc><s:LastMod>2026-01-01</s:LastMod></s:Sitemap>
  <!-- a comment between entries -->
  <s:Sitemap><s:Loc>&host;/two.xml</s:Loc></s:Sitemap>
</s:SitemapIndex>"#;
        let map = parse(xml, MAX).unwrap();
        assert_eq!(map.kind, Kind::Index);
        let locs: Vec<&str> = map.entries.iter().map(|e| e.loc.as_str()).collect();
        assert_eq!(
            locs,
            ["https://example.com/one.xml", "https://example.com/two.xml"]
        );
        assert_eq!(map.entries[0].lastmod.as_deref(), Some("2026-01-01"));
    }

    #[test]
    fn an_empty_urlset_has_no_entries() {
        let map = parse(
            b"<urlset xmlns='http://www.sitemaps.org/schemas/sitemap/0.9'/>",
            MAX,
        )
        .unwrap();
        assert_eq!(map.kind, Kind::Urlset);
        assert!(map.entries.is_empty());
    }

    #[test]
    fn external_entities_in_a_sitemap_are_never_fetched() {
        let xml = br#"<!DOCTYPE u [<!ENTITY x SYSTEM "file:///etc/passwd">]><urlset><url><loc>&x;</loc></url></urlset>"#;
        let err = parse(xml, MAX).unwrap_err();
        assert!(err.contains("never fetched"), "{err}");
    }

    #[test]
    fn a_sitemap_cut_off_part_way_gives_what_it_has_and_says_so() {
        let map = parse(
            b"<urlset><url><loc>https://example.com/a</loc></url><url><loc>",
            MAX,
        )
        .unwrap();
        let locs: Vec<&str> = map.entries.iter().map(|e| e.loc.as_str()).collect();
        assert_eq!(locs, ["https://example.com/a"]);
        let repaired = map.repaired.unwrap();
        assert!(
            repaired.contains("ends inside <urlset><url><loc>; closed"),
            "{repaired}"
        );
    }

    #[test]
    fn the_broken_sitemaps_of_the_wild_are_read_and_the_repairs_named() {
        let urlset = |decl: &str, x: &str, y: &str| {
            format!(
                "<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\"{decl}><url><loc>https://e/a{x}</loc>{y}</url><url><loc>https://e/b</loc></url></urlset>"
            )
        };
        for (xml, a, alternates, says) in [
            (
                urlset(
                    "",
                    "",
                    "<image:image><image:loc>https://e/i.jpg</image:loc></image:image>",
                ),
                "https://e/a",
                vec![],
                "prefix image of <image:image> isn't declared",
            ),
            (
                urlset(
                    "",
                    "",
                    r#"<xhtml:link rel="alternate" hreflang="de" href="https://e/de"/>"#,
                ),
                "https://e/a",
                vec!["https://e/de"],
                "prefix xhtml of <xhtml:link> isn't declared",
            ),
            (
                urlset("", "&nbsp;", ""),
                "https://e/a",
                vec![],
                "&nbsp; isn't declared",
            ),
            (
                urlset("", "?a=1&b=2", ""),
                "https://e/a?a=1&b=2",
                vec![],
                "starts no reference",
            ),
            (urlset("", "\u{b}", ""), "https://e/a", vec![], "U+000B"),
            (
                urlset("", "", "") + "\n<!-- generated --> <b>PHP notice</b>",
                "https://e/a",
                vec![],
                "after the root element; the rest is ignored",
            ),
            (
                urlset(r#" a="1" a="2""#, "", ""),
                "https://e/a",
                vec![],
                "twice",
            ),
        ] {
            let map = parse(xml.as_bytes(), MAX).unwrap_or_else(|e| panic!("{xml}: {e}"));
            let locs: Vec<&str> = map.entries.iter().map(|e| e.loc.as_str()).collect();
            assert_eq!(locs, [a, "https://e/b"], "{xml}");
            assert_eq!(map.entries[0].alternates, alternates, "{xml}");
            let repaired = map.repaired.unwrap_or_default();
            assert!(repaired.contains(says), "{xml}: {repaired}");
        }
        // A sound one needs no repairs, with or without whitespace before
        // its declaration.
        let sound = format!("\n\n<?xml version='1.0'?>{}", urlset("", "", ""));
        assert_eq!(parse(sound.as_bytes(), MAX).unwrap().repaired, None);
    }

    #[test]
    fn neither_xml_nor_urls_is_an_error() {
        assert!(parse(b"<html><body>Not found</body></html>", MAX).is_err());
        assert!(parse(b"just some words", MAX).is_err());
    }
}
