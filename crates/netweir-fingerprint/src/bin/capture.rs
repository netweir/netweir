//! Prints the fingerprint of the next connection to https://localhost:PORT.
//!
//!     cargo run -p netweir-fingerprint --bin capture -- 8443 [--http1 | --h3]
//!
//! then open https://localhost:8443/ in the browser being captured. The
//! certificate is kept in ~/.netweir/capture/ ($NETWEIR_HOME moves it), so
//! a browser that can't be told to skip the check, such as Safari, can be
//! made to trust cert.pem there once.
//!
//! `--h3` also listens for QUIC on UDP port PORT and answers over TCP with
//! `Alt-Svc: h3`, so the browser's next connection is HTTP/3. Chrome won't
//! use QUIC with a certificate it was only told to accept; start it with
//! `--origin-to-force-quic-on=localhost:PORT` and
//! `--ignore-certificate-errors-spki-list=HASH`, HASH from
//!
//!     openssl x509 -in ~/.netweir/capture/cert.pem -pubkey -noout |
//!       openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64
//!
//! `--early` with `--h3` gives QUIC session tickets that allow early data
//! and takes requests in 0-RTT, set up for a browser to open /zero-rtt
//! (`Server::start_h3_early`): each capture's `http3.early` says whether
//! its request came in 0-RTT. `--hold-ms N` holds each QUIC datagram to
//! the browser back N ms, as a network would.
//!
//! `--qpack CAPACITY,BLOCKED` with `--h3` offers a QPACK dynamic table of
//! CAPACITY bytes and BLOCKED blocked streams (`Server::start_h3_qpack`),
//! for a browser to open /qpack: each capture's `http3.encoder` and
//! `http3.field_section` say what it put in the table and how each
//! request used it. With `--early` too, the server also takes 0-RTT, to
//! see whether a resumed connection fills the table before the server's
//! SETTINGS come. With `--http1` too, TCP offers only HTTP/1.1, which
//! Safari needs before it moves its requests to QUIC.

#[tokio::main]
async fn main() {
    let port = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8443);
    let http1 = std::env::args().any(|a| a == "--http1");
    let h3 = std::env::args().any(|a| a == "--h3");
    let early = std::env::args().any(|a| a == "--early");
    // `--hold-ms N`: each QUIC datagram to the browser held back N ms.
    let hold = std::env::args()
        .skip_while(|a| a != "--hold-ms")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .map_or(std::time::Duration::ZERO, std::time::Duration::from_millis);
    // `--qpack CAPACITY,BLOCKED`: the dynamic table offered.
    let qpack = std::env::args()
        .skip_while(|a| a != "--qpack")
        .nth(1)
        .and_then(|v| {
            let (capacity, blocked) = v.split_once(',')?;
            Some(netweir_fingerprint::QpackOffer {
                capacity: capacity.parse().ok()?,
                blocked_streams: blocked.parse().ok()?,
            })
        });
    let home = std::env::var_os("NETWEIR_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".netweir")))
        .expect("$HOME or $NETWEIR_HOME");
    let dir = home.join("capture");
    let mut server = if h3 && early {
        netweir_fingerprint::Server::start_h3_early_offering(port, Some(&dir), Some(hold), qpack)
            .await
    } else if let (true, Some(offer)) = (h3, qpack) {
        netweir_fingerprint::Server::start_h3_qpack(port, Some(&dir), offer, http1).await
    } else if h3 {
        netweir_fingerprint::Server::start_h3(port, Some(&dir)).await
    } else {
        netweir_fingerprint::Server::start_trusted(port, http1, &dir).await
    }
    .expect("bind");
    eprintln!("certificate: {}", dir.join("cert.pem").display());
    eprintln!("listening on https://localhost:{}/", server.port);
    while let Some(capture) = server.next().await {
        // Browsers open extra connections (favicon, preconnect); print each.
        println!("{}", serde_json::to_string_pretty(&capture).unwrap());
    }
}
