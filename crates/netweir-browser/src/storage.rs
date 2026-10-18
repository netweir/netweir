//! A context's storage state, its cookies and each origin's localStorage,
//! in the JSON Playwright saves (`storageState`), so a file either writes
//! the other reads.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::browser::Inner;
use crate::page::{Cookie, Page, WaitUntil};
use crate::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StorageState {
    pub cookies: Vec<Cookie>,
    pub origins: Vec<OriginStorage>,
}

/// One origin's localStorage.
#[derive(Debug, Clone, PartialEq)]
pub struct OriginStorage {
    /// `https://shop.example`, as `location.origin` says it.
    pub origin: String,
    pub local_storage: Vec<(String, String)>,
}

impl StorageState {
    /// As Playwright writes it: cookies with its key names, `expires` -1
    /// for a session cookie.
    pub fn to_json(&self) -> Value {
        let cookies: Vec<Value> = self
            .cookies
            .iter()
            .map(|c| {
                let mut v = json!({
                    "name": c.name,
                    "value": c.value,
                    "domain": c.domain,
                    "path": c.path,
                    "expires": c.expires,
                    "httpOnly": c.http_only,
                    "secure": c.secure,
                });
                if let Some(s) = &c.same_site {
                    v["sameSite"] = Value::from(s.as_str());
                }
                if let Some(k) = &c.partition_key {
                    v["partitionKey"] = Value::from(k.as_str());
                }
                v
            })
            .collect();
        let origins: Vec<Value> = self
            .origins
            .iter()
            .map(|o| {
                json!({
                    "origin": o.origin,
                    "localStorage": o.local_storage.iter()
                        .map(|(k, v)| json!({"name": k, "value": v}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({"cookies": cookies, "origins": origins})
    }

    /// Reads Playwright's JSON, or Netweir's own cookie dicts' key names
    /// (`http_only`, `same_site`, `partition_key`).
    pub fn from_json(v: &Value) -> Result<StorageState> {
        let bad = |what: &str| Error::Invalid(format!("a storage state {what}"));
        if !v.is_object() {
            return Err(bad("must be a JSON object"));
        }
        let mut state = StorageState::default();
        for c in v["cookies"].as_array().into_iter().flatten() {
            state.cookies.push(cookie_of(c)?);
        }
        for o in v["origins"].as_array().into_iter().flatten() {
            let origin = o["origin"]
                .as_str()
                .ok_or_else(|| bad("origin needs an \"origin\""))?
                .to_string();
            let mut local_storage = Vec::new();
            for item in o["localStorage"].as_array().into_iter().flatten() {
                let (Some(name), Some(value)) = (item["name"].as_str(), item["value"].as_str())
                else {
                    return Err(bad("localStorage item needs a \"name\" and a \"value\""));
                };
                local_storage.push((name.to_string(), value.to_string()));
            }
            state.origins.push(OriginStorage {
                origin,
                local_storage,
            });
        }
        Ok(state)
    }

    /// Reads a file `to_json` wrote, or Playwright did.
    pub fn read(path: &Path) -> Result<StorageState> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Invalid(format!("can't read {}: {e}", path.display())))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| {
            Error::Invalid(format!("{} isn't a storage state: {e}", path.display()))
        })?;
        StorageState::from_json(&v)
    }

    /// Writes `to_json` to `path`, readable by its owner alone.
    pub fn write(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(&self.to_json()).unwrap_or_default();
        write_private(path, text.as_bytes())
            .map_err(|e| Error::Invalid(format!("can't write {}: {e}", path.display())))
    }
}

/// A cookie from either spelling of its keys.
pub fn cookie_of(c: &Value) -> Result<Cookie> {
    let text = |keys: &[&str]| keys.iter().find_map(|k| c[*k].as_str().map(str::to_string));
    let flag = |keys: &[&str]| keys.iter().any(|k| c[*k].as_bool() == Some(true));
    let needed =
        |key: &str| text(&[key]).ok_or_else(|| Error::Invalid(format!("a cookie needs a {key:?}")));
    Ok(Cookie {
        name: needed("name")?,
        value: needed("value")?,
        domain: needed("domain")?,
        path: text(&["path"]).unwrap_or_else(|| "/".into()),
        expires: c["expires"].as_f64().unwrap_or(-1.0),
        http_only: flag(&["httpOnly", "http_only"]),
        secure: flag(&["secure"]),
        same_site: text(&["sameSite", "same_site"]),
        partition_key: text(&["partitionKey", "partition_key"])
            .or_else(|| text(&["topLevelSite"]))
            .or_else(|| {
                c["partitionKey"]["topLevelSite"]
                    .as_str()
                    .map(str::to_string)
            }),
    })
}

/// Writes `bytes` to `path` through a new file, made readable by its owner
/// alone before anything is in it, then renamed over `path`: what it
/// holds is never readable by others on the way. Each save has a file of
/// its own, so saves to one path at once each land whole, the last
/// renamed winning. Known limit: on Windows the file takes its folder's
/// permissions.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::hash::{BuildHasher, Hasher};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SAVES: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // The process, a count and a random part: unique among this
    // process's saves, and another's on a shared disk. A name that's
    // taken all the same is passed over, never removed.
    let mut tries = 0;
    let (temp, mut file) = loop {
        let random = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        let temp = dir.join(format!(
            ".{name}.{}.{}.{random:016x}.tmp",
            std::process::id(),
            SAVES.fetch_add(1, Ordering::Relaxed)
        ));
        match options.open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 16 => tries += 1,
            Err(e) => return Err(e),
        }
    };
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    match written.and_then(|()| std::fs::rename(&temp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// `context`'s cookies, and the localStorage of each of `origins`.
pub(crate) async fn state_of(
    browser: &Arc<Inner>,
    context: Option<&str>,
    origins: &[String],
) -> Result<StorageState> {
    Ok(StorageState {
        cookies: crate::browser::context_cookies(browser, context).await?,
        origins: read_local(browser, context, origins).await?,
    })
}

/// Sets `state` in `context`: its cookies, and its origins' localStorage,
/// which are remembered as the context's own from then on.
pub(crate) async fn set_state(
    browser: &Arc<Inner>,
    context: Option<&str>,
    origins: &crate::page::Origins,
    state: &StorageState,
) -> Result<()> {
    if !state.cookies.is_empty() {
        crate::browser::set_context_cookies(browser, context, &state.cookies).await?;
    }
    write_local(browser, context, &state.origins).await?;
    let mut known = origins.lock().unwrap_or_else(|e| e.into_inner());
    known.extend(state.origins.iter().map(|o| o.origin.clone()));
    Ok(())
}

/// Reads the localStorage of each of `origins` in `context`.
pub(crate) async fn read_local(
    browser: &Arc<Inner>,
    context: Option<&str>,
    origins: &[String],
) -> Result<Vec<OriginStorage>> {
    let mut out = Vec::new();
    for origin in origins {
        let items = at_origin(
            browser,
            context,
            origin,
            "Object.entries(localStorage)".into(),
        )
        .await?;
        let local_storage: Vec<(String, String)> = items
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pair| Some((pair[0].as_str()?.to_string(), pair[1].as_str()?.to_string())))
            .collect();
        if !local_storage.is_empty() {
            out.push(OriginStorage {
                origin: origin.clone(),
                local_storage,
            });
        }
    }
    Ok(out)
}

/// Fills each origin's localStorage in `context`.
pub(crate) async fn write_local(
    browser: &Arc<Inner>,
    context: Option<&str>,
    origins: &[OriginStorage],
) -> Result<()> {
    for o in origins.iter().filter(|o| !o.local_storage.is_empty()) {
        let items = serde_json::to_string(&o.local_storage).unwrap_or_else(|_| "[]".into());
        let js =
            format!("{{ for (const [k, v] of {items}) localStorage.setItem(k, v); return true; }}");
        at_origin(browser, context, &o.origin, format!("(() => {js})()")).await?;
    }
    Ok(())
}

/// Runs `js` in a page of `context` at `origin`, opened for it and closed
/// after. Nothing it asks for leaves Chrome: every request is answered
/// with an empty page, so the site never sees it.
async fn at_origin(
    browser: &Arc<Inner>,
    context: Option<&str>,
    origin: &str,
    js: String,
) -> Result<Value> {
    if !(origin.starts_with("https://") || origin.starts_with("http://")) {
        return Err(Error::Invalid(format!(
            "localStorage is kept for http and https origins only, not {origin:?}"
        )));
    }
    let page = Page::open(
        browser.clone(),
        context.map(str::to_string),
        false,
        None,
        None,
    )
    .await?;
    let result = async {
        let mut routes = page.route().await?;
        tokio::spawn(async move {
            while let Some(route) = routes.recv().await {
                let _ = route
                    .fulfill(
                        200,
                        &[("Content-Type".into(), "text/html".into())],
                        // An icon of its own, so Chrome asks the site for none.
                        b"<!doctype html><link rel=icon href=\"data:,\"><title></title>",
                    )
                    .await;
            }
        });
        let origin = origin.trim_end_matches('/');
        page.goto(&format!("{origin}/"), WaitUntil::Load, None)
            .await?;
        landed(origin, page.evaluate("location.origin").await?.as_str())?;
        page.evaluate(&js).await
    }
    .await;
    let _ = page.close().await;
    result
}

/// Whether the page asked for `origin` is at it (`at`, its
/// `location.origin`): HSTS or HTTPS-Upgrades can move an http origin to
/// https, whose localStorage is another's.
fn landed(origin: &str, at: Option<&str>) -> Result<()> {
    match at {
        Some(at) if at.eq_ignore_ascii_case(origin) => Ok(()),
        at => Err(Error::Invalid(format!(
            "localStorage for {origin} can't be reached: Chrome took the page to {}, \
             a different origin (HSTS or HTTPS-Upgrades can do that to an http one)",
            at.unwrap_or("nowhere")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localstorage_is_only_read_at_the_origin_asked_for() {
        assert!(landed("http://shop.example", Some("http://shop.example")).is_ok());
        assert!(landed("http://Shop.Example", Some("http://shop.example")).is_ok());
        let moved = landed("http://shop.example", Some("https://shop.example")).unwrap_err();
        assert!(
            moved.to_string().contains("https://shop.example"),
            "{moved}"
        );
        assert!(landed("http://shop.example", None).is_err());
    }

    #[test]
    fn playwrights_json_and_netweirs_keys_read_the_same() {
        let playwright = json!({
            "cookies": [{"name": "sid", "value": "1", "domain": ".shop.example", "path": "/",
                         "expires": -1, "httpOnly": true, "secure": true, "sameSite": "Lax"}],
            "origins": [{"origin": "https://shop.example",
                         "localStorage": [{"name": "cart", "value": "[1,2]"}]}],
        });
        let state = StorageState::from_json(&playwright).unwrap();
        assert!(state.cookies[0].http_only);
        assert_eq!(state.cookies[0].same_site.as_deref(), Some("Lax"));
        assert_eq!(
            state.origins[0].local_storage,
            [("cart".to_string(), "[1,2]".to_string())]
        );
        assert_eq!(StorageState::from_json(&state.to_json()).unwrap(), state);
        let netweir = json!({"cookies": [{"name": "a", "value": "b", "domain": "e.com",
                                          "http_only": true, "same_site": "Strict",
                                          "partition_key": "https://top.example"}]});
        let c = &StorageState::from_json(&netweir).unwrap().cookies[0];
        assert!(c.http_only);
        assert_eq!(c.path, "/");
        assert_eq!(c.partition_key.as_deref(), Some("https://top.example"));
        assert!(StorageState::from_json(&json!([])).is_err());
        assert!(StorageState::from_json(&json!({"cookies": [{"name": "a"}]})).is_err());
    }

    #[test]
    fn a_saved_state_is_for_its_owners_eyes_only() {
        let dir = std::env::temp_dir().join(format!("netweir-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, "old").unwrap();
        let state = StorageState {
            cookies: Vec::new(),
            origins: vec![OriginStorage {
                origin: "https://e.com".into(),
                local_storage: vec![("k".into(), "v".into())],
            }],
        };
        state.write(&path).unwrap();
        assert_eq!(StorageState::read(&path).unwrap(), state);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Nothing left behind but the file.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saves_to_one_file_at_once_each_land_whole() {
        let dir = std::env::temp_dir().join(format!("netweir-state-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let reader = s.spawn(|| {
                let mut torn = 0;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(bytes) = std::fs::read(&path) {
                        let whole = bytes.len() == 1 << 16 && bytes.iter().all(|b| *b == bytes[0]);
                        torn += usize::from(!whole);
                    }
                }
                torn
            });
            let writers: Vec<_> = (0..8u8)
                .map(|i| {
                    let path = &path;
                    s.spawn(move || {
                        let bytes = vec![b'a' + i; 1 << 16];
                        (0..40)
                            .map(|_| write_private(path, &bytes))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let results: Vec<_> = writers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect();
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            let torn = reader.join().unwrap();
            for written in results {
                written.unwrap();
            }
            assert_eq!(torn, 0, "a read found a file half written");
        });
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
