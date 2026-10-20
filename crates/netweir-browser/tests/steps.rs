mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Reply, browser, html, serve};
use netweir_browser::WaitUntil;
use netweir_browser::steps::{self, Call, Status};
use serde_json::{Value, json};

/// A call step heard: its place, and the method it names.
type Asked = (usize, Option<String>);

fn no_calls() -> Box<Call> {
    Box::new(|_, _, _| Box::pin(async { Err("no calls here".to_string()) }))
}

fn json_reply(body: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("Content-Type", "application/json".into())],
        body: body.to_string(),
    }
}

/// A shop page: a cookie banner over everything, a "load more" button
/// that fetches the next items, and a search form that goes to another
/// page.
const SHOP: &str = r#"<!DOCTYPE html>
<style>
  #banner { position: fixed; inset: 0; background: white; }
  li { height: 40px; }
</style>
<div id=banner><button id=accept onclick="this.parentNode.remove(); document.cookie = 'consent=1'">OK</button></div>
<ul id=list><li>one</li></ul>
<button id=more onclick="more()">Load more</button>
<form action=/results><input id=q name=q><select id=size name=size><option value=s>S</option><option value=m>M</option></select><button id=go>Go</button></form>
<script>
  window.keys = 0;
  addEventListener('keydown', () => keys++);
  async function more() {
    const r = await fetch('/api/items');
    for (const name of await r.json()) {
      document.querySelector('#list').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
    }
  }
</script>"#;

#[tokio::test]
async fn steps_click_type_and_wait_with_the_page_own_actions() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        ("/", html(SHOP)),
        ("/api/items", json_reply(r#"["two", "three"]"#)),
        (
            "/results?q=green+tea&size=m",
            html("<title>Results</title><p id=found>found</p>"),
        ),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let list = steps::parse(&json!([
        {"action": "click", "selector": "#accept"},
        {"action": "click", "selector": "#more"},
        {"action": "wait_for_response", "url": "**/api/items"},
        {"action": "wait_for", "selector": "li:nth-child(3)"},
        {"action": "evaluate", "js": "document.querySelectorAll('li').length"},
        {"action": "type", "selector": "#q", "text": "green tea", "delay": [0.01, 0.03]},
        {"action": "evaluate", "js": "keys"},
        {"action": "select_option", "selector": "#size", "value": "m"},
        {"action": "screenshot"},
        {"action": "click", "selector": "#go"},
        {"action": "wait_for_navigation"},
        {"action": "evaluate", "js": "document.title"},
    ]))
    .unwrap();
    let report = steps::run(&page, &list, &*no_calls(), None).await;
    assert_eq!(report.failed, None, "{:?}", report.failure());
    let o = &report.outcomes;
    assert!(o.iter().all(|o| o.status == Status::Ok), "{o:?}");
    assert_eq!(o[0].action, "click");
    let response = o[2].value.as_ref().unwrap();
    assert!(response["url"].as_str().unwrap().ends_with("/api/items"));
    assert_eq!(response["status"], 200);
    assert_eq!(o[4].value, Some(json!(3)));
    assert_eq!(o[6].value, Some(json!(9)), "one keydown a character");
    assert_eq!(o[7].value, Some(json!(["m"])));
    assert!(o[8].image.as_ref().unwrap().starts_with(b"\x89PNG"));
    assert!(
        o[10].value.as_ref().unwrap()["url"]
            .as_str()
            .unwrap()
            .contains("/results?q=green+tea")
    );
    assert_eq!(o[11].value, Some(json!("Results")));
    assert!(o.iter().all(|o| o.elapsed > Duration::ZERO));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_failing_step_continues_stops_or_fails_as_it_says() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(SHOP))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let missing = |on_error: &str| json!({"action": "click", "selector": "#nowhere", "timeout": 0.3, "on_error": on_error});
    let mark = json!({"action": "evaluate", "js": "window.marked = (window.marked || 0) + 1"});
    let run = |list: Value| {
        let page = page.clone();
        async move {
            let list = steps::parse(&list).unwrap();
            steps::run(&page, &list, &*no_calls(), None).await
        }
    };

    let report = run(json!([missing("continue"), mark])).await;
    assert_eq!(report.failed, None);
    assert_eq!(report.outcomes[0].status, Status::Failed);
    assert!(
        report.outcomes[0]
            .error
            .as_deref()
            .unwrap()
            .contains("#nowhere"),
        "{:?}",
        report.outcomes[0]
    );
    assert_eq!(report.outcomes[1].status, Status::Ok);

    let report = run(json!([missing("stop"), mark])).await;
    assert_eq!(report.failed, None);
    assert_eq!(report.outcomes[1].status, Status::Skipped);

    let report = run(json!([mark, missing("fail"), mark])).await;
    assert_eq!(report.failed, Some(1));
    assert_eq!(report.outcomes[2].status, Status::Skipped);
    assert!(
        report
            .failure()
            .unwrap()
            .starts_with("step 2 (click) failed")
    );
    assert_eq!(page.evaluate("window.marked").await.unwrap(), json!(2));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn steps_answer_dialogs_act_in_frames_and_run_calls() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<!DOCTYPE html>
<button id=ask onclick="window.answer = confirm('Sure?')">Delete</button>
<script>setTimeout(() => document.body.insertAdjacentHTML('beforeend', '<iframe name=consent src=/frame></iframe>'), 300)</script>"#,
            ),
        ),
        (
            "/frame",
            html(r#"<button id=agree onclick="parent.agreed = true">Agree</button>"#),
        ),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let asked: Arc<Mutex<Vec<Asked>>> = Arc::default();
    let log = asked.clone();
    let call: Box<Call> = Box::new(move |i, method, _| {
        log.lock().unwrap().push((i, method));
        Box::pin(async { Ok(json!({"logged_in": true})) })
    });
    let list = steps::parse(&json!([
        {"action": "answer_dialogs", "accept": true},
        {"action": "click", "selector": "#ask"},
        {"action": "evaluate", "js": "window.answer"},
        {"action": "click", "selector": "#agree", "frame": {"name": "consent"}},
        {"action": "evaluate", "js": "window.agreed"},
        {"action": "call", "method": "log_in"},
    ]))
    .unwrap();
    let report = steps::run(&page, &list, &*call, None).await;
    assert_eq!(report.failed, None, "{:?}", report.failure());
    assert_eq!(report.outcomes[2].value, Some(json!(true)), "accepted");
    assert_eq!(report.outcomes[4].value, Some(json!(true)));
    assert_eq!(report.outcomes[5].value, Some(json!({"logged_in": true})));
    assert_eq!(*asked.lock().unwrap(), [(5, Some("log_in".to_string()))]);

    // Once the steps are over, dialogs go back to being dismissed.
    page.click("#ask", None).await.unwrap();
    assert_eq!(page.evaluate("window.answer").await.unwrap(), json!(false));

    // A call that's too slow fails at its timeout.
    let slow: Box<Call> = Box::new(|_, _, _| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(Value::Null)
        })
    });
    let list = steps::parse(&json!([{"action": "call", "timeout": 0.2}])).unwrap();
    let report = steps::run(&page, &list, &*slow, None).await;
    assert_eq!(report.failed, Some(0));
    assert!(
        report.outcomes[0]
            .error
            .as_deref()
            .unwrap()
            .contains("took more than")
    );
    browser.close().await.unwrap();
}

/// A field that logs every input event it, and the document, hears.
const LOGGED: &str = r#"<!DOCTYPE html><input id=q value="old" style="width: 300px">
<script>
window.log = [];
for (const t of ['mousedown', 'touchstart', 'keydown', 'keyup', 'input', 'select']) {
  document.addEventListener(t, e => log.push({t, key: e.key, code: e.code, keyCode: e.keyCode,
    shift: e.shiftKey, trusted: e.isTrusted, at: e.timeStamp, data: e.data ?? null}), true);
}
</script>"#;

#[tokio::test]
async fn typing_clicks_holds_keys_and_uses_shift_as_a_person_does() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(LOGGED))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let text = "Ab.@é tea";
    page.type_text(
        "#q",
        text,
        (Duration::from_millis(60), Duration::from_millis(220)),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#q').value")
            .await
            .unwrap(),
        json!(text),
        "what was there is replaced"
    );
    let log: Vec<Value> = serde_json::from_value(page.evaluate("log").await.unwrap()).unwrap();
    let of = |t: &'static str| log.iter().filter(move |e| e["t"] == t);
    assert!(log.iter().all(|e| e["trusted"] == true), "{log:?}");
    // Selected with the keys: the select comes after Control or Command.
    let select = of("select").next().expect("what was there selected")["at"]
        .as_f64()
        .unwrap();
    let modifier = of("keydown")
        .find(|e| matches!(e["key"].as_str(), Some("Control" | "Meta")))
        .expect("Control-A or Command-A")["at"]
        .as_f64()
        .unwrap();
    assert!(modifier <= select);
    // The field is clicked first, and nobody types in the same instant.
    let pressed = of("mousedown").next().expect("a click into the field")["at"]
        .as_f64()
        .unwrap();
    let downs: Vec<&Value> = of("keydown").collect();
    let first = downs.first().unwrap()["at"].as_f64().unwrap();
    assert!(first - pressed >= 150.0, "{}", first - pressed);
    // Control-A or Command-A, then the keys.
    assert!(
        matches!(downs[0]["key"].as_str(), Some("Control" | "Meta")),
        "{:?}",
        downs[0]
    );
    let key = |k: &str| {
        downs
            .iter()
            .find(|e| e["key"] == k)
            .unwrap_or_else(|| panic!("no keydown for {k}: {downs:?}"))
    };
    assert_eq!(
        (
            key("A")["code"].clone(),
            key("A")["keyCode"].clone(),
            key("A")["shift"].clone()
        ),
        (json!("KeyA"), json!(65), json!(true))
    );
    assert_eq!(
        (key("b")["code"].clone(), key("b")["shift"].clone()),
        (json!("KeyB"), json!(false))
    );
    assert_eq!(
        (key(".")["code"].clone(), key(".")["keyCode"].clone()),
        (json!("Period"), json!(190))
    );
    assert_eq!(
        (
            key("@")["code"].clone(),
            key("@")["keyCode"].clone(),
            key("@")["shift"].clone()
        ),
        (json!("Digit2"), json!(50), json!(true))
    );
    assert_eq!(downs.iter().filter(|e| e["key"] == "Shift").count(), 2);
    // Off the keyboard, é comes as text with no key.
    assert!(downs.iter().all(|e| e["key"] != "é"));
    assert!(of("input").any(|e| e["data"] == "é"));
    // Every key is held, for a person's time.
    let ups: Vec<&Value> = of("keyup").collect();
    let printable: Vec<&&Value> = downs
        .iter()
        .filter(|e| e["key"].as_str().unwrap().chars().count() == 1)
        .collect();
    for down in &printable {
        let up = ups
            .iter()
            .find(|u| u["code"] == down["code"] && u["at"].as_f64() >= down["at"].as_f64())
            .unwrap();
        let held = up["at"].as_f64().unwrap() - down["at"].as_f64().unwrap();
        assert!(
            (40.0..=200.0).contains(&held),
            "{} held {held} ms",
            down["key"]
        );
    }
    // And the gaps between keys vary, not one fixed pace.
    let at: Vec<f64> = printable
        .iter()
        .map(|e| e["at"].as_f64().unwrap())
        .collect();
    let gaps: Vec<f64> = at.windows(2).map(|w| w[1] - w[0]).collect();
    let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
    let spread = (gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / gaps.len() as f64).sqrt();
    assert!(spread > 5.0, "{gaps:?}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn double_clicks_xpath_and_two_waits_for_one_url() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<!DOCTYPE html><p id=cell ondblclick="this.textContent = 'edited'">cell</p>
<button onclick="fetch('/api/a')">one</button><button id=two onclick="fetch('/api/a')">two</button>"#,
            ),
        ),
        ("/api/a", json_reply("{}")),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let list = steps::parse(&json!([
        {"action": "double_click", "selector": "xpath=//p[@id='cell']"},
        {"action": "evaluate", "js": "document.querySelector('#cell').textContent"},
        {"action": "click", "selector": "xpath=//button[1]"},
        {"action": "wait_for_response", "url": "**/api/a"},
        // The first response was taken: this one needs a second.
        {"action": "wait_for_response", "url": "**/api/a", "timeout": 0.5, "on_error": "continue"},
        {"action": "click", "selector": "#two"},
        {"action": "wait_for_response", "url": "**/api/a"},
    ]))
    .unwrap();
    let report = steps::run(&page, &list, &*no_calls(), None).await;
    let statuses: Vec<_> = report.outcomes.iter().map(|o| o.status.as_str()).collect();
    assert_eq!(
        statuses,
        ["ok", "ok", "ok", "ok", "failed", "ok", "ok"],
        "{report:?}"
    );
    assert_eq!(report.outcomes[1].value, Some(json!("edited")));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn the_steps_stop_at_their_cap_whatever_their_on_error() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html("<p>x</p>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let list = steps::parse(&json!([
        {"action": "wait", "seconds": 0.2},
        {"action": "wait_for", "selector": "#never", "on_error": "continue"},
        {"action": "evaluate", "js": "1"},
    ]))
    .unwrap();
    let started = std::time::Instant::now();
    let report = steps::run(&page, &list, &*no_calls(), Some(Duration::from_millis(800))).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(report.failed, Some(1), "{report:?}");
    assert!(
        report.failure().unwrap().contains("browser_steps_max"),
        "{report:?}"
    );
    assert_eq!(report.outcomes[2].status, Status::Skipped);
    browser.close().await.unwrap();
}
