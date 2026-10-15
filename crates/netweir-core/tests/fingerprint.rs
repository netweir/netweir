//! A request made with a profile must be indistinguishable, on the wire,
//! from the browser the profile was captured from.
//!
//! The captures in `profiles/` record each browser doing four top-level
//! navigations against the recording server: a page that sets a cookie, a
//! revisit that sends it, a redirect to another site, and a redirect within
//! the site. Here netweir does the same four and every request is compared.
//! The post captures record forms, a page's fetch() and a link; the image
//! captures record a page's `<img>`. Every profile in `profiles/` is
//! replayed against its own captures, so a new one is covered by being
//! there.

use std::path::PathBuf;

use netweir_core::{FetchOptions, Fetcher, Outgoing, Profile};
use netweir_fingerprint::{Capture, Server, differences, extension_order_differs};

fn local_fetcher(profile: &str) -> Fetcher {
    let mut options = FetchOptions::new(Profile::named(profile).unwrap());
    options.verify_certificates = false;
    Fetcher::new(options).unwrap()
}

fn profiles_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles")
}

/// Every profile netweir ships: one per `profiles/*.toml`.
fn shipped() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(profiles_dir())
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".toml").map(str::to_string)
        })
        .collect();
    names.sort();
    assert!(!names.is_empty());
    names
}

/// A profile's recording of one scenario, such as `.post.http1`, if there
/// is one.
fn recorded(profile: &str, scenario: &str) -> Option<Vec<Capture>> {
    let path = profiles_dir().join(format!("{profile}{scenario}.capture.json"));
    let json = std::fs::read_to_string(path).ok()?;
    Some(serde_json::from_str(&json).unwrap())
}

/// Profiles whose image request is written from the browser's documented
/// behaviour rather than a recording of it.
const IMAGE_NOT_RECORDED: &[&str] = &["safari-27-macos"];

/// The navigations the browsers made for the captures, against `port`.
/// Each redirect is one navigation but two requests, so this makes six.
fn scenario(port: u16) -> [String; 4] {
    [
        format!("https://localhost:{port}/?set=nw"),
        format!("https://localhost:{port}/"),
        format!("https://localhost:{port}/redirect?to=https://127.0.0.1:{port}/after&set=r"),
        format!("https://localhost:{port}/redirect?to=https://localhost:{port}/final&set=s"),
    ]
}

async fn run(fetcher: &Fetcher, server: &mut Server, urls: &[String]) -> Vec<Capture> {
    let mut seen = Vec::new();
    for url in urls {
        let response = fetcher.get(url, &[]).await.unwrap();
        assert_eq!(response.status, 200, "{url}");
        let hops = if url.contains("/redirect") { 2 } else { 1 };
        for _ in 0..hops {
            seen.push(server.next().await.unwrap());
        }
    }
    seen
}

/// Each request of `actual` against the browser's, and, for a browser that
/// doesn't shuffle its extensions, their order too.
///
/// The browsers were recorded opening a second connection to localhost
/// that resumed the first one's session, while netweir keeps its first
/// connection. A resumed handshake differs from a fresh one in more than
/// the pre_shared_key extension (Firefox leaves out session_ticket), so
/// a request netweir sent on a fresh handshake is compared with the
/// browser's fresh handshake to that host over the same protocol, and
/// everything above TLS with the request itself. Resumed handshakes are
/// compared on their own, in `resumed_handshakes_are_indistinguishable`.
fn check(profile: &str, http1: bool, expected: &[Capture], actual: &[Capture], what: &str) {
    const PRE_SHARED_KEY: u16 = 0x0029;
    let resumed = |c: &Capture| c.client_hello.extensions.contains(&PRE_SHARED_KEY);
    let fresh = recorded(profile, if http1 { ".http1" } else { "" }).unwrap();
    let in_order = !Profile::named(profile).unwrap().tls.permute_extensions;
    let what = format!("{profile} {what}");
    assert_eq!(expected.len(), actual.len(), "{what}: request count");
    for (e, a) in expected.iter().zip(actual) {
        let mut e = e.clone();
        if resumed(&e) && !resumed(a) {
            let first = fresh
                .iter()
                .find(|f| !resumed(f) && f.client_hello.server_name == e.client_hello.server_name)
                .unwrap_or_else(|| panic!("{what}: no fresh handshake recorded"));
            e.client_hello = first.client_hello.clone();
            e.ja4 = first.ja4.clone();
        }
        let diff = differences(&e, a);
        assert!(
            diff.is_empty(),
            "{what}: request {} ({}) differs from the browser:\n{}",
            a.request,
            e.path,
            diff.join("\n")
        );
        if in_order && let Some(diff) = extension_order_differs(&e, a) {
            panic!("{what}: request {} ({}): {diff}", a.request, e.path);
        }
    }
}

#[test]
fn every_shipped_profile_is_registered_and_has_its_recordings() {
    for name in shipped() {
        let profile = Profile::named(&name).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(profile.name, name);
        let mut needed = vec!["", ".http1", ".post", ".post.http1"];
        if !IMAGE_NOT_RECORDED.contains(&name.as_str()) {
            needed.extend([".image", ".image.http1"]);
        }
        for scenario in needed {
            assert!(
                recorded(&name, scenario).is_some(),
                "{name} has no {name}{scenario}.capture.json"
            );
        }
    }
}

#[tokio::test]
async fn navigations_over_http2_are_indistinguishable() {
    for name in shipped() {
        let expected = recorded(&name, "").unwrap();
        // Several runs, each on fresh connections, because Chrome shuffles
        // its extensions and GREASE values per connection and so must we.
        for _ in 0..3 {
            let mut server = Server::start().await.unwrap();
            let fetcher = local_fetcher(&name);
            let urls = scenario(server.port);
            let actual = run(&fetcher, &mut server, &urls).await;
            check(&name, false, &expected, &actual, "over HTTP/2");
        }
    }
}

#[tokio::test]
async fn navigations_over_http1_are_indistinguishable() {
    for name in shipped() {
        let expected = recorded(&name, ".http1").unwrap();
        let mut server = Server::start_http1(0).await.unwrap();
        let port = server.port;
        let fetcher = local_fetcher(&name);
        // A first request can't know a host lacks HTTP/2 (see
        // Fetcher::expects_http2), so let netweir meet both hosts first.
        // Safari had met localhost over HTTP/1.1 before this navigation
        // (and so offered it only http/1.1), but not 127.0.0.1.
        let remembers = Profile::named(&name).unwrap().remembers_http1;
        let mut warm = vec![format!("https://localhost:{port}/warm")];
        if !remembers {
            warm.push(format!("https://127.0.0.1:{port}/warm"));
        }
        run(&fetcher, &mut server, &warm).await;
        let actual = run(&fetcher, &mut server, &scenario(port)).await;
        let expected = if remembers {
            either_pooled_connection(expected, &actual)
        } else {
            expected
        };
        check(&name, true, &expected, &actual, "over HTTP/1.1");
    }
}

#[tokio::test]
async fn extension_order_is_shuffled_per_connection_like_chrome() {
    let mut server = Server::start().await.unwrap();
    let url = format!("https://localhost:{}/", server.port);
    let mut orders = std::collections::HashSet::new();
    for _ in 0..5 {
        let fetcher = local_fetcher("chrome");
        let captures = run(&fetcher, &mut server, std::slice::from_ref(&url)).await;
        orders.insert(captures[0].client_hello.extensions.clone());
    }
    assert!(
        orders.len() > 1,
        "five connections sent the same extension order"
    );
}

#[tokio::test]
async fn caller_headers_replace_in_place_and_new_ones_go_last() {
    for http1 in [false, true] {
        let mut server = if http1 {
            Server::start_http1(0).await
        } else {
            Server::start().await
        }
        .unwrap();
        let url = format!("https://localhost:{}/", server.port);
        let fetcher = local_fetcher("chrome");
        if http1 {
            run(&fetcher, &mut server, std::slice::from_ref(&url)).await;
        }
        let headers = [
            ("accept-language".to_string(), "fr-FR,fr;q=0.9".to_string()),
            ("X-Trace".to_string(), "1".to_string()),
        ];
        fetcher.get(&url, &headers).await.unwrap();
        let seen = server.next().await.unwrap();
        let names: Vec<&str> = seen.headers.iter().map(|(k, _)| k.as_str()).collect();
        let lang = names
            .iter()
            .position(|k| k.eq_ignore_ascii_case("accept-language"))
            .unwrap();
        assert!(
            names[lang - 1].eq_ignore_ascii_case("accept-encoding"),
            "{names:?}"
        );
        assert_eq!(seen.headers[lang].1, "fr-FR,fr;q=0.9");
        assert_eq!(
            names
                .iter()
                .filter(|k| k.eq_ignore_ascii_case("accept-language"))
                .count(),
            1
        );
        if http1 {
            // The caller's capitalisation survives on HTTP/1.1.
            assert_eq!(names[lang], "Accept-Language");
            assert_eq!(names.last(), Some(&"X-Trace"), "{names:?}");
        } else {
            // HTTP/2: Chrome's HTTP/2-only header follows the profile's own,
            // and the caller's new header comes after everything.
            assert_eq!(&names[names.len() - 2..], ["priority", "x-trace"]);
        }
    }
}

/// The browsers' recordings include a second connection to localhost that
/// resumed the first one's session. Here netweir is made to do the same:
/// the server hangs up after `/close` (which also sets the cookie the
/// recorded revisit carries), so the revisit goes on a new connection with
/// the session netweir was given, and is held to the browser's resumed
/// revisit: the same handshake, pre_shared_key in the same place, and the
/// same JA4.
#[tokio::test]
async fn resumed_handshakes_are_indistinguishable() {
    const PRE_SHARED_KEY: u16 = 0x0029;
    let from_end = |c: &Capture| {
        let ext = &c.client_hello.extensions;
        ext.iter()
            .position(|&x| x == PRE_SHARED_KEY)
            .map(|i| ext.len() - i)
    };
    let mut checked = 0;
    for name in shipped() {
        let recording = recorded(&name, ".http1").unwrap();
        let Some(expected) = recording
            .iter()
            .find(|c| c.path == "/" && from_end(c).is_some())
        else {
            continue;
        };
        let mut server = Server::start_http1(0).await.unwrap();
        let port = server.port;
        let fetcher = local_fetcher(&name);
        let urls = [
            format!("https://localhost:{port}/close?set=nw"),
            format!("https://localhost:{port}/"),
        ];
        let seen = run(&fetcher, &mut server, &urls).await;
        let actual = &seen[1];
        let what = format!("{name} resuming over HTTP/1.1");
        assert_ne!(
            actual.connection, seen[0].connection,
            "{what}: no new connection"
        );
        assert_eq!(
            from_end(actual),
            from_end(expected),
            "{what}: pre_shared_key place"
        );
        check(
            &name,
            true,
            std::slice::from_ref(expected),
            std::slice::from_ref(actual),
            "resuming",
        );
        checked += 1;
    }
    assert!(checked > 0, "no recording resumed a session");
}

#[tokio::test]
async fn too_many_redirects_is_an_error_not_a_loop() {
    let server = Server::start().await.unwrap();
    let fetcher = local_fetcher("chrome");
    // /loop redirects to itself, with a relative Location.
    let url = format!("https://localhost:{}/loop", server.port);
    let err = fetcher.get(&url, &[]).await.unwrap_err();
    assert_eq!(
        err.kind,
        netweir_core::FetchErrorKind::TooManyRedirects,
        "{err}"
    );
}

/// Safari kept two connections to localhost: one opened before it learned
/// the origin was HTTP/1.1-only, which still carries the h2 offer, and one
/// opened after. Which of the two a request rides is Safari's pooling, not
/// something a site can tell apart, so a request Safari sent on a
/// connection opened before the scenario may carry either connection's
/// ALPN offer, as long as Safari made that offer to the same host and
/// netweir sent it on a connection it had already used. A new connection
/// is held to Safari's offer exactly.
fn either_pooled_connection(mut expected: Vec<Capture>, actual: &[Capture]) -> Vec<Capture> {
    let opened_here = |c: &Capture| {
        expected
            .iter()
            .any(|o| o.connection == c.connection && o.request == 0)
    };
    let pooled: Vec<bool> = expected.iter().map(|e| !opened_here(e)).collect();
    let offers: Vec<(Option<String>, Vec<String>, String)> = expected
        .iter()
        .map(|e| {
            let hello = &e.client_hello;
            (hello.server_name.clone(), hello.alpn.clone(), e.ja4.clone())
        })
        .collect();
    for ((e, a), pooled) in expected.iter_mut().zip(actual).zip(pooled) {
        if !pooled || a.request == 0 {
            continue;
        }
        let host = &e.client_hello.server_name;
        if let Some((_, alpn, ja4)) = offers
            .iter()
            .find(|(h, alpn, _)| h == host && *alpn == a.client_hello.alpn)
        {
            e.client_hello.alpn = alpn.clone();
            e.ja4 = ja4.clone();
        }
    }
    expected
}

/// What the browsers did after the navigations, in the post captures:
/// submitted a form, posted JSON from a script, signed in through a form
/// answered with a redirect, and followed a link. Each on the page that
/// started it.
async fn post_scenario(fetcher: &Fetcher, server: &mut Server) -> Vec<Capture> {
    let base = format!("https://localhost:{}", server.port);
    let page = |path: &str| format!("{base}{path}");
    // The cookie the captured requests carry.
    fetcher.get(&page("/?set=nw"), &[]).await.unwrap();
    server.next().await.unwrap();
    let steps = [
        (
            "/submitted",
            Outgoing::form(b"q=rust&page=2".to_vec(), None, Some(page("/form"))),
            1,
        ),
        (
            "/api",
            Outgoing::fetch(
                "POST",
                Some(br#"{"q":"rust","page":2}"#.to_vec()),
                Some("application/json".into()),
                Some(page("/fetch")),
            ),
            1,
        ),
        (
            "/redirect?to=/after",
            Outgoing::form(b"user=ada".to_vec(), None, Some(page("/form-redirect"))),
            2,
        ),
        ("/linked", Outgoing::link(page("/link")), 1),
    ];
    let mut seen = Vec::new();
    for (path, outgoing, requests) in steps {
        let response = fetcher.send(&page(path), &outgoing, &[]).await.unwrap();
        assert_eq!(response.status, 200, "{path}");
        for _ in 0..requests {
            seen.push(server.next().await.unwrap());
        }
    }
    seen
}

#[tokio::test]
async fn forms_and_scripts_post_like_the_browser_over_http2() {
    for name in shipped() {
        let expected = recorded(&name, ".post").unwrap();
        let mut server = Server::start().await.unwrap();
        let fetcher = local_fetcher(&name);
        let actual = post_scenario(&fetcher, &mut server).await;
        check(&name, false, &expected, &actual, "posting over HTTP/2");
    }
}

#[tokio::test]
async fn forms_and_scripts_post_like_the_browser_over_http1() {
    for name in shipped() {
        let expected = recorded(&name, ".post.http1").unwrap();
        let mut server = Server::start_http1(0).await.unwrap();
        let fetcher = local_fetcher(&name);
        let remembers = Profile::named(&name).unwrap().remembers_http1;
        if !remembers {
            // As for the navigations: let netweir meet the host first, so
            // it knows there's no HTTP/2 to offer.
            let warm = [format!("https://localhost:{}/warm", server.port)];
            run(&fetcher, &mut server, &warm).await;
        }
        let actual = post_scenario(&fetcher, &mut server).await;
        let expected = if remembers {
            learned_http1(expected, &actual)
        } else {
            expected
        };
        check(&name, true, &expected, &actual, "posting over HTTP/1.1");
    }
}

/// Safari sent the whole scenario on the connection it opened before it
/// learned the server speaks only HTTP/1.1, which offered h2 too; any
/// connection it opens after learning offers http/1.1 alone (the
/// navigation capture shows one). netweir opens a new connection where
/// Safari kept the old one, so requests of netweir's on a connection it
/// opened after learning carry the after-learning offer. Anything else
/// about them must still match.
fn learned_http1(mut expected: Vec<Capture>, actual: &[Capture]) -> Vec<Capture> {
    for (e, a) in expected.iter_mut().zip(actual) {
        if a.client_hello.alpn == ["http/1.1"] {
            e.client_hello.alpn = vec!["http/1.1".into()];
            e.ja4 = e.ja4.replacen("h2_", "h1_", 1);
        }
    }
    expected
}

#[tokio::test]
async fn a_form_from_another_site_says_so_and_gives_only_its_origin() {
    for name in shipped() {
        let mut server = Server::start().await.unwrap();
        let fetcher = local_fetcher(&name);
        let page = "https://shop.example/basket?id=7#items";
        let outgoing = Outgoing::form(b"a=1".to_vec(), None, Some(page.into()));
        let url = format!("https://localhost:{}/submitted", server.port);
        fetcher.send(&url, &outgoing, &[]).await.unwrap();
        let capture = server.next().await.unwrap();
        let header = |name: &str| {
            capture
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(header("sec-fetch-site"), Some("cross-site"), "{name}");
        assert_eq!(header("origin"), Some("https://shop.example"), "{name}");
        assert_eq!(header("referer"), Some("https://shop.example/"), "{name}");
        assert_eq!(
            (capture.method.as_str(), capture.body.as_str()),
            ("POST", "a=1"),
            "{name}"
        );
    }
}

/// An `<img>` on `/image` loading `/picture.png`, as the image captures
/// recorded it: the request netweir.images makes.
async fn image_scenario(fetcher: &Fetcher, server: &mut Server) -> Vec<Capture> {
    let page = format!("https://localhost:{}/image", server.port);
    let picture = format!("https://localhost:{}/picture.png", server.port);
    let response = fetcher
        .send(&picture, &Outgoing::image(Some(page)), &[])
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    vec![server.next().await.unwrap()]
}

/// The image captures of Chrome 154 and Firefox 156 were recorded from the
/// browser a version on (Chrome 155, Firefox 157). The headers whose value
/// differs between the two versions are named here and taken from the
/// profile's own `[image]` shape: for Chrome the User-Agent, the client
/// hints and Accept (155 added image/jxl); for Firefox only the
/// User-Agent, since 157 sent the same Accept as 156. Their order and
/// everything else, TLS and HTTP/2 included, must match the recording.
fn recorded_a_version_on(profile: &str) -> &'static [&'static str] {
    match profile {
        "chrome-154-macos" => &["user-agent", "sec-ch-ua", "accept"],
        "firefox-156-macos" => &["user-agent"],
        _ => &[],
    }
}

fn at_profile_version(mut expected: Vec<Capture>, profile: &str) -> Vec<Capture> {
    let versioned = recorded_a_version_on(profile);
    let shape = Profile::named(profile).unwrap().image.unwrap();
    for capture in &mut expected {
        for (name, value) in &mut capture.headers {
            if !versioned.contains(&name.to_ascii_lowercase().as_str()) {
                continue;
            }
            if let Some((_, own)) = shape
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
            {
                *value = own.clone();
            }
        }
    }
    expected
}

#[tokio::test]
async fn images_load_like_the_browser_over_http2() {
    for name in shipped() {
        let Some(expected) = recorded(&name, ".image") else {
            continue;
        };
        let expected = at_profile_version(expected, &name);
        let mut server = Server::start().await.unwrap();
        let fetcher = local_fetcher(&name);
        let actual = image_scenario(&fetcher, &mut server).await;
        check(
            &name,
            false,
            &expected,
            &actual,
            "loading an image over HTTP/2",
        );
    }
}

#[tokio::test]
async fn images_load_like_the_browser_over_http1() {
    for name in shipped() {
        let Some(expected) = recorded(&name, ".image.http1") else {
            continue;
        };
        let expected = at_profile_version(expected, &name);
        let mut server = Server::start_http1(0).await.unwrap();
        let fetcher = local_fetcher(&name);
        let warm = [format!("https://localhost:{}/warm", server.port)];
        run(&fetcher, &mut server, &warm).await;
        let actual = image_scenario(&fetcher, &mut server).await;
        check(
            &name,
            true,
            &expected,
            &actual,
            "loading an image over HTTP/1.1",
        );
    }
}

/// The image tests take some headers that name a version from the
/// `[image]` shape, so those are checked here instead: the same as the
/// profile's own navigation, and, for Chrome 154's Accept, the value
/// Chrome for Testing 154 sent for an image (155 added image/jxl).
#[test]
fn image_shapes_name_the_profiles_own_versions() {
    for name in shipped() {
        let profile = Profile::named(&name).unwrap();
        let image = profile.image.as_ref().unwrap();
        let value = |headers: &[(String, String)], key: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.clone())
        };
        for key in [
            "user-agent",
            "sec-ch-ua",
            "sec-ch-ua-mobile",
            "sec-ch-ua-platform",
        ] {
            assert_eq!(
                value(&image.headers, key),
                value(&profile.headers, key),
                "{name} [image] {key}"
            );
        }
    }
    let chrome = Profile::named("chrome-154-macos").unwrap();
    let accept = chrome
        .image
        .unwrap()
        .headers
        .into_iter()
        .find(|(k, _)| k == "Accept")
        .map(|(_, v)| v);
    assert_eq!(
        accept.as_deref(),
        Some("image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8")
    );
}

/// Checking the server's certificate happens on the client, after the
/// server has the ClientHello: whether it's checked, and against which
/// CAs, changes nothing a server sees. Each profile fetches from a server
/// whose certificate only a CA file of its own vouches for, once with the
/// check off and once trusting that file alone, and both are held to the
/// browser's recording. With the bundled roots the same server is refused.
#[tokio::test]
async fn checking_certificates_or_not_looks_the_same_on_the_wire() {
    let dir = std::env::temp_dir().join(format!("netweir-ca-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for name in shipped() {
        let recording = recorded(&name, "").unwrap();
        let expected = recording.iter().find(|c| c.path == "/?set=nw").unwrap();
        let mut server = Server::start_trusted(0, false, &dir).await.unwrap();
        let url = format!("https://localhost:{}/?set=nw", server.port);
        let mut trusting = FetchOptions::new(Profile::named(&name).unwrap());
        trusting.ca_certificates = Some(std::fs::read(dir.join("cert.pem")).unwrap());
        for (what, fetcher) in [
            ("unchecked", local_fetcher(&name)),
            ("checked against a CA file", Fetcher::new(trusting).unwrap()),
        ] {
            let response = fetcher.get(&url, &[]).await.unwrap();
            assert_eq!(response.status, 200, "{name} {what}");
            let actual = server.next().await.unwrap();
            check(
                &name,
                false,
                std::slice::from_ref(expected),
                std::slice::from_ref(&actual),
                what,
            );
        }
        let bundled = Fetcher::new(FetchOptions::new(Profile::named(&name).unwrap())).unwrap();
        let err = bundled.get(&url, &[]).await.unwrap_err();
        assert_eq!(err.kind, netweir_core::FetchErrorKind::Tls, "{name}: {err}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The headers of `capture` as a server sees them, without the ones that
/// name the test server (Host, :authority, :path) and with its port taken
/// out of the rest, and without `skip`.
fn headers_of(capture: &Capture, skip: &[&str]) -> Vec<(String, String)> {
    let unported = |v: &str| {
        let mut out = String::new();
        let mut rest = v;
        while let Some(at) = rest.find("localhost:") {
            out.push_str(&rest[..at + "localhost".len()]);
            rest = rest[at + "localhost:".len()..].trim_start_matches(|c: char| c.is_ascii_digit());
        }
        out + rest
    };
    capture
        .headers
        .iter()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !["host", ":authority", ":path"].contains(&k.as_str()) && !skip.contains(&k.as_str())
        })
        .map(|(k, v)| (k.clone(), unported(v)))
        .collect()
}

/// What the browsers sent once they held a login, in the auth captures:
/// the page typed in, then the page's script POSTing JSON, its image, and
/// its form, answered with a redirect. Netweir sends each with the login
/// up front.
async fn login_scenario(fetcher: &Fetcher, server: &mut Server) -> Vec<Capture> {
    let base = format!("https://localhost:{}", server.port);
    let page = format!("{base}/auth");
    let ada = || netweir_core::Policy {
        auth: Some(netweir_core::Auth::new("ada", "secret", None)),
        ..Default::default()
    };
    let with_login = |mut outgoing: Outgoing| {
        outgoing.policy = ada();
        outgoing
    };
    let steps = [
        ("/auth", with_login(Outgoing::navigate()), 1),
        (
            "/api",
            with_login(Outgoing::fetch(
                "POST",
                Some(br#"{"q":"rust"}"#.to_vec()),
                Some("application/json".into()),
                Some(page.clone()),
            )),
            1,
        ),
        (
            "/picture.png",
            with_login(Outgoing::image(Some(page.clone()))),
            1,
        ),
        (
            "/redirect?to=/after-auth",
            with_login(Outgoing::form(b"q=rust".to_vec(), None, Some(page.clone()))),
            2,
        ),
    ];
    let mut seen = Vec::new();
    for (path, outgoing, requests) in steps {
        let response = fetcher
            .send(&format!("{base}{path}"), &outgoing, &[])
            .await
            .unwrap();
        assert_eq!(response.status, 200, "{path}");
        for _ in 0..requests {
            seen.push(server.next().await.unwrap());
        }
    }
    seen
}

/// From a browser's auth recording, the requests `login_scenario` makes:
/// the last typed visit to /auth that carried the login (Firefox's retry
/// of the request a 401 answered puts it elsewhere; its revisit is what
/// holding a login looks like), and then the page's own requests.
fn login_requests(recording: &[Capture]) -> Vec<Capture> {
    let carries = |c: &Capture| {
        c.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    };
    let typed = recording
        .iter()
        .rev()
        .find(|c| c.path.starts_with("/auth") && carries(c))
        .expect("a visit to /auth with the login");
    let mut out = vec![typed.clone()];
    for path in [
        "/api",
        "/picture.png",
        "/redirect?to=/after-auth",
        "/after-auth",
    ] {
        let c = recording.iter().find(|c| c.path == path).unwrap();
        assert!(carries(c), "{path} was recorded without the login");
        out.push(c.clone());
    }
    out
}

/// Authorization goes where the browser puts it, in every kind of request,
/// and so does every other header around it. Two things the recorded page
/// did by script are left out: it added its image from a script, which
/// Chrome loads at a lower priority than an `<img>` in the HTML, as
/// netweir's image requests are sent; and it submitted its form from a
/// script, so no Sec-Fetch-User, which a person pressing the button sends
/// and netweir does. The image and post tests check both.
#[tokio::test]
async fn logins_go_where_the_browser_puts_them() {
    let mut checked = 0;
    for name in shipped() {
        for http1 in [false, true] {
            let scenario = if http1 { ".auth.http1" } else { ".auth" };
            let Some(recording) = recorded(&name, scenario) else {
                continue;
            };
            let expected = login_requests(&recording);
            let mut server = if http1 {
                Server::start_http1(0).await
            } else {
                Server::start().await
            }
            .unwrap();
            let fetcher = local_fetcher(&name);
            if http1 {
                let warm = [format!("https://localhost:{}/warm", server.port)];
                run(&fetcher, &mut server, &warm).await;
            }
            let actual = login_scenario(&fetcher, &mut server).await;
            assert_eq!(expected.len(), actual.len(), "{name}{scenario}");
            for (e, a) in expected.iter().zip(&actual) {
                let skip: &[&str] = match e.path.as_str() {
                    "/picture.png" => &["priority"],
                    "/redirect?to=/after-auth" | "/after-auth" => &["sec-fetch-user"],
                    _ => &[],
                };
                assert_eq!(
                    headers_of(a, skip),
                    headers_of(e, skip),
                    "{name}{scenario}: {} differs from the browser's",
                    e.path
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 4, "only {checked} auth recordings replayed");
}
