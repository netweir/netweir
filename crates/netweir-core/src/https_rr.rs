//! HTTPS (type 65) DNS records the way Firefox 160 asks for them, reads
//! them and picks one, with TRR off: through the operating system's
//! resolver, with the call Firefox makes on that system
//! (netwerk/dns/PlatformDNSUnix.cpp, PlatformDNSMac.cpp,
//! PlatformDNSWin.cpp), so the DNS traffic is Firefox's; then
//! GetAddrInfo.cpp, DNSPacket.cpp and HTTPSSVC.cpp line for line,
//! including where they do something odd.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;
use std::time::Duration;

/// One record as Firefox's SVCB holds it. `target` is never "." for a
/// ServiceMode record: the owner name stands in for it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Svcb {
    pub priority: u16,
    pub target: String,
    pub alpn: Vec<String>,
    pub no_default_alpn: bool,
    pub port: Option<u16>,
    pub ech: Option<Vec<u8>>,
    pub ipv4hint: Vec<Ipv4Addr>,
    pub ipv6hint: Vec<Ipv6Addr>,
}

/// A lookup's records, in Firefox's order, and how long they last.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Records {
    pub records: Vec<Svcb>,
    pub ttl: Duration,
}

/// What a transaction connects to from a record
/// (nsHttpConnectionInfo::CloneAndAdoptHTTPSSVCRecord). `alpn` is the
/// token the connection offers alone: "h3", "h2" (only with
/// no-default-alpn), "http/1.1", or "" for TLS's usual h2 and http/1.1.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Route {
    pub host: String,
    pub port: u16,
    pub alpn: String,
    pub ech: Option<Vec<u8>>,
}

/// Test seam: answers by query name, as raw DNS response packets (what
/// res_nquery returns), decoded by the Linux path's parser whatever the
/// OS. When set, no OS query is made and the OS's hosts file and search
/// domains aren't consulted.
pub(crate) type Seam = HashMap<String, Vec<u8>>;

/// Queries `host`'s HTTPS records for an origin on `port`, as Firefox does
/// on this OS. None: no query on this OS or for this host, or it failed.
pub(crate) async fn query(host: &str, port: u16, seam: Option<&Seam>) -> Option<Records> {
    let qname = qname(host, port)?;
    let seam = seam.cloned();
    tokio::task::spawn_blocking(move || {
        if seam.is_none() && (!os::enabled() || os_excluded(&qname)) {
            return None;
        }
        resolve(&qname, seam.as_ref())
    })
    .await
    .ok()
    .flatten()
}

/// The canonical name of `host` from an address lookup, as Firefox's
/// HTTPSRecordResolver asks for one (RESOLVE_CANONICAL_NAME, getaddrinfo
/// with AI_CANONNAME) alongside the HTTPS query, for
/// CheckRecordIsUsableWithCname. None if the lookup fails, which skips
/// that check, as in Firefox.
// Known limit: Windows gives None, so the check is skipped there.
pub(crate) async fn canonical_name(host: &str) -> Option<String> {
    let host = std::ffi::CString::new(host).ok()?;
    tokio::task::spawn_blocking(move || os::canonical(&host))
        .await
        .ok()
        .flatten()
}

/// The name Firefox queries for `host` on `port`, or None when it queries
/// nothing (nsHostResolver::ResolveHost).
fn qname(host: &str, port: u16) -> Option<String> {
    // net_IsValidDNSHost; what it lets through beyond these is IPv6
    // literals, which are skipped next anyway.
    let valid = host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-$+_".contains(&b));
    if host.is_empty() || !valid || host.parse::<IpAddr>().is_ok() {
        return None;
    }
    // network.dns.port_prefixed_qname_https_rr
    let qname = if port == 443 {
        host.to_string()
    } else {
        format!("_{port}._https.{host}")
    };
    // network.trr.builtin-excluded-domains; excluded-domains is empty.
    (!excluded(&qname, |d| d == "localhost" || d == "local")).then_some(qname)
}

/// Whether `name` or a domain it is under is one `listed`
/// (TRRService::IsExcludedFromTRR_unlocked).
fn excluded(name: &str, listed: impl Fn(&str) -> bool) -> bool {
    let mut dot = 0;
    while dot < name.len() {
        if listed(&name[dot..]) {
            return true;
        }
        match name[dot + 1..].find('.') {
            Some(at) => dot += at + 2,
            None => break,
        }
    }
    false
}

/// The exclusions Firefox reads from the system: the names in the hosts
/// file (TRRService::ReadEtcHostsFile) and the DNS search domains
/// (TRRService::RebuildSuffixList, from the network link service). Read
/// once, as Firefox reads them at startup.
fn os_excluded(qname: &str) -> bool {
    static LISTS: OnceLock<(HashSet<String>, Vec<String>)> = OnceLock::new();
    let (hosts, suffixes) = LISTS.get_or_init(|| {
        let hosts = os::hosts_path()
            .and_then(|p| std::fs::File::open(p).ok())
            .map(|f| hosts_names(std::io::BufReader::new(f)))
            .unwrap_or_default();
        (hosts, os::search_domains())
    });
    excluded(qname, |d| {
        hosts.contains(d) || suffixes.iter().any(|s| s == d)
    })
}

/// The host names in a hosts file (rust_parse_etc_hosts in
/// netwerk/base/rust-helper).
fn hosts_names(file: impl BufRead) -> HashSet<String> {
    const INVALID: [char; 14] = [
        '\0', '\t', '\n', '\r', ' ', '#', '%', '/', ':', '?', '@', '[', '\\', ']',
    ];
    let mut names = HashSet::new();
    for line in file.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(_) => break,
        };
        let mut words = line.split('#').next().unwrap_or("").split_whitespace();
        words.next();
        names.extend(
            words
                .filter(|h| h.parse::<Ipv4Addr>().is_err() && !h.contains(&INVALID[..]))
                .map(String::from),
        );
    }
    names
}

/// How a lookup failed: NS_ERROR_UNKNOWN_HOST, after which an alias chain
/// can still end in its target, or anything else, which ends it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fail {
    UnknownHost,
    Bad,
}

type Res<T> = Result<T, Fail>;

/// What one platform lookup gives (ResolveHTTPSRecordImpl): records, or a
/// name to look up next (HTTPSAliasTarget).
#[derive(Debug, PartialEq)]
enum Lookup {
    Records(Vec<Svcb>),
    Alias { name: String, from_alias_mode: bool },
}

/// Follows AliasMode targets across lookups (GetAddrInfo.cpp::
/// ResolveHTTPSRecord).
fn resolve(qname: &str, seam: Option<&Seam>) -> Option<Records> {
    // nsHostResolver::ThreadFunc starts the TTL here, once per chain.
    let mut ttl = u32::MAX;
    let mut host = qname.to_string();
    let mut target: Option<String> = None;
    for _ in 0..8 {
        let found = match seam {
            Some(seam) => match seam.get(&host) {
                // DNSPacket::FillBuffer refuses more than MAX_SIZE.
                Some(p) if p.len() <= MAX_SIZE => parse_packet(p, &host, &mut ttl),
                _ => Err(Fail::UnknownHost),
            },
            None => os::lookup(&host, &mut ttl),
        };
        let alias = match found {
            Ok(Lookup::Records(records)) => return Some(records_with(records, ttl)),
            Ok(Lookup::Alias {
                name,
                from_alias_mode,
            }) => Some((name, from_alias_mode)),
            Err(Fail::UnknownHost) => None,
            Err(Fail::Bad) => return None,
        };
        if let Some((name, from_alias_mode)) = alias
            && !name.eq_ignore_ascii_case(&host)
            && (from_alias_mode || target.is_some())
        {
            target = Some(name.clone());
            host = name;
            continue;
        }
        // RFC 9460: an alias whose target has no HTTPS record still routes
        // to the target.
        return target.map(|target| {
            records_with(
                vec![Svcb {
                    target,
                    ..Svcb::default()
                }],
                ttl,
            )
        });
    }
    None
}

fn records_with(records: Vec<Svcb>, ttl: u32) -> Records {
    Records {
        records,
        ttl: Duration::from_secs(ttl.into()),
    }
}

/// DNSPacket::MAX_SIZE: the answer buffer res_nquery gets.
const MAX_SIZE: usize = 3200;

fn u16_at(b: &[u8], i: usize) -> Res<u16> {
    b.get(i..i + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .ok_or(Fail::Bad)
}

fn u32_at(b: &[u8], i: usize) -> Res<u32> {
    b.get(i..i + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or(Fail::Bad)
}

/// A name at `*index`, following compression pointers; `*index` moves past
/// it (DNSPacket::GetQname).
fn qname_at(b: &[u8], index: &mut usize) -> Res<String> {
    let mut name = Vec::new();
    let mut c = *index;
    let mut end = 0;
    // Counts pointers and labels alike: a valid name never loops this much.
    let mut budget = 128u32;
    loop {
        let len = *b.get(c).ok_or(Fail::Bad)?;
        if len & 0xc0 == 0xc0 {
            let low = *b.get(c + 1).ok_or(Fail::Bad)?;
            if end == 0 {
                end = c + 2;
            }
            c = usize::from(len & 0x3f) << 8 | usize::from(low);
        } else if len & 0xc0 != 0 {
            return Err(Fail::Bad);
        } else {
            c += 1;
            if len == 0 {
                break;
            }
            if !name.is_empty() {
                name.push(b'.');
            }
            name.extend_from_slice(b.get(c..c + usize::from(len)).ok_or(Fail::Bad)?);
            c += usize::from(len);
        }
        budget -= 1;
        if budget == 0 {
            return Err(Fail::Bad);
        }
    }
    *index = if end == 0 { c } else { end };
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// Moves `*index` past a name without reading it (DNSPacket::PassQName).
fn pass_qname(b: &[u8], index: &mut usize) -> Res<()> {
    loop {
        let len = *b.get(*index).ok_or(Fail::Bad)?;
        if len & 0xc0 == 0xc0 {
            if b.len() < *index + 2 {
                return Err(Fail::Bad);
            }
            *index += 2;
            return Ok(());
        }
        if len & 0xc0 != 0 || b.len() < *index + 1 + usize::from(len) {
            return Err(Fail::Bad);
        }
        *index += 1 + usize::from(len);
        if len == 0 {
            return Ok(());
        }
    }
}

/// An HTTPS record's RDATA at `at` (DNSPacket::ParseHTTPS). A TargetName
/// of "." in ServiceMode becomes `owner`; in AliasMode it stays empty.
fn parse_https(rdlen: u16, at: usize, b: &[u8], owner: &str) -> Res<Svcb> {
    let mut available = i32::from(rdlen);
    if available < 3 {
        return Err(Fail::Bad);
    }
    let mut i = at;
    let mut svcb = Svcb {
        priority: u16_at(b, i)?,
        ..Svcb::default()
    };
    i += 2;
    svcb.target = qname_at(b, &mut i)?;
    if svcb.target.is_empty() {
        if svcb.priority == 0 {
            return Ok(svcb);
        }
        svcb.target = owner.to_string();
    }
    available -= i32::try_from(i - at).map_err(|_| Fail::Bad)?;
    if available < 0 {
        return Err(Fail::Bad);
    }
    let mut last = -1;
    while available >= 4 {
        let key = u16_at(b, i)?;
        // RFC 9460 2.2: keys in strictly increasing order.
        if i32::from(key) <= last {
            return Err(Fail::Bad);
        }
        last = i32::from(key);
        let len = u16_at(b, i + 2)?;
        i += 4;
        available -= 4 + i32::from(len);
        if available < 0 {
            return Err(Fail::Bad);
        }
        let value = b.get(i..i + usize::from(len)).ok_or(Fail::Bad)?;
        svc_param(&mut svcb, key, value)?;
        i += usize::from(len);
    }
    Ok(svcb)
}

/// The SvcParamKeys Firefox knows (IsValidSvcParamKey): mandatory to
/// ipv6hint, and odoh.
fn known_key(key: u16) -> bool {
    key <= 6 || key == 32769
}

/// One SvcParam (DNSPacket::ParseSvcParam). Unknown keys are ignored, and
/// hints are kept whatever the address (aAllowRFC1918 is true natively).
fn svc_param(svcb: &mut Svcb, key: u16, value: &[u8]) -> Res<()> {
    let len = value.len();
    match key {
        0 if !len.is_multiple_of(2) => return Err(Fail::Bad),
        0 => {
            if value
                .chunks(2)
                .any(|k| !known_key(u16::from_be_bytes([k[0], k[1]])))
            {
                return Err(Fail::Bad);
            }
        }
        1 => {
            let mut rest = value;
            while let Some((&n, tail)) = rest.split_first() {
                let id = tail.get(..usize::from(n)).ok_or(Fail::Bad)?;
                svcb.alpn.push(String::from_utf8_lossy(id).into_owned());
                rest = &tail[usize::from(n)..];
            }
        }
        2 if len != 0 => return Err(Fail::Bad),
        2 => svcb.no_default_alpn = true,
        3 if len != 2 => return Err(Fail::Bad),
        3 => svcb.port = Some(u16::from_be_bytes([value[0], value[1]])),
        4 if !len.is_multiple_of(4) => return Err(Fail::Bad),
        4 => {
            svcb.ipv4hint = value
                .chunks(4)
                .map(|a| Ipv4Addr::new(a[0], a[1], a[2], a[3]))
                .collect()
        }
        5 => svcb.ech = Some(value.to_vec()),
        6 if !len.is_multiple_of(16) => return Err(Fail::Bad),
        6 => {
            svcb.ipv6hint = value
                .chunks(16)
                .map(|a| Ipv6Addr::from(<[u8; 16]>::try_from(a).unwrap_or_default()))
                .collect();
        }
        _ => {}
    }
    Ok(())
}

/// One reading of a response for `host` (DNSPacket::DecodeInternal for
/// type 65, from a native packet: the ID and RCODE go unchecked).
#[derive(Debug, Default)]
struct Decoded {
    records: Option<Vec<Svcb>>,
    cname: String,
    cname_is_alias: bool,
}

fn decode(b: &[u8], host: &str, ttl: &mut u32) -> Res<Decoded> {
    let need = |end: usize| {
        if b.len() < end {
            Err(Fail::Bad)
        } else {
            Ok(())
        }
    };
    need(12)?;
    let mut out = Decoded::default();
    let mut i = 12;
    for _ in 0..u16_at(b, 4)? {
        qname_at(b, &mut i)?;
        need(i + 4)?;
        i += 4;
    }
    let mut answers = u16_at(b, 6)?;
    while answers > 0 {
        let owner = qname_at(b, &mut i)?;
        need(i + 2)?;
        let kind = u16_at(b, i)?;
        need(i + 4)?;
        if u16_at(b, i + 2)? != 1 {
            return Err(Fail::Bad);
        }
        need(i + 8)?;
        let record_ttl = u32_at(b, i + 4)?;
        need(i + 10)?;
        let rdlen = u16_at(b, i + 8)?;
        i += 10;
        need(i + usize::from(rdlen))?;
        let matches = (owner.len() == host.len()
            || (host.len() == owner.len() + 1 && host.ends_with('.')))
            && host.as_bytes()[..owner.len()].eq_ignore_ascii_case(owner.as_bytes());
        if matches && kind == 5 && out.cname.is_empty() {
            let mut at = i;
            let cname = qname_at(b, &mut at)?;
            out.cname = cname.to_ascii_lowercase();
        } else if matches && kind == 65 {
            let svcb = parse_https(rdlen, i, b, &owner)?;
            if svcb.priority == 0 && svcb.target.is_empty() {
                // Firefox's `continue` here skips moving past the record
                // and counting it: the next pass reads its RDATA as a
                // record, which mostly ends the decode with an error.
                continue;
            }
            if out.cname.is_empty() && svcb.priority == 0 {
                out.cname = svcb.target.to_ascii_lowercase();
                out.cname_is_alias = true;
                // AliasMode wins over ServiceMode seen so far.
                out.records = None;
            } else {
                *ttl = record_ttl;
                out.records.get_or_insert_with(Vec::new).push(svcb);
            }
        }
        i += usize::from(rdlen);
        answers -= 1;
    }
    for _ in 0..u16_at(b, 8)? {
        pass_qname(b, &mut i)?;
        need(i + 10)?;
        i += 10 + usize::from(u16_at(b, i + 8)?);
        need(i)?;
    }
    for _ in 0..u16_at(b, 10)? {
        qname_at(b, &mut i)?;
        need(i + 10)?;
        i += 10 + usize::from(u16_at(b, i + 8)?);
        need(i)?;
    }
    if i != b.len() {
        return Err(Fail::Bad);
    }
    if out.cname.is_empty() && out.records.is_none() {
        return Err(Fail::UnknownHost);
    }
    if let Some(records) = &mut out.records {
        // SVCB::operator< with network.dns.echconfig.enabled: ECH first,
        // then priority. shortcut: Firefox's sort isn't stable, so ties
        // may come out in another order there.
        records.sort_by_key(|r| (r.ech.is_none(), r.priority));
    }
    Ok(out)
}

/// A response for `host`, following CNAMEs and AliasMode within it
/// (GetAddrInfo.cpp::ParseHTTPSRecord).
fn parse_packet(b: &[u8], host: &str, ttl: &mut u32) -> Res<Lookup> {
    let mut host = host.to_string();
    let mut alias: Option<(String, bool)> = None;
    for _ in 0..64 {
        match decode(b, &host, ttl) {
            Err(Fail::UnknownHost) if alias.is_some() => break,
            Err(e) => return Err(e),
            Ok(Decoded {
                records: Some(records),
                ..
            }) => return Ok(Lookup::Records(records)),
            Ok(d) => {
                let from = alias.is_some_and(|a| a.1) || d.cname_is_alias;
                host.clone_from(&d.cname);
                alias = Some((d.cname, from));
            }
        }
    }
    alias
        .map(|(name, from_alias_mode)| Lookup::Alias {
            name,
            from_alias_mode,
        })
        .ok_or(Fail::UnknownHost)
}

/// One record of a DnsQuery_A answer list: its owner, type, TTL, and the
/// raw RDATA of a type 65 record or the target of a CNAME.
#[cfg_attr(not(windows), allow(dead_code))]
struct WinRecord {
    name: Vec<u8>,
    kind: u16,
    ttl: u32,
    data: Vec<u8>,
}

/// Firefox's walk of DnsQuery_A's list (PlatformDNSWin.cpp::
/// ResolveHTTPSRecordImpl): owners compared exactly, records unsorted,
/// the TTL the least seen, 60 seconds if none.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_records(list: &[WinRecord], host: &str, ttl: &mut u32) -> Res<Lookup> {
    *ttl = u32::MAX;
    let mut host = host.to_string();
    let mut records: Option<Vec<Svcb>> = None;
    let mut alias: Option<(String, bool)> = None;
    let mut loops = 64;
    while loops > 0 && records.is_none() {
        loops -= 1;
        let mut cname = None;
        for r in list.iter().filter(|r| r.name == host.as_bytes()) {
            if r.kind == 65 {
                let len = u16::try_from(r.data.len()).map_err(|_| Fail::Bad)?;
                let svcb = parse_https(len, 0, &r.data, &host)?;
                if svcb.priority == 0 && svcb.target.is_empty() {
                    continue;
                }
                if svcb.priority == 0 {
                    cname = Some((svcb.target.to_ascii_lowercase(), true));
                    break;
                }
                records.get_or_insert_with(Vec::new).push(svcb);
                *ttl = (*ttl).min(r.ttl);
            } else if r.kind == 5 {
                let name = String::from_utf8_lossy(&r.data).to_ascii_lowercase();
                cname = Some((name, false));
                *ttl = (*ttl).min(r.ttl);
                break;
            }
        }
        match cname {
            Some((name, is_alias)) if records.is_none() && !name.is_empty() => {
                let from = alias.is_some_and(|a| a.1) || is_alias;
                host.clone_from(&name);
                alias = Some((name, from));
            }
            _ if records.is_none() => break,
            _ => {}
        }
    }
    // Firefox checks the count, not the result: records found on the last
    // pass count as a loop too.
    if loops == 0 {
        return Err(Fail::UnknownHost);
    }
    match records {
        Some(records) => {
            if *ttl == u32::MAX {
                *ttl = 60;
            }
            Ok(Lookup::Records(records))
        }
        None => alias
            .map(|(name, from_alias_mode)| Lookup::Alias {
                name,
                from_alias_mode,
            })
            .ok_or(Fail::UnknownHost),
    }
}

/// What DNSServiceQueryRecord's callback gathers (PlatformDNSMac.cpp::
/// DNSContext).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct MacAnswer {
    fail: Option<Fail>,
    records: Option<Vec<Svcb>>,
    alias: String,
    ttl: u32,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl MacAnswer {
    /// One callback (PlatformDNSMac.cpp::QueryCallback).
    fn record(&mut self, error: i32, fullname: &[u8], kind: u16, rdata: &[u8], ttl: u32) {
        if error != 0 {
            self.fail = Some(Fail::UnknownHost);
            return;
        }
        // kDNSServiceFlagsReturnIntermediates hands over CNAMEs too.
        if self.fail.is_some() || kind != 65 {
            return;
        }
        let Ok(len) = u16::try_from(rdata.len()) else {
            return;
        };
        if len == 0 {
            self.fail = Some(Fail::UnknownHost);
            return;
        }
        let owner = String::from_utf8_lossy(fullname.strip_suffix(b".").unwrap_or(fullname));
        match parse_https(len, 0, rdata, &owner) {
            Err(e) => self.fail = Some(e),
            // AliasMode "." means no service; it's dropped.
            Ok(svcb) if svcb.priority == 0 && svcb.target.is_empty() => {}
            Ok(svcb) if svcb.priority == 0 => self.alias = svcb.target.to_ascii_lowercase(),
            Ok(svcb) => {
                self.records.get_or_insert_with(Vec::new).push(svcb);
                self.ttl = self.ttl.min(ttl);
            }
        }
    }

    fn finish(self, ttl: &mut u32) -> Res<Lookup> {
        *ttl = self.ttl;
        if let Some(fail) = self.fail {
            return Err(fail);
        }
        match self.records {
            Some(records) => {
                if *ttl == u32::MAX {
                    *ttl = 60;
                }
                Ok(Lookup::Records(records))
            }
            None if !self.alias.is_empty() => Ok(Lookup::Alias {
                name: self.alias,
                from_alias_mode: true,
            }),
            None => Err(Fail::UnknownHost),
        }
    }
}

/// Picks the record a transaction connects to
/// (HTTPSRecordResolver::InvokeCallback: GetServiceModeRecordWithCname,
/// else GetAliasModeRecord). `cname` is the origin's canonical name from
/// its address lookup, which network.dns.https_rr.check_record_with_cname
/// requires a record's target to equal; `h3_excluded` says whether a
/// target is on nsHttpHandler's HTTP/3 excluded list.
pub(crate) fn choose(
    records: &Records,
    origin_host: &str,
    origin_port: u16,
    cname: Option<&str>,
    h3_excluded: &dyn Fn(&str) -> bool,
) -> Option<Route> {
    let host = qname_host(origin_host, origin_port);
    let picked = service_mode(&records.records, &host, cname, false, true, h3_excluded);
    match picked {
        Some((svcb, alpn)) => Some(route(svcb, alpn, origin_host, origin_port)),
        None => match records.records.as_slice() {
            [only] if only.priority == 0 && !only.target.is_empty() => {
                Some(route(only, None, origin_host, origin_port))
            }
            _ => None,
        },
    }
}

/// The host Firefox's record set is keyed by (DNSHTTPSSVCRecordBase::mHost):
/// the query name, port prefix and all.
fn qname_host(host: &str, port: u16) -> String {
    if port == 443 {
        host.to_string()
    } else {
        format!("_{port}._https.{host}")
    }
}

/// IsAlpnSupported's ranks, with HTTP/2 and HTTP/3 on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    No,
    H1,
    H2,
    H3,
}

fn rank(alpn: &str) -> Rank {
    match alpn {
        "h3" => Rank::H3,
        "h2" => Rank::H2,
        a if a.eq_ignore_ascii_case("http/1.1") => Rank::H1,
        _ => Rank::No,
    }
}

/// SVCB::GetPort: a port Firefox won't connect to reads as 0.
fn port_of(svcb: &Svcb) -> Option<u16> {
    svcb.port
        .map(|p| if crate::h3::banned_port(p) { 0 } else { p })
}

type Flat<'a> = (&'a Svcb, Option<(String, Rank)>);

/// One entry per record and ALPN, best ALPN first (FlattenRecords and
/// SVCB::GetAllAlpn), and how many are h3.
fn flatten<'a>(host: &str, records: &'a [Svcb]) -> (Vec<Flat<'a>>, usize) {
    let mut out = Vec::new();
    let mut h3 = 0;
    for r in records {
        // shortcut: GetAllAlpn's sort isn't stable either; equal ranks
        // keep the record's order here.
        let mut alpns: Vec<(String, Rank)> = r.alpn.iter().map(|a| (a.clone(), rank(a))).collect();
        alpns.sort_by_key(|a| std::cmp::Reverse(a.1));
        if alpns.is_empty() {
            out.push((r, None));
            continue;
        }
        // The default ALPN gets an entry of its own unless the record is
        // the origin itself without ECH, where plain fallback covers it.
        let h1_added = !r.no_default_alpn && (host != r.target || r.ech.is_some());
        if h1_added {
            alpns.push((String::new(), Rank::H1));
        }
        for (alpn, rank) in alpns {
            if rank == Rank::H3 {
                h3 += 1;
            }
            if rank == Rank::H2 && h1_added {
                continue;
            }
            // "" offers h2 and http/1.1 both; no-default-alpn rules out
            // http/1.1, so h2 then stays explicit.
            let alpn = if rank == Rank::H2 && !r.no_default_alpn {
                String::new()
            } else {
                alpn
            };
            out.push((r, Some((alpn, rank))));
        }
    }
    (out, h3)
}

/// DNSHTTPSSVCRecordBase::GetServiceModeRecordInternal, with no domain on
/// the failed-SVC list and ECH on for HTTP/3 as for TCP.
fn service_mode<'a>(
    records: &'a [Svcb],
    host: &str,
    cname: Option<&str>,
    no_h3: bool,
    check_excluded: bool,
    h3_excluded: &dyn Fn(&str) -> bool,
) -> Option<(&'a Svcb, Option<(String, Rank)>)> {
    let (flat, h3_count) = flatten(host, records);
    let mut selected = None;
    let (mut no_default, mut unmatched, mut h3_excluded_count) = (0, 0, 0);
    for (r, alpn) in &flat {
        if r.priority == 0 {
            return None;
        }
        if r.no_default_alpn {
            no_default += 1;
        }
        if port_of(r) == Some(0) {
            continue;
        }
        if cname.is_some_and(|c| !c.is_empty() && r.target != c) {
            unmatched += 1;
            continue;
        }
        if let Some((_, rank)) = alpn {
            let usable = match rank {
                Rank::No => false,
                Rank::H3 if check_excluded && h3_excluded(&r.target) => {
                    h3_excluded_count += 1;
                    false
                }
                Rank::H3 => !no_h3,
                _ => true,
            };
            if !usable {
                continue;
            }
        }
        if selected.is_none() {
            selected = Some((*r, alpn.clone()));
        }
    }
    if selected.is_none() {
        if no_default == flat.len() || unmatched == flat.len() {
            return None;
        }
        // Better a target on the excluded list than nothing.
        if h3_excluded_count > 0 && h3_excluded_count == h3_count && check_excluded {
            return service_mode(records, host, cname, no_h3, false, h3_excluded);
        }
    }
    selected
}

fn route(svcb: &Svcb, alpn: Option<(String, Rank)>, origin_host: &str, origin_port: u16) -> Route {
    Route {
        host: if svcb.target.is_empty() {
            origin_host.to_string()
        } else {
            svcb.target.clone()
        },
        port: port_of(svcb).unwrap_or(origin_port),
        alpn: alpn.map(|a| a.0).unwrap_or_default(),
        ech: svcb.ech.clone().filter(|e| !e.is_empty()),
    }
}

/// A name in DNS wire form, uncompressed.
fn wire_name(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in name.split('.').filter(|l| !l.is_empty()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// A response packet for `qname`, type 65, with these answers, authority
/// and additional records already in wire form.
fn response_packet(
    qname: &str,
    answers: &[Vec<u8>],
    authority: &[Vec<u8>],
    additional: &[Vec<u8>],
) -> Vec<u8> {
    let count = |v: &[Vec<u8>]| u16::try_from(v.len()).unwrap_or(u16::MAX).to_be_bytes();
    let mut p = vec![0, 0, 0x81, 0x80, 0, 1];
    p.extend(count(answers));
    p.extend(count(authority));
    p.extend(count(additional));
    p.extend(wire_name(qname));
    p.extend([0, 65, 0, 1]);
    for rr in answers.iter().chain(authority).chain(additional) {
        p.extend(rr);
    }
    p
}

/// A resource record: owner (wire form), type, class, TTL, RDATA.
fn wire_record(owner: &[u8], kind: u16, class: u16, ttl: u32, rdata: &[u8]) -> Vec<u8> {
    let mut r = owner.to_vec();
    r.extend(kind.to_be_bytes());
    r.extend(class.to_be_bytes());
    r.extend(ttl.to_be_bytes());
    r.extend(u16::try_from(rdata.len()).unwrap_or(u16::MAX).to_be_bytes());
    r.extend(rdata);
    r
}

/// HTTPS RDATA: priority, target ("." or "" for the root), SvcParams as
/// given, in the order given.
fn https_rdata(priority: u16, target: &str, params: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut d = priority.to_be_bytes().to_vec();
    d.extend(wire_name(target));
    for (key, value) in params {
        d.extend(key.to_be_bytes());
        d.extend(u16::try_from(value.len()).unwrap_or(u16::MAX).to_be_bytes());
        d.extend(value);
    }
    d
}

/// For tests: a DNS response to an HTTPS query for `qname` answering with
/// these records (priority, target, SvcParams as key and value), each
/// owned by `qname` (compressed to the question) with this TTL.
#[allow(clippy::type_complexity)]
#[doc(hidden)]
pub fn https_answer(qname: &str, records: &[(u16, &str, &[(u16, Vec<u8>)])], ttl: u32) -> Vec<u8> {
    let answers: Vec<Vec<u8>> = records
        .iter()
        .map(|(priority, target, params)| {
            wire_record(
                &[0xc0, 12],
                65,
                1,
                ttl,
                &https_rdata(*priority, target, params),
            )
        })
        .collect();
    response_packet(qname, &answers, &[], &[])
}

/// The system calls, one module per family of OS Firefox has a native
/// HTTPS lookup on; elsewhere Firefox makes none (MOZ_NO_HTTPS_IMPL).
#[allow(unsafe_code)]
mod os {
    /// getaddrinfo's canonical name for `host`, asked as NSPR's
    /// PR_GetAddrInfoByName asks for Firefox (GetAddrInfo.cpp::_GetAddrInfo
    /// _Portable: PR_AI_ADDRCONFIG, and the canonical name).
    #[cfg(unix)]
    pub(super) fn canonical(host: &std::ffi::CStr) -> Option<String> {
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_family = libc::AF_UNSPEC;
        hints.ai_socktype = libc::SOCK_STREAM;
        hints.ai_flags = libc::AI_ADDRCONFIG | libc::AI_CANONNAME;
        let mut list = std::ptr::null_mut();
        // SAFETY: valid C strings and hints; the list is freed below.
        let failed =
            unsafe { libc::getaddrinfo(host.as_ptr(), std::ptr::null(), &hints, &mut list) };
        if failed != 0 {
            return None;
        }
        let first = std::ptr::NonNull::new(list)?;
        // SAFETY: getaddrinfo succeeded and gave a list, so `first` points
        // at its first entry until freeaddrinfo; the name is copied first.
        unsafe {
            let canonname = first.as_ref().ai_canonname;
            let name = (!canonname.is_null()).then(|| {
                std::ffi::CStr::from_ptr(canonname)
                    .to_string_lossy()
                    .into_owned()
            });
            libc::freeaddrinfo(first.as_ptr());
            name
        }
    }

    #[cfg(not(unix))]
    pub(super) fn canonical(_: &std::ffi::CStr) -> Option<String> {
        None
    }

    #[cfg(any(
        all(target_os = "linux", target_env = "gnu"),
        target_os = "freebsd",
        target_os = "macos"
    ))]
    pub(super) use unix::*;
    #[cfg(windows)]
    pub(super) use windows::*;

    #[cfg(not(any(
        all(target_os = "linux", target_env = "gnu"),
        target_os = "freebsd",
        target_os = "macos",
        windows
    )))]
    mod none {
        pub(in super::super) fn enabled() -> bool {
            false
        }
        pub(in super::super) fn lookup(
            _: &str,
            _: &mut u32,
        ) -> super::super::Res<super::super::Lookup> {
            Err(super::super::Fail::UnknownHost)
        }
        pub(in super::super) fn hosts_path() -> Option<std::path::PathBuf> {
            None
        }
        pub(in super::super) fn search_domains() -> Vec<String> {
            Vec::new()
        }
    }
    #[cfg(not(any(
        all(target_os = "linux", target_env = "gnu"),
        target_os = "freebsd",
        target_os = "macos",
        windows
    )))]
    pub(super) use none::*;

    /// resolv.h's state, which Rust's libc doesn't describe: only the
    /// search list is read, at the offset glibc, FreeBSD and macOS share.
    #[cfg(any(
        all(target_os = "linux", target_env = "gnu"),
        target_os = "freebsd",
        target_os = "macos"
    ))]
    mod unix {
        use std::ffi::{CStr, c_char, c_int};

        /// Larger than any libc's `struct __res_state` (568 bytes on glibc).
        #[repr(C, align(8))]
        struct ResState([u8; 2048]);

        const DNSRCH: usize = if cfg!(target_pointer_width = "64") {
            72
        } else {
            68
        };
        const MAXDNSRCH: usize = 6;

        /// The resolver's functions, found when first needed: glibc 2.34
        /// and later export `res_nquery` and the rest from libc, keeping
        /// `__res_nquery` only for old binaries, which nothing can link
        /// to; older glibc and FreeBSD have only `__res_nquery`, in
        /// libresolv or libc. A wheel built on one runs on the other.
        #[cfg(not(target_os = "macos"))]
        type Ninit = unsafe extern "C" fn(*mut ResState) -> c_int;
        #[cfg(not(target_os = "macos"))]
        type Nclose = unsafe extern "C" fn(*mut ResState);
        #[cfg(not(target_os = "macos"))]
        type Nquery = unsafe extern "C" fn(
            *mut ResState,
            *const c_char,
            c_int,
            c_int,
            *mut u8,
            c_int,
        ) -> c_int;

        #[cfg(not(target_os = "macos"))]
        struct Resolv {
            ninit: Ninit,
            nclose: Nclose,
            nquery: Nquery,
        }

        #[cfg(not(target_os = "macos"))]
        fn resolv() -> Option<&'static Resolv> {
            static RESOLV: std::sync::OnceLock<Option<Resolv>> = std::sync::OnceLock::new();
            RESOLV
                .get_or_init(|| {
                    // SAFETY: dlopen and dlsym with NUL-terminated names; the
                    // library stays loaded for the process's life.
                    unsafe {
                        let library = libc::dlopen(c"libresolv.so.2".as_ptr(), libc::RTLD_NOW);
                        let find = |name: &CStr| {
                            let old = format!("__{}", name.to_str().ok()?);
                            let old = std::ffi::CString::new(old).ok()?;
                            // A library that didn't load is null, which
                            // dlsym reads as the default search again.
                            [libc::RTLD_DEFAULT, library]
                                .into_iter()
                                .flat_map(|h| {
                                    [name.as_ptr(), old.as_ptr()].map(|n| libc::dlsym(h, n))
                                })
                                .find(|p| !p.is_null())
                        };
                        Some(Resolv {
                            ninit: std::mem::transmute::<*mut libc::c_void, Ninit>(find(
                                c"res_ninit",
                            )?),
                            nclose: std::mem::transmute::<*mut libc::c_void, Nclose>(find(
                                c"res_nclose",
                            )?),
                            nquery: std::mem::transmute::<*mut libc::c_void, Nquery>(find(
                                c"res_nquery",
                            )?),
                        })
                    }
                })
                .as_ref()
        }

        #[cfg(not(target_os = "macos"))]
        unsafe fn res_ninit(state: *mut ResState) -> c_int {
            // SAFETY: as the caller's, with the function glibc gives.
            resolv().map_or(-1, |r| unsafe { (r.ninit)(state) })
        }

        #[cfg(not(target_os = "macos"))]
        unsafe fn res_nclose(state: *mut ResState) {
            if let Some(r) = resolv() {
                // SAFETY: as the caller's.
                unsafe { (r.nclose)(state) }
            }
        }

        #[cfg(not(target_os = "macos"))]
        unsafe fn res_nquery(
            state: *mut ResState,
            name: *const c_char,
            class: c_int,
            kind: c_int,
            answer: *mut u8,
            len: c_int,
        ) -> c_int {
            // SAFETY: as the caller's.
            resolv().map_or(-1, |r| unsafe {
                (r.nquery)(state, name, class, kind, answer, len)
            })
        }

        #[cfg(target_os = "macos")]
        #[link(name = "resolv")]
        unsafe extern "C" {
            #[link_name = "res_9_ninit"]
            fn res_ninit(state: *mut ResState) -> c_int;
            #[link_name = "res_9_ndestroy"]
            fn res_nclose(state: *mut ResState);
        }

        impl ResState {
            fn init() -> Option<Box<ResState>> {
                // Firefox serialises res_ninit (PlatformDNSUnix.cpp sMutex).
                static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
                let mut state = Box::new(ResState([0; 2048]));
                let _lock = LOCK.lock();
                // SAFETY: a zeroed state, larger than the C struct, as
                // Firefox memsets its own before the call.
                (unsafe { res_ninit(&mut *state) } == 0).then_some(state)
            }
        }

        impl Drop for ResState {
            fn drop(&mut self) {
                // SAFETY: only states res_ninit initialised are kept.
                unsafe { res_nclose(self) }
            }
        }

        pub(in super::super) fn enabled() -> bool {
            true
        }

        pub(in super::super) fn hosts_path() -> Option<std::path::PathBuf> {
            Some("/etc/hosts".into())
        }

        /// res_ninit's search list, as the network link service reads it
        /// (NetlinkService::ExtractDNSProperties, nsNetworkLinkService.mm::
        /// GetDnsSuffixListInternal). FreeBSD has no link service.
        pub(in super::super) fn search_domains() -> Vec<String> {
            if cfg!(target_os = "freebsd") {
                return Vec::new();
            }
            let Some(state) = ResState::init() else {
                return Vec::new();
            };
            let base = state.0.as_ptr();
            (0..MAXDNSRCH)
                .map(|i| {
                    // SAFETY: dnsrch is an array of MAXDNSRCH + 1 pointers
                    // at DNSRCH, inside the state.
                    unsafe {
                        base.add(DNSRCH)
                            .cast::<*const c_char>()
                            .add(i)
                            .read_unaligned()
                    }
                })
                .take_while(|p| !p.is_null())
                // SAFETY: res_ninit points them at NUL-terminated names in
                // the state, which outlives this.
                .map(|p| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
                .collect()
        }

        /// PlatformDNSUnix.cpp::ResolveHTTPSRecordImpl: res_nquery on this
        /// thread's resolver state.
        #[cfg(not(target_os = "macos"))]
        pub(in super::super) fn lookup(
            host: &str,
            ttl: &mut u32,
        ) -> super::super::Res<super::super::Lookup> {
            use std::cell::RefCell;
            use std::ffi::CString;

            use super::super::Fail;
            thread_local! {
                static STATE: RefCell<Option<Box<ResState>>> = const { RefCell::new(None) };
            }
            let name = CString::new(host).map_err(|_| Fail::UnknownHost)?;
            let mut answer = [0u8; super::super::MAX_SIZE];
            let len = STATE.with_borrow_mut(|state| {
                if state.is_none() {
                    *state = ResState::init();
                }
                let state = state.as_mut()?;
                // SAFETY: an initialised state, a C string, and a buffer
                // of the length given.
                Some(unsafe {
                    res_nquery(
                        &mut **state,
                        name.as_ptr(),
                        1,
                        65,
                        answer.as_mut_ptr(),
                        answer.len() as c_int,
                    )
                })
            });
            let len = len
                .and_then(|l| usize::try_from(l).ok())
                .filter(|&l| l <= answer.len())
                .ok_or(Fail::UnknownHost)?;
            super::super::parse_packet(&answer[..len], host, ttl)
        }

        #[cfg(target_os = "macos")]
        pub(in super::super) use super::mac::lookup;
    }

    /// PlatformDNSMac.cpp::ResolveHTTPSRecordImpl, over dns_sd, which is in
    /// libSystem.
    #[cfg(target_os = "macos")]
    mod mac {
        use std::ffi::{CStr, CString, c_char, c_int, c_void};

        use super::super::{Fail, Lookup, MacAnswer, Res};

        type Ref = *mut c_void;
        type Callback = extern "C" fn(
            Ref,
            u32,
            u32,
            i32,
            *const c_char,
            u16,
            u16,
            u16,
            *const c_void,
            u32,
            *mut c_void,
        );

        const RETURN_INTERMEDIATES: u32 = 0x1000;
        /// network.dns.native_https_timeout_mac_msec
        const TIMEOUT_MS: c_int = 5000;

        unsafe extern "C" {
            fn DNSServiceQueryRecord(
                sd: *mut Ref,
                flags: u32,
                interface: u32,
                name: *const c_char,
                kind: u16,
                class: u16,
                callback: Callback,
                context: *mut c_void,
            ) -> i32;
            fn DNSServiceRefSockFD(sd: Ref) -> c_int;
            fn DNSServiceProcessResult(sd: Ref) -> i32;
            fn DNSServiceRefDeallocate(sd: Ref);
        }

        #[allow(clippy::too_many_arguments)]
        extern "C" fn callback(
            _: Ref,
            _flags: u32,
            _interface: u32,
            error: i32,
            fullname: *const c_char,
            kind: u16,
            _class: u16,
            rdlen: u16,
            rdata: *const c_void,
            ttl: u32,
            context: *mut c_void,
        ) {
            // SAFETY: dns_sd passes back the context lookup gave it, and a
            // name and RDATA valid for the call.
            let (answer, name, rdata) = unsafe {
                let name = if fullname.is_null() {
                    &[][..]
                } else {
                    CStr::from_ptr(fullname).to_bytes()
                };
                let rdata = if rdata.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(rdata.cast::<u8>(), rdlen.into())
                };
                (&mut *context.cast::<MacAnswer>(), name, rdata)
            };
            answer.record(error, name, kind, rdata, ttl);
        }

        pub(in super::super) fn lookup(host: &str, ttl: &mut u32) -> Res<Lookup> {
            let name = CString::new(host).map_err(|_| Fail::UnknownHost)?;
            let mut answer = MacAnswer {
                fail: None,
                records: None,
                alias: String::new(),
                ttl: *ttl,
            };
            let mut sd: Ref = std::ptr::null_mut();
            // SAFETY: `answer` outlives the query, deallocated below.
            let err = unsafe {
                DNSServiceQueryRecord(
                    &mut sd,
                    RETURN_INTERMEDIATES,
                    0,
                    name.as_ptr(),
                    65,
                    1,
                    callback,
                    (&raw mut answer).cast(),
                )
            };
            if err != 0 {
                return Err(Fail::UnknownHost);
            }
            // SAFETY: `sd` is a live query until DNSServiceRefDeallocate.
            unsafe {
                let mut fd = libc::pollfd {
                    fd: DNSServiceRefSockFD(sd),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // One reply only, as Firefox reads one: an NXDOMAIN never
                // calls back, hence the timeout.
                if libc::poll(&mut fd, 1, TIMEOUT_MS) > 0 && fd.revents & libc::POLLIN != 0 {
                    DNSServiceProcessResult(sd);
                }
                DNSServiceRefDeallocate(sd);
            }
            answer.finish(ttl)
        }
    }

    /// PlatformDNSWin.cpp, on Windows 11 and later only
    /// (nsHostResolver::Init: network.dns.native_https_query_win10 is false).
    #[cfg(windows)]
    mod windows {
        use std::ffi::CString;

        use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
        use windows_sys::Win32::NetworkManagement::Dns::{DNS_RECORDA, DnsFree, DnsQuery_A};
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
        };
        use windows_sys::Win32::System::Registry::{
            HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW,
        };
        use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryA, OSVERSIONINFOW};

        use super::super::{Fail, Lookup, Res, WinRecord, windows_records};

        /// mozilla::IsWin11OrLater: 10.0.22000. RtlGetVersion, as a
        /// process without Firefox's manifest gets told 6.2 by
        /// VerifyVersionInfoW.
        pub(in super::super) fn enabled() -> bool {
            static WIN11: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *WIN11.get_or_init(|| {
                // SAFETY: a zeroed OSVERSIONINFOW with its size set.
                let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
                info.dwOSVersionInfoSize = size_of::<OSVERSIONINFOW>() as u32;
                // SAFETY: as above.
                let ok = unsafe { RtlGetVersion(&mut info) } == 0;
                ok && (info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
                    >= (10, 0, 22000)
            })
        }

        /// DNSServiceBase::DoReadEtcHostsFile: CSIDL_SYSTEM's
        /// drivers\etc\hosts.
        pub(in super::super) fn hosts_path() -> Option<std::path::PathBuf> {
            let mut buf = [0u8; 261];
            // SAFETY: a buffer of the length given.
            let len = unsafe { GetSystemDirectoryA(buf.as_mut_ptr(), buf.len() as u32) } as usize;
            let dir = std::str::from_utf8(buf.get(..len).filter(|_| len > 0)?).ok()?;
            Some(format!("{dir}\\drivers\\etc\\hosts").into())
        }

        fn wide(p: *const u16) -> String {
            if p.is_null() {
                return String::new();
            }
            // SAFETY: a NUL-terminated UTF-16 string from Windows.
            let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
        }

        /// HKLM\`path`\SearchList, or None if it can't be read
        /// (WinRegistry::GetString).
        fn search_list(path: &str) -> Option<String> {
            let path: Vec<u16> = path.encode_utf16().chain([0]).collect();
            let value: Vec<u16> = "SearchList".encode_utf16().chain([0]).collect();
            let mut size = 0u32;
            let get = |buf: *mut u16, size: &mut u32| {
                // SAFETY: NUL-terminated names, and a buffer of `size` bytes
                // or none.
                unsafe {
                    RegGetValueW(
                        HKEY_LOCAL_MACHINE,
                        path.as_ptr(),
                        value.as_ptr(),
                        RRF_RT_REG_SZ,
                        std::ptr::null_mut(),
                        buf.cast(),
                        size,
                    )
                }
            };
            if get(std::ptr::null_mut(), &mut size) != 0 {
                return None;
            }
            let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
            if get(buf.as_mut_ptr(), &mut size) != 0 {
                return None;
            }
            Some(wide(buf.as_ptr()))
        }

        /// The suffixes nsNotifyAddrListener::CheckAdaptersAddresses
        /// gathers: each up adapter's own, then the policy's or the
        /// user's SearchList.
        pub(in super::super) fn search_domains() -> Vec<String> {
            let mut out = Vec::new();
            // GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_ANYCAST
            let flags = 0x4 | 0x2;
            let mut len = 16384u32;
            let mut buf = vec![0u64; len as usize / 8];
            // SAFETY: a buffer of `len` bytes, 8-aligned.
            let mut ret = unsafe {
                GetAdaptersAddresses(
                    0,
                    flags,
                    std::ptr::null(),
                    buf.as_mut_ptr().cast(),
                    &mut len,
                )
            };
            if ret == 111 {
                // ERROR_BUFFER_OVERFLOW
                buf = vec![0u64; (len as usize).div_ceil(8)];
                // SAFETY: as above.
                ret = unsafe {
                    GetAdaptersAddresses(
                        0,
                        flags,
                        std::ptr::null(),
                        buf.as_mut_ptr().cast(),
                        &mut len,
                    )
                };
            }
            if ret == 0 {
                let mut adapter = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
                while !adapter.is_null() {
                    // SAFETY: a list GetAdaptersAddresses wrote into `buf`.
                    let a = unsafe { &*adapter };
                    adapter = a.Next;
                    // IfOperStatusUp, IF_TYPE_SOFTWARE_LOOPBACK
                    if a.OperStatus != 1
                        || a.FirstUnicastAddress.is_null()
                        || a.IfType == 24
                        || wide(a.FriendlyName).contains("VMnet")
                        || wide(a.Description).contains("VMnet")
                    {
                        continue;
                    }
                    let suffix = wide(a.DnsSuffix);
                    if !suffix.is_empty() {
                        out.push(suffix);
                    }
                }
            }
            let list = search_list(r"SOFTWARE\Policies\Microsoft\Windows NT\DNSClient")
                .or_else(|| search_list(r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters"))
                .unwrap_or_default();
            let list: String = list.chars().filter(|c| !c.is_whitespace()).collect();
            out.extend(list.split(',').filter(|s| !s.is_empty()).map(String::from));
            out
        }

        pub(in super::super) fn lookup(host: &str, ttl: &mut u32) -> Res<Lookup> {
            let name = CString::new(host).map_err(|_| Fail::UnknownHost)?;
            let mut result: *mut DNS_RECORDA = std::ptr::null_mut();
            // SAFETY: a C string and an out pointer; DNS_QUERY_STANDARD.
            let status = unsafe {
                DnsQuery_A(
                    name.as_ptr().cast(),
                    65,
                    0,
                    std::ptr::null_mut(),
                    &mut result,
                    std::ptr::null_mut(),
                )
            };
            if status != 0 {
                return Err(Fail::UnknownHost);
            }
            let mut list = Vec::new();
            let mut at = result;
            while !at.is_null() {
                // SAFETY: the list DnsQuery_A returned, freed below. A type
                // 65 record's Data is its raw RDATA, as Firefox reads it.
                unsafe {
                    let r = &*at;
                    let data = match r.wType {
                        65 => std::slice::from_raw_parts(
                            (&raw const r.Data).cast::<u8>(),
                            r.wDataLength.into(),
                        )
                        .to_vec(),
                        5 if !r.Data.CNAME.pNameHost.is_null() => {
                            std::ffi::CStr::from_ptr(r.Data.CNAME.pNameHost.cast())
                                .to_bytes()
                                .to_vec()
                        }
                        _ => Vec::new(),
                    };
                    let name = if r.pName.is_null() {
                        Vec::new()
                    } else {
                        std::ffi::CStr::from_ptr(r.pName.cast()).to_bytes().to_vec()
                    };
                    list.push(WinRecord {
                        name,
                        kind: r.wType,
                        ttl: r.dwTtl,
                        data,
                    });
                    at = r.pNext;
                }
            }
            // SAFETY: DnsRecordListFree(result, DnsFreeRecordList).
            unsafe { DnsFree(result.cast(), 1) };
            windows_records(&list, host, ttl)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::type_complexity)]
    fn https(qname: &str, records: &[(u16, &str, &[(u16, Vec<u8>)])]) -> Vec<u8> {
        https_answer(qname, records, 300)
    }

    fn parse(packet: &[u8], host: &str) -> Res<Lookup> {
        let mut ttl = u32::MAX;
        parse_packet(packet, host, &mut ttl)
    }

    fn records(lookup: Res<Lookup>) -> Vec<Svcb> {
        match lookup {
            Ok(Lookup::Records(r)) => r,
            other => panic!("{other:?}"),
        }
    }

    fn alpn(ids: &[&str]) -> Vec<u8> {
        ids.iter()
            .flat_map(|id| std::iter::once(id.len() as u8).chain(id.bytes()))
            .collect()
    }

    fn set(records: Vec<Svcb>) -> Records {
        Records {
            records,
            ttl: Duration::from_secs(60),
        }
    }

    fn rec(priority: u16, target: &str, alpns: &[&str]) -> Svcb {
        Svcb {
            priority,
            target: target.into(),
            alpn: alpns.iter().map(|a| a.to_string()).collect(),
            ..Svcb::default()
        }
    }

    #[test]
    fn records_are_read_with_their_params() {
        let ech = vec![0xfe, 0x0d, 0, 1];
        let p = https(
            "example.com",
            &[(
                1,
                "svc.example.net",
                &[
                    (1, alpn(&["h3", "h2"])),
                    (3, 8443u16.to_be_bytes().to_vec()),
                    (4, vec![192, 168, 1, 1]),
                    (5, ech.clone()),
                    (6, vec![0; 16]),
                    (99, vec![1, 2, 3]),
                ],
            )],
        );
        let mut ttl = u32::MAX;
        let r = records(parse_packet(&p, "example.com", &mut ttl));
        assert_eq!(ttl, 300);
        assert_eq!(
            r,
            [Svcb {
                priority: 1,
                target: "svc.example.net".into(),
                alpn: vec!["h3".into(), "h2".into()],
                port: Some(8443),
                ech: Some(ech),
                ipv4hint: vec![Ipv4Addr::new(192, 168, 1, 1)],
                ipv6hint: vec![Ipv6Addr::UNSPECIFIED],
                ..Svcb::default()
            }]
        );
        // The owner matches case-insensitively, and with a trailing dot.
        assert_eq!(records(parse(&p, "EXAMPLE.com.")).len(), 1);
    }

    #[test]
    fn a_service_mode_dot_target_is_the_owner_port_prefix_and_all() {
        let p = https(
            "_8443._https.example.com",
            &[(1, ".", &[(1, alpn(&["h3"]))])],
        );
        let r = records(parse(&p, "_8443._https.example.com"));
        assert_eq!(r[0].target, "_8443._https.example.com");
        assert_eq!(
            qname("example.com", 8443).as_deref(),
            Some("_8443._https.example.com")
        );
        assert_eq!(qname("example.com", 443).as_deref(), Some("example.com"));
    }

    #[test]
    fn names_firefox_never_queries() {
        for host in [
            "",
            "127.0.0.1",
            "[::1]",
            "::1",
            "localhost",
            "a.localhost",
            "printer.local",
            "a b.com",
        ] {
            assert_eq!(qname(host, 443), None, "{host}");
        }
        assert_eq!(
            qname("local.example", 443).as_deref(),
            Some("local.example")
        );
        assert_eq!(qname(&"a".repeat(254), 443), None);
        let hosts = hosts_names(
            &b"127.0.0.1 localhost dev.test # gone.test\n::1 ip6.test 10.0.0.1 a:b\n\xff\n"[..],
        );
        assert_eq!(
            hosts,
            ["localhost", "dev.test", "ip6.test"]
                .map(String::from)
                .into()
        );
        assert!(excluded("_8443._https.www.dev.test", |d| hosts.contains(d)));
        assert!(!excluded("dev.test.org", |d| hosts.contains(d)));
    }

    #[test]
    fn compression_is_followed_in_the_question_answers_and_target() {
        // The question's name points into the answer's owner, which is
        // spelt out, and the target points back at it.
        let owner = wire_name("example.com");
        let mut p = vec![0, 0, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        p.extend([0xc0, 18, 0, 65, 0, 1]);
        let mut rdata = vec![0, 1, 3, b's', b'v', b'c', 0xc0, 18];
        rdata.extend([0, 1, 0, 3, 2, b'h', b'3']);
        p.extend(wire_record(&owner, 65, 1, 60, &rdata));
        let r = records(parse(&p, "example.com"));
        assert_eq!(
            (r[0].target.as_str(), r[0].alpn.as_slice()),
            ("svc.example.com", &["h3".to_string()][..])
        );
        // A pointer loop is an error.
        let mut looped = p[..12].to_vec();
        looped.extend([0xc0, 12, 0, 65, 0, 1]);
        assert_eq!(parse(&looped, "example.com"), Err(Fail::Bad));
    }

    #[test]
    fn cnames_are_followed_within_the_response() {
        let mut ttl = u32::MAX;
        let answers = [
            wire_record(&[0xc0, 12], 5, 1, 30, &wire_name("Edge.CDN.net")),
            wire_record(
                &wire_name("other.net"),
                65,
                1,
                99,
                &https_rdata(1, ".", &[]),
            ),
            wire_record(
                &wire_name("edge.cdn.net"),
                65,
                1,
                40,
                &https_rdata(2, ".", &[]),
            ),
        ];
        let p = response_packet("example.com", &answers, &[], &[]);
        let r = records(parse_packet(&p, "example.com", &mut ttl));
        // The owner that matched stands in for ".".
        assert_eq!(r, [rec(2, "edge.cdn.net", &[])]);
        assert_eq!(ttl, 40);
        // A CNAME whose target isn't answered: the name to query next.
        let p = response_packet("example.com", &answers[..1], &[], &[]);
        assert_eq!(
            parse(&p, "example.com"),
            Ok(Lookup::Alias {
                name: "edge.cdn.net".into(),
                from_alias_mode: false
            })
        );
    }

    #[test]
    fn owners_that_dont_match_are_skipped() {
        let answers = [wire_record(
            &wire_name("example.org"),
            65,
            1,
            60,
            &https_rdata(1, ".", &[]),
        )];
        let p = response_packet("example.com", &answers, &[], &[]);
        assert_eq!(parse(&p, "example.com"), Err(Fail::UnknownHost));
        // Other types are skipped whatever their owner.
        let answers = [
            wire_record(&[0xc0, 12], 1, 1, 60, &[1, 2, 3, 4]),
            wire_record(&[0xc0, 12], 65, 1, 60, &https_rdata(1, ".", &[])),
        ];
        let p = response_packet("example.com", &answers, &[], &[]);
        assert_eq!(records(parse(&p, "example.com")).len(), 1);
    }

    #[test]
    fn malformed_responses_fail_the_lookup() {
        let good = https("example.com", &[(1, ".", &[])]);
        // An answer not of class IN, even of a type Firefox skips.
        let answers = [wire_record(&[0xc0, 12], 1, 3, 60, &[1, 2, 3, 4])];
        assert_eq!(
            parse(
                &response_packet("example.com", &answers, &[], &[]),
                "example.com"
            ),
            Err(Fail::Bad)
        );
        // Trailing bytes.
        assert_eq!(
            parse(&[&good[..], &[0]].concat(), "example.com"),
            Err(Fail::Bad)
        );
        // Authority and additional records are walked, and must fit.
        let soa = wire_record(&[0xc0, 12], 6, 1, 60, &[0; 22]);
        let opt = wire_record(&[0], 41, 1232, 0, &[]);
        let p = response_packet("example.com", &[good[29..].to_vec()], &[soa], &[opt]);
        assert_eq!(records(parse(&p, "example.com")).len(), 1);
        assert_eq!(parse(&p[..p.len() - 1], "example.com"), Err(Fail::Bad));
        assert_eq!(parse(&good[..11], "example.com"), Err(Fail::Bad));
        let bad = |params: &[(u16, Vec<u8>)]| {
            parse(&https("example.com", &[(1, ".", params)]), "example.com")
        };
        // A mandatory key Firefox doesn't know.
        assert_eq!(bad(&[(0, 7u16.to_be_bytes().to_vec())]), Err(Fail::Bad));
        assert!(bad(&[(0, 1u16.to_be_bytes().to_vec()), (1, alpn(&["h2"]))]).is_ok());
        // Keys out of order, or repeated.
        assert_eq!(
            bad(&[(3, vec![1, 187]), (1, alpn(&["h2"]))]),
            Err(Fail::Bad)
        );
        assert_eq!(
            bad(&[(1, alpn(&["h2"])), (1, alpn(&["h3"]))]),
            Err(Fail::Bad)
        );
        // Wrong lengths.
        for params in [
            (2, vec![0]),
            (3, vec![1]),
            (4, vec![1, 2, 3]),
            (6, vec![0; 15]),
            (1, vec![3, b'h']),
        ] {
            assert_eq!(bad(&[params]), Err(Fail::Bad));
        }
    }

    #[test]
    fn alias_mode_dot_derails_the_decode_as_it_does_in_firefox() {
        // DecodeInternal's `continue` rereads the RDATA as a record, which
        // runs off the end here.
        let p = https("example.com", &[(0, ".", &[])]);
        assert_eq!(parse(&p, "example.com"), Err(Fail::Bad));
        // Mac and Windows just drop it.
        let mut answer = mac_answer();
        answer.record(0, b"example.com.", 65, &https_rdata(0, ".", &[]), 60);
        assert_eq!(answer.finish(&mut 0), Err(Fail::UnknownHost));
    }

    #[test]
    fn service_mode_records_sort_with_ech_first() {
        let p = https(
            "example.com",
            &[
                (3, "c.net", &[]),
                (1, "a.net", &[]),
                (2, "b.net", &[(5, vec![1])]),
            ],
        );
        let r = records(parse(&p, "example.com"));
        let targets: Vec<_> = r.iter().map(|r| r.target.as_str()).collect();
        assert_eq!(targets, ["b.net", "a.net", "c.net"]);
    }

    #[test]
    fn aliases_are_chased_across_lookups() {
        let seam: Seam = [
            (
                "example.com".into(),
                https("example.com", &[(0, "alias.net", &[])]),
            ),
            (
                "alias.net".into(),
                https("alias.net", &[(1, ".", &[(1, alpn(&["h3"]))])]),
            ),
        ]
        .into();
        let r = resolve("example.com", Some(&seam)).unwrap();
        assert_eq!(r.records, [rec(1, "alias.net", &["h3"])]);
        // A target with no records: the alias itself.
        let seam: Seam = [("example.com".into(), seam["example.com"].clone())].into();
        let r = resolve("example.com", Some(&seam)).unwrap();
        assert_eq!(r.records, [rec(0, "alias.net", &[])]);
        assert_eq!(r.ttl, Duration::from_secs(u32::MAX.into()));
        assert_eq!(
            choose(&r, "example.com", 443, None, &|_| false),
            Some(Route {
                host: "alias.net".into(),
                port: 443,
                alpn: String::new(),
                ech: None
            })
        );
        // A plain CNAME's target is the resolver's business.
        let cname = response_packet(
            "example.com",
            &[wire_record(&[0xc0, 12], 5, 1, 30, &wire_name("cdn.net"))],
            &[],
            &[],
        );
        let seam: Seam = [
            ("example.com".into(), cname),
            ("cdn.net".into(), https("cdn.net", &[(1, ".", &[])])),
        ]
        .into();
        assert_eq!(resolve("example.com", Some(&seam)), None);
        // Too long a chain.
        let seam: Seam = (0..9)
            .map(|i| {
                (
                    format!("a{i}.net"),
                    https(
                        &format!("a{i}.net"),
                        &[(0, &format!("a{}.net", i + 1), &[])],
                    ),
                )
            })
            .collect();
        assert_eq!(resolve("a0.net", Some(&seam)), None);
    }

    #[test]
    fn windows_walks_the_answer_list() {
        let list = [
            WinRecord {
                name: b"example.com".to_vec(),
                kind: 5,
                ttl: 20,
                data: b"CDN.net".to_vec(),
            },
            WinRecord {
                name: b"cdn.net".to_vec(),
                kind: 65,
                ttl: 50,
                data: https_rdata(2, ".", &[]),
            },
            WinRecord {
                name: b"cdn.net".to_vec(),
                kind: 65,
                ttl: 40,
                data: https_rdata(1, "x.net", &[]),
            },
        ];
        let mut ttl = 0;
        let r = records(windows_records(&list, "example.com", &mut ttl));
        // Unsorted, and the least TTL, the CNAME's included.
        assert_eq!(r, [rec(2, "cdn.net", &[]), rec(1, "x.net", &[])]);
        assert_eq!(ttl, 20);
        // Owners are compared exactly.
        assert_eq!(
            windows_records(&list, "EXAMPLE.com", &mut ttl),
            Err(Fail::UnknownHost)
        );
    }

    fn mac_answer() -> MacAnswer {
        MacAnswer {
            fail: None,
            records: None,
            alias: String::new(),
            ttl: u32::MAX,
        }
    }

    #[test]
    fn mac_takes_records_from_callbacks() {
        let mut answer = mac_answer();
        answer.record(0, b"cdn.net.", 5, &wire_name("x"), 10);
        answer.record(0, b"Cdn.net.", 65, &https_rdata(2, ".", &[]), 70);
        answer.record(0, b"cdn.net.", 65, &https_rdata(1, ".", &[]), 0);
        let mut ttl = 0;
        assert_eq!(
            records(answer.finish(&mut ttl)),
            [rec(2, "Cdn.net", &[]), rec(1, "cdn.net", &[])]
        );
        assert_eq!(ttl, 0);
        let mut answer = mac_answer();
        answer.record(0, b"a.net.", 65, &https_rdata(1, ".", &[]), 70);
        answer.record(-65554, b"", 0, &[], 0);
        assert_eq!(answer.finish(&mut ttl), Err(Fail::UnknownHost));
    }

    #[test]
    fn choose_prefers_h3_and_the_default_alpn_entry() {
        let records = set(vec![rec(1, "svc.net", &["h2", "h3"])]);
        let flat = flatten("example.com", &records.records).0;
        let alpns: Vec<_> = flat.iter().map(|f| f.1.clone().unwrap().0).collect();
        // h3 first, then the default entry; the explicit h2 folds into it.
        assert_eq!(alpns, ["h3", ""]);
        let route = choose(&records, "example.com", 443, None, &|_| false).unwrap();
        assert_eq!(
            (route.host.as_str(), route.port, route.alpn.as_str()),
            ("svc.net", 443, "h3")
        );
        // The origin as its own target without ECH: no default entry, and
        // h2 normalised to "".
        let own = [rec(1, "example.com", &["h2"])];
        let own = flatten("example.com", &own).0;
        assert_eq!(own[0].1, Some((String::new(), Rank::H2)));
        // no-default-alpn keeps h2 explicit.
        let mut nda = rec(1, "example.com", &["h2"]);
        nda.no_default_alpn = true;
        assert_eq!(
            flatten("example.com", std::slice::from_ref(&nda)).0[0].1,
            Some(("h2".into(), Rank::H2))
        );
    }

    #[test]
    fn choose_checks_the_cname() {
        let records = set(vec![rec(1, "a.net", &["h3"]), rec(2, "b.net", &["h3"])]);
        let route = choose(&records, "example.com", 443, Some("b.net"), &|_| false).unwrap();
        assert_eq!(route.host, "b.net");
        assert_eq!(
            choose(&records, "example.com", 443, Some("c.net"), &|_| false),
            None
        );
        assert_eq!(
            choose(&records, "example.com", 443, Some(""), &|_| false)
                .unwrap()
                .host,
            "a.net"
        );
    }

    #[test]
    fn choose_skips_excluded_h3_unless_all_are() {
        let records = set(vec![rec(1, "a.net", &["h3"]), rec(2, "b.net", &["h3"])]);
        let route = choose(&records, "example.com", 443, None, &|t| t == "a.net").unwrap();
        // a.net's own default entry comes before b.net's h3.
        assert_eq!((route.host.as_str(), route.alpn.as_str()), ("a.net", ""));
        let only_h3 = |t: &str| {
            let mut r = rec(1, t, &["h3"]);
            r.no_default_alpn = true;
            r
        };
        let records = set(vec![only_h3("a.net"), only_h3("b.net")]);
        let route = choose(&records, "example.com", 443, None, &|t| t == "a.net").unwrap();
        assert_eq!((route.host.as_str(), route.alpn.as_str()), ("b.net", "h3"));
        // All excluded, but every record no-default-alpn: rejected before
        // the retry is reached.
        assert_eq!(choose(&records, "example.com", 443, None, &|_| true), None);
        let records = set(vec![
            rec(1, "example.com", &["h3"]),
            rec(2, "example.com", &["h3"]),
        ]);
        let route = choose(&records, "example.com", 443, None, &|_| true).unwrap();
        assert_eq!(
            (route.host.as_str(), route.alpn.as_str()),
            ("example.com", "h3")
        );
    }

    #[test]
    fn choose_skips_banned_ports_and_uses_the_record_port() {
        let mut banned = rec(1, "a.net", &["h3"]);
        banned.port = Some(25);
        let mut ok = rec(2, "b.net", &["h3"]);
        ok.port = Some(8443);
        let route = choose(&set(vec![banned, ok]), "example.com", 443, None, &|_| false).unwrap();
        assert_eq!((route.host.as_str(), route.port), ("b.net", 8443));
    }

    #[test]
    fn choose_rejects_a_set_with_an_alias_among_service_records() {
        let records = set(vec![rec(0, "a.net", &[]), rec(1, "b.net", &["h3"])]);
        assert_eq!(choose(&records, "example.com", 443, None, &|_| false), None);
    }
}
