//! Each profile trusts its own browser's roots: Firefox's Mozilla's (the
//! NSS builtins), Chrome's and Safari's the Chrome Root Store. A root that
//! only one of the two stores holds decides the outcome per profile; the
//! test server's certificate stands in for one, through the hidden
//! `extra_built_in_roots`, since a real root's private key isn't at hand.

use netweir_core::{FetchErrorKind, FetchOptions, Fetcher, Profile, Roots, built_in_profiles};
use netweir_fingerprint::Server;

/// A test server whose certificate is kept in a directory of its own, and
/// the certificate, DER and PEM.
async fn test_server(name: &str) -> (Server, std::path::PathBuf, Vec<u8>, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("netweir-roots-{name}-{}", std::process::id()));
    let server = Server::start_trusted(0, false, &dir).await.unwrap();
    let der = std::fs::read(dir.join("cert.der")).unwrap();
    let pem = std::fs::read(dir.join("cert.pem")).unwrap();
    (server, dir, der, pem)
}

#[test]
fn firefox_trusts_mozillas_roots_and_the_others_chromes() {
    for (name, _) in built_in_profiles() {
        let roots = Profile::named(name).unwrap().tls.roots;
        let expected = if name.starts_with("firefox") {
            Roots::Mozilla
        } else {
            Roots::Chromium
        };
        assert_eq!(roots, expected, "{name}");
    }
}

#[tokio::test]
async fn a_root_only_one_store_holds_decides_per_profile() {
    let (server, dir, der, _) = test_server("store").await;
    let url = format!("https://localhost:{}/", server.port);
    for (profile, store, trusted) in [
        ("firefox", Roots::Mozilla, true),
        ("firefox", Roots::Chromium, false),
        ("chrome", Roots::Chromium, true),
        ("chrome", Roots::Mozilla, false),
        ("safari", Roots::Chromium, true),
        ("safari", Roots::Mozilla, false),
    ] {
        let mut options = FetchOptions::new(Profile::named(profile).unwrap());
        options.extra_built_in_roots = vec![(store, der.clone())];
        let fetched = Fetcher::new(options).unwrap().get(&url, &[]).await;
        match (trusted, fetched) {
            (true, Ok(response)) => assert_eq!(response.status, 200, "{profile}"),
            (false, Err(e)) => assert_eq!(e.kind, FetchErrorKind::Tls, "{profile}: {e}"),
            (true, Err(e)) => panic!("{profile} with the root in {store:?}: {e}"),
            (false, Ok(_)) => panic!("{profile} trusted a root only {store:?} holds"),
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn ca_certificates_take_the_place_of_the_bundled_roots() {
    let (server, dir, der, pem) = test_server("cas").await;
    let url = format!("https://localhost:{}/", server.port);
    for profile in ["firefox", "chrome"] {
        // Trusted by the CAs given, though no bundled store holds it...
        let mut options = FetchOptions::new(Profile::named(profile).unwrap());
        options.ca_certificates = Some(pem.clone());
        let response = Fetcher::new(options).unwrap().get(&url, &[]).await.unwrap();
        assert_eq!(response.status, 200, "{profile}");
        // ...and with CAs given, the bundled roots aren't trusted at all.
        let other = test_server("other").await;
        let mut options = FetchOptions::new(Profile::named(profile).unwrap());
        options.ca_certificates = Some(other.3.clone());
        let roots = Profile::named(profile).unwrap().tls.roots;
        options.extra_built_in_roots = vec![(roots, der.clone())];
        let error = Fetcher::new(options)
            .unwrap()
            .get(&url, &[])
            .await
            .unwrap_err();
        assert_eq!(error.kind, FetchErrorKind::Tls, "{profile}: {error}");
        let _ = std::fs::remove_dir_all(other.1);
    }
    let _ = std::fs::remove_dir_all(dir);
}
