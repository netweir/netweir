//! A Firefox page's commands, over WebDriver BiDi. Its page is the same
//! [`Page`] as Chrome's, made by [`open`] instead of a CDP target; what
//! isn't here yet is refused by name (see `Page::chrome_only`).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use super::{ContextRules, Opening, Page, PageEnv, Response, Shared, WaitUntil, lock_state};
use crate::browser::{Guard, Inner, str_of};
use crate::conn::{GONE, Handler};
use crate::environment::Environment;
use crate::{Error, Result};

/// The sandbox netweir's own scripts run in, apart from the page's.
const SANDBOX: &str = "netweir";

/// Events a page keeps its state by.
const EVENTS: &[&str] = &[
    "browsingContext.load",
    "browsingContext.domContentLoaded",
    "browsingContext.fragmentNavigated",
    "network.responseCompleted",
];

pub(super) async fn open(
    browser: Arc<Inner>,
    context: Option<String>,
    owns_context: bool,
    guard: Option<Guard>,
    shared_context: Option<Arc<ContextRules>>,
    env: Environment,
) -> Result<Page> {
    if guard.is_some() {
        return Err(unsupported("Browser.new_guarded_page"));
    }
    if env != Environment::default() {
        return Err(unsupported("a page's environment"));
    }
    let user_context = match context.clone().or_else(|| browser.default_context.clone()) {
        Some(id) => id,
        None => {
            return Err(Error::Launch(
                "Firefox's session has no user context".into(),
            ));
        }
    };
    // A window, not a tab, so every page is in front, as a person's one tab
    // is: a tab behind another is hidden, which its page can see.
    let created = create_window(&browser, &user_context).await?;
    let target = str_of(&created, "context");
    if let Err(e) = place(&browser, &target).await {
        let _ = browser
            .call("", "browsingContext.close", json!({"context": target}))
            .await;
        return Err(e);
    }
    let opening = Opening {
        context,
        owns_context,
        guard: None,
        shared_context,
        opener: None,
        env: PageEnv::default(),
    };
    let page = Page::start(browser.clone(), target.clone(), target.clone(), opening).await?;
    let subscribed = browser
        .call(
            "",
            "session.subscribe",
            json!({"events": EVENTS, "contexts": [target]}),
        )
        .await;
    if let Err(e) = subscribed {
        let _ = page.close().await;
        return Err(e);
    }
    lock_state(&page.inner.shared).url = "about:blank".into();
    Ok(page)
}

/// Opens a window whose one tab is in `user_context`. On Windows,
/// Firefox's create can fail with "BrowsingContext does no longer exist"
/// though the window opened: the window's first document moved to another
/// process while the create was waiting on the old one. The window is
/// fine, so the new one in `user_context` is taken as the answer; only
/// when none appeared is the create asked again, after a pause, a longer
/// one each time, three times at most.
async fn create_window(browser: &Inner, user_context: &str) -> Result<Value> {
    let mut tries = 0;
    loop {
        let before: Vec<String> = top_level(browser)
            .await?
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        let created = browser
            .call(
                "",
                "browsingContext.create",
                json!({"type": "window", "userContext": user_context}),
            )
            .await;
        match created {
            Err(Error::Protocol { message, .. })
                if message.contains("DiscardedBrowsingContextError") && tries < 3 =>
            {
                tries += 1;
                // 100, 300, then 900 ms: Firefox settling after a window.
                tokio::time::sleep(std::time::Duration::from_millis(100 * 3u64.pow(tries - 1)))
                    .await;
                let new: Vec<(String, String)> = top_level(browser)
                    .await?
                    .into_iter()
                    .filter(|(c, _)| !before.contains(c))
                    .collect();
                if let [(context, uc)] = &new[..]
                    && uc == user_context
                {
                    return Ok(json!({"context": context, "userContext": uc}));
                }
                for (left, _) in new {
                    let _ = browser
                        .call("", "browsingContext.close", json!({"context": left}))
                        .await;
                }
            }
            created => return created,
        }
    }
}

/// Every top-level browsing context, with its user context.
async fn top_level(browser: &Inner) -> Result<Vec<(String, String)>> {
    let tree = browser
        .call("", "browsingContext.getTree", json!({"maxDepth": 0}))
        .await?;
    Ok(tree["contexts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| (str_of(c, "context"), str_of(c, "userContext")))
        .collect())
}

/// Puts the window of the new page `context` where Firefox opened its
/// first: Firefox cascades each new window from the last, somewhere a
/// person's one window isn't.
async fn place(browser: &Inner, context: &str) -> Result<()> {
    let Some(([x, y, width, height], maximized)) = browser.first_window else {
        return Ok(());
    };
    let tree = browser
        .call(
            "",
            "browsingContext.getTree",
            json!({"root": context, "maxDepth": 0}),
        )
        .await?;
    let window = str_of(&tree["contexts"][0], "clientWindow");
    // A maximized window has no frame above its tabs, which a page can
    // measure, so the page's window is maximized when the first was.
    let state = if maximized {
        json!({"clientWindow": window, "state": "maximized"})
    } else {
        json!({"clientWindow": window, "state": "normal",
            "x": x, "y": y, "width": width, "height": height})
    };
    browser
        .call("", "browser.setClientWindowState", state)
        .await
        .map(drop)
}

/// What a Firefox page hears: where its document is, and the responses
/// to its navigations, by navigation id (a document's loader, as Chrome's
/// loader id is).
pub(super) fn handler(shared: Arc<Shared>) -> Handler {
    Arc::new(move |method, params| {
        let mut state = lock_state(&shared);
        match method {
            GONE => state.gone = true,
            "browsingContext.load"
            | "browsingContext.domContentLoaded"
            | "browsingContext.fragmentNavigated" => {
                state.url = str_of(params, "url");
                if let Some(navigation) = params["navigation"].as_str() {
                    state.loader = navigation.to_string();
                }
            }
            "network.responseCompleted" => {
                // Only a navigation's own response: its document's.
                let Some(navigation) = params["navigation"].as_str() else {
                    return;
                };
                let request = &params["request"];
                let response = &params["response"];
                let headers = response["headers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|h| (str_of(h, "name"), str_of(&h["value"], "value")))
                    .collect();
                state.responses.insert(
                    navigation.to_string(),
                    Response {
                        url: str_of(response, "url"),
                        status: response["status"].as_u64().unwrap_or(0) as u16,
                        headers,
                        loader: navigation.to_string(),
                        request_id: str_of(request, "request"),
                        session: String::new(),
                        method: str_of(request, "method"),
                        resource_type: "document".into(),
                    },
                );
            }
            _ => return,
        }
        drop(state);
        shared.changed.notify_waiters();
    })
}

pub(super) async fn goto(
    page: &Page,
    url: &str,
    wait: WaitUntil,
    timeout: Option<Duration>,
) -> Result<Response> {
    let wait = match wait {
        WaitUntil::Load => "complete",
        WaitUntil::DomContentLoaded => "interactive",
        WaitUntil::NetworkIdle => return Err(unsupported("WaitUntil::NetworkIdle")),
    };
    if page.inner.closed.load(Ordering::Relaxed) {
        return Err(Error::PageClosed);
    }
    let limit = page.timeout(timeout);
    let navigate = page.inner.browser.conn.call(
        "",
        "browsingContext.navigate",
        json!({"context": page.inner.target, "url": url, "wait": wait}),
    );
    let navigated = tokio::time::timeout(limit, navigate)
        .await
        .map_err(|_| {
            Error::Timeout(format!(
                "navigating to {url} didn't reach {wait} within {}s",
                limit.as_secs_f64()
            ))
        })?
        .map_err(|e| match e {
            Error::Protocol { message, .. } => Error::Navigation {
                url: url.to_string(),
                error: message,
                said: None,
            },
            e => e,
        })?;
    let navigation = str_of(&navigated, "navigation");
    let mut state = page.state();
    let response = state.responses.get(&navigation).cloned();
    // Only the document showing now: one response per navigation would
    // pile up for as long as the page is open.
    state.responses.retain(|n, _| *n == navigation);
    state.loader = navigation.clone();
    state.url = match &response {
        Some(r) => r.url.clone(),
        None => str_of(&navigated, "url"),
    };
    let url = state.url.clone();
    drop(state);
    Ok(response.unwrap_or_else(|| Response::bare(url, navigation)))
}

/// Runs `body` as an async function in the page's main frame; see
/// [`super::firefox_call`].
pub(super) async fn call(page: &Page, body: &str, sandboxed: bool) -> Result<Value> {
    if page.inner.closed.load(Ordering::Relaxed) {
        return Err(Error::PageClosed);
    }
    let mut target = json!({"context": page.inner.target});
    if sandboxed {
        target["sandbox"] = Value::from(SANDBOX);
    }
    let r = page
        .inner
        .browser
        .conn
        .call(
            "",
            "script.callFunction",
            json!({
                "functionDeclaration": format!("async () => {{ {body}\n}}"),
                "awaitPromise": true,
                "target": target,
                "resultOwnership": "none",
            }),
        )
        .await?;
    if r["type"] == "exception" {
        let details = &r["exceptionDetails"];
        let message = details["text"]
            .as_str()
            .filter(|t| !t.is_empty())
            .unwrap_or("script error");
        return Err(Error::Script(message.to_string()));
    }
    to_json(&r["result"])
}

pub(super) async fn close(page: &Page) -> Result<()> {
    let inner = &page.inner;
    let browser = &inner.browser;
    let closed = browser
        .call(
            "",
            "browsingContext.close",
            json!({"context": inner.target}),
        )
        .await;
    browser.conn.off(&inner.session);
    if inner.owns_context
        && let Some(id) = &inner.context
    {
        let _ = browser
            .call("", "browser.removeUserContext", json!({"userContext": id}))
            .await;
    }
    match closed {
        Ok(_) | Err(Error::Closed) | Err(Error::Protocol { .. }) => Ok(()),
        Err(e) => Err(e),
    }
}

fn unsupported(feature: &str) -> Error {
    Error::Unsupported {
        engine: "Firefox",
        feature: feature.to_string(),
    }
}

/// A BiDi remote value as the JSON Chrome's evaluate gives for the same
/// value: numbers JSON can't hold, and BigInts, as text; objects as their
/// own properties, undefined ones left out; what has no JSON form of its
/// own (a function, a date, a map, a node) as an empty object.
pub(crate) fn to_json(remote: &Value) -> Result<Value> {
    Ok(match remote["type"].as_str().unwrap_or_default() {
        "undefined" | "null" => Value::Null,
        "string" | "boolean" => remote["value"].clone(),
        "number" => match &remote["value"] {
            // NaN, -0, Infinity and -Infinity come as text.
            Value::String(text) => Value::from(text.as_str()),
            n => n.clone(),
        },
        "bigint" => remote["value"].clone(),
        "array" => Value::Array(
            remote["value"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| match jsonless(v) {
                    true => Ok(Value::Null),
                    false => to_json(v),
                })
                .collect::<Result<_>>()?,
        ),
        "object" => {
            let mut object = serde_json::Map::new();
            for pair in remote["value"].as_array().into_iter().flatten() {
                let (Some(key), Some(value)) = (pair[0].as_str(), pair.get(1)) else {
                    continue;
                };
                if !jsonless(value) {
                    object.insert(key.to_string(), to_json(value)?);
                }
            }
            Value::Object(object)
        }
        _ => Value::Object(serde_json::Map::new()),
    })
}

/// Values left out of an object, and written as null in an array.
fn jsonless(remote: &Value) -> bool {
    remote["type"] == "undefined"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: Value) -> Value {
        json!({"type": "number", "value": v})
    }

    #[test]
    fn remote_values_read_as_chrome_gives_them() {
        assert_eq!(to_json(&n(json!(1))).unwrap(), json!(1));
        for text in ["NaN", "-0", "Infinity", "-Infinity"] {
            assert_eq!(to_json(&n(json!(text))).unwrap(), json!(text));
        }
        assert_eq!(
            to_json(&json!({"type": "bigint", "value": "10"})).unwrap(),
            json!("10")
        );
        assert_eq!(to_json(&json!({"type": "undefined"})).unwrap(), Value::Null);
        let object = json!({"type": "object", "value": [
            ["a", n(json!(2))],
            ["f", {"type": "function"}],
            ["u", {"type": "undefined"}],
            ["list", {"type": "array", "value": [
                {"type": "string", "value": "x"},
                {"type": "function"},
                {"type": "null"},
            ]}],
        ]});
        assert_eq!(
            to_json(&object).unwrap(),
            json!({"a": 2, "f": {}, "list": ["x", {}, null]})
        );
        assert_eq!(
            to_json(&json!({"type": "date", "value": "1970-01-01T00:00:00.000Z"})).unwrap(),
            json!({})
        );
        assert_eq!(
            to_json(&json!({"type": "map", "value": [[n(json!(1)), n(json!(2))]]})).unwrap(),
            json!({})
        );
    }
}
