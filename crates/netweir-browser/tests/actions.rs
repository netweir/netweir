mod common;

use std::time::Duration;

use common::{Reply, browser, html, serve};
use netweir_browser::{Error, LaunchOptions, Pick, WaitUntil};
use serde_json::json;

const SHORT: Option<Duration> = Some(Duration::from_millis(800));

fn json_reply(body: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("Content-Type", "application/json".into())],
        body: body.to_string(),
    }
}

#[tokio::test]
async fn hover_brings_the_mouse_over_the_element() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<style>
  #menu { width: 100px; height: 30px; color: rgb(0, 0, 255); }
  #menu:hover { color: rgb(255, 0, 0); }
  #wrap { position: relative; width: 100px; height: 30px; }
  #cover { position: absolute; inset: 0; }
</style>
<div style="height: 2000px"></div>
<div id=menu onmouseover="window.over = (window.over || 0) + 1">menu</div>
<div id=wrap><div id=under>under</div><div id=cover></div></div>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.hover("#menu", None).await.unwrap();
    assert_eq!(page.evaluate("over").await.unwrap(), json!(1));
    assert_eq!(
        page.evaluate("getComputedStyle(document.querySelector('#menu')).color")
            .await
            .unwrap(),
        json!("rgb(255, 0, 0)"),
        "the element is in :hover"
    );
    let err = page.hover("#under", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("hover #under") && m.contains("covering")),
        "{err}"
    );
    browser.close().await.unwrap();
}

/// A list that fetches its next page from the server as you near the end,
/// as an infinite feed does, until there's no next page.
const FEED: &str = r#"<!DOCTYPE html>
<style>li { height: 120px; }</style>
<ul id=list></ul>
<script>
  window.wheels = 0;
  addEventListener('wheel', () => wheels++);
  let next = 1, loading = false, done = false;
  async function more() {
    if (loading || done) return;
    loading = true;
    const r = await fetch('/items?page=' + next);
    if (r.ok) {
      for (const name of await r.json()) {
        document.querySelector('#list').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
      }
      next++;
    } else {
      done = true;
    }
    loading = false;
  }
  addEventListener('scroll', () => {
    if (scrollY + innerHeight > document.documentElement.scrollHeight - 300) more();
  });
  more();
</script>"#;

fn feed() -> common::Server {
    let page = |n: usize| {
        let names: Vec<String> = (0..10)
            .map(|i| format!("\"item {}\"", n * 10 + i))
            .collect();
        json_reply(&format!("[{}]", names.join(",")))
    };
    serve(vec![
        ("/", html(FEED)),
        ("/items?page=1", page(0)),
        ("/items?page=2", page(1)),
        ("/items?page=3", page(2)),
    ])
}

#[tokio::test]
async fn scrolling_to_the_bottom_loads_an_endless_list_until_it_ends() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = feed();
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.wait_for("li", "visible", None).await.unwrap();
    let grew = page
        .scroll(None, 0.0, None, None, Duration::from_secs(2), None)
        .await
        .unwrap();
    assert_eq!(grew, 2, "pages 2 and 3 came; page 4 didn't");
    assert_eq!(
        page.evaluate("document.querySelectorAll('li').length")
            .await
            .unwrap(),
        json!(30)
    );
    assert!(
        server
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| p == "/items?page=4"),
        "it scrolled until the list ran out"
    );
    let wheels = page.evaluate("wheels").await.unwrap();
    assert!(wheels.as_u64().unwrap() > 0, "it used the wheel: {wheels}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn scrolling_stops_after_as_many_times_as_asked() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = feed();
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.wait_for("li", "visible", None).await.unwrap();
    let grew = page
        .scroll(None, 0.0, None, Some(1), Duration::from_secs(2), None)
        .await
        .unwrap();
    assert_eq!(grew, 1);
    assert_eq!(
        page.evaluate("document.querySelectorAll('li').length")
            .await
            .unwrap(),
        json!(20)
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn scroll_brings_elements_into_view_and_moves_by_a_distance() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<div style="height: 3000px"></div>
<p id=far>far</p>
<div id=box style="height: 200px; overflow: auto">
  <div style="height: 2000px"></div>
  <p id=deep>deep</p>
</div>
<div style="height: 3000px"></div>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let in_view = |id: &str| {
        format!(
            "(() => {{ const r = document.querySelector('#{id}').getBoundingClientRect(); \
             return r.top >= 0 && r.bottom <= innerHeight; }})()"
        )
    };
    page.scroll(Some("#far"), 0.0, None, None, Duration::ZERO, None)
        .await
        .unwrap();
    assert_eq!(page.evaluate(&in_view("far")).await.unwrap(), json!(true));
    page.scroll(Some("#deep"), 0.0, None, None, Duration::ZERO, None)
        .await
        .unwrap();
    assert_eq!(page.evaluate(&in_view("deep")).await.unwrap(), json!(true));
    assert!(
        page.evaluate("document.querySelector('#box').scrollTop")
            .await
            .unwrap()
            .as_f64()
            .unwrap()
            > 1000.0,
        "the box scrolled, not only the page"
    );

    page.evaluate("scrollTo(0, 0)").await.unwrap();
    page.scroll(None, 0.0, Some(400.0), None, Duration::ZERO, None)
        .await
        .unwrap();
    let y = page.evaluate("scrollY").await.unwrap().as_f64().unwrap();
    assert!((300.0..=500.0).contains(&y), "{y}");

    let err = page
        .scroll(Some("#nothing"), 0.0, None, None, Duration::ZERO, SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("nothing matched")),
        "{err}"
    );
    browser.close().await.unwrap();
}

const FORM: &str = r#"<!DOCTYPE html>
<script>
  window.heard = [];
  addEventListener('input', (e) => heard.push('input ' + e.target.id), true);
  addEventListener('change', (e) => heard.push('change ' + e.target.id), true);
</script>
<select id=one>
  <option value=a>Apple</option>
  <option value=b>Banana</option>
  <option value=c disabled>Cherry</option>
</select>
<select id=many multiple>
  <option value=a>Apple</option>
  <option value=b>Banana</option>
  <option value=c>Cherry</option>
</select>
<input id=box type=checkbox onclick="window.clicks = (window.clicks || 0) + 1">
<label for=labelled>labelled</label><input id=labelled type=checkbox>
<input id=stubborn type=checkbox onclick="event.preventDefault()">
<input id=r1 type=radio name=r><input id=r2 type=radio name=r>
<input id=text>
<input id=file type=file>
<input id=files type=file multiple>
<script>
  document.querySelector('#file').addEventListener('change', async (e) => {
    const f = e.target.files[0];
    window.uploaded = f ? [f.name, await f.text()] : null;
  });
</script>"#;

#[tokio::test]
async fn select_option_chooses_by_value_label_or_index_and_tells_the_page() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(FORM))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let values = |v: &[&str]| Pick::Values(v.iter().map(|s| s.to_string()).collect());

    assert_eq!(
        page.select_option("#one", &values(&["b"]), None)
            .await
            .unwrap(),
        ["b"]
    );
    assert_eq!(
        page.evaluate("heard").await.unwrap(),
        json!(["input one", "change one"])
    );
    // Choosing what's chosen already changes nothing, so the page hears
    // nothing, as with a person.
    page.select_option("#one", &values(&["b"]), None)
        .await
        .unwrap();
    assert_eq!(page.evaluate("heard.length").await.unwrap(), json!(2));
    assert_eq!(
        page.select_option("#one", &Pick::Labels(vec!["Apple".into()]), None)
            .await
            .unwrap(),
        ["a"]
    );
    assert_eq!(
        page.select_option("#one", &Pick::Indexes(vec![1]), None)
            .await
            .unwrap(),
        ["b"]
    );
    assert_eq!(
        page.select_option("#many", &values(&["a", "c"]), None)
            .await
            .unwrap(),
        ["a", "c"]
    );
    assert_eq!(
        page.evaluate("[...document.querySelector('#many').selectedOptions].map(o => o.value)")
            .await
            .unwrap(),
        json!(["a", "c"])
    );

    let err = page
        .select_option("#one", &values(&["zz"]), SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains(r#"no option has the value "zz""#)),
        "{err}"
    );
    let err = page
        .select_option("#one", &values(&["c"]), SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("disabled")),
        "{err}"
    );
    let err = page
        .select_option("#one", &values(&["a", "b"]), SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("takes one option")),
        "{err}"
    );
    let err = page
        .select_option("#text", &values(&["a"]), SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("#text") && m.contains("isn't a <select>")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn check_and_uncheck_click_only_when_the_state_differs() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(FORM))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let checked = |id: &str| format!("document.querySelector('#{id}').checked");

    page.set_checked("#box", true, None).await.unwrap();
    page.set_checked("#box", true, None).await.unwrap();
    assert_eq!(page.evaluate(&checked("box")).await.unwrap(), json!(true));
    assert_eq!(page.evaluate("clicks").await.unwrap(), json!(1));
    page.set_checked("#box", false, None).await.unwrap();
    page.set_checked("#box", false, None).await.unwrap();
    assert_eq!(page.evaluate(&checked("box")).await.unwrap(), json!(false));
    assert_eq!(page.evaluate("clicks").await.unwrap(), json!(2));

    page.set_checked("label[for=labelled]", true, None)
        .await
        .unwrap();
    assert_eq!(
        page.evaluate(&checked("labelled")).await.unwrap(),
        json!(true)
    );

    page.set_checked("#r1", true, None).await.unwrap();
    page.set_checked("#r2", true, None).await.unwrap();
    assert_eq!(page.evaluate(&checked("r1")).await.unwrap(), json!(false));
    let err = page.set_checked("#r2", false, SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("radio")),
        "{err}"
    );

    let err = page
        .set_checked("#stubborn", true, SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("didn't check it")),
        "{err}"
    );
    let err = page.set_checked("#text", true, SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("isn't a checkbox")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn set_input_files_gives_a_file_input_its_files() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(FORM))]);
    let dir = std::env::temp_dir().join(format!("netweir-upload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
    std::fs::write(&a, "first file").unwrap();
    std::fs::write(&b, "second").unwrap();
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();

    page.set_input_files("#file", std::slice::from_ref(&a), None)
        .await
        .unwrap();
    let read = "new Promise(r => { const t = () => window.uploaded ? r(uploaded) : setTimeout(t, 10); t(); })";
    assert_eq!(
        page.evaluate(read).await.unwrap(),
        json!(["a.txt", "first file"])
    );
    assert_eq!(
        page.evaluate("heard").await.unwrap(),
        json!(["input file", "change file"])
    );

    page.set_input_files("#files", &[a.clone(), b.clone()], None)
        .await
        .unwrap();
    assert_eq!(
        page.evaluate("[...document.querySelector('#files').files].map(f => f.name)")
            .await
            .unwrap(),
        json!(["a.txt", "b.txt"])
    );
    page.set_input_files("#files", &[], None).await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#files').files.length")
            .await
            .unwrap(),
        json!(0)
    );

    let err = page
        .set_input_files("#file", &[a.clone(), b.clone()], SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("takes one file")),
        "{err}"
    );
    let err = page
        .set_input_files("#file", &[dir.join("missing.txt")], SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Invalid(m) if m.contains("missing.txt")),
        "{err}"
    );
    let err = page
        .set_input_files("#text", std::slice::from_ref(&a), SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("isn't an <input type=file>")),
        "{err}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
    browser.close().await.unwrap();
}

const INNER: &str = r#"<!DOCTYPE html><title>inner</title>
<script>window.where = 'inner'; window.clicks = 0;</script>
<style>#h:hover { color: rgb(255, 0, 0); }</style>
<button id=b onclick="clicks++">b</button>
<input id=t>
<select id=s><option value=a>A</option><option value=b>B</option></select>
<input id=c type=checkbox>
<div id=h onmouseover="window.hovered = true">hover me</div>"#;

/// A page with a frame from its own site, below the fold, and one from
/// another site, which Chrome runs in a process of its own.
const OUTER: &str = r#"<!DOCTYPE html><title>outer</title>
<script>window.where = 'outer'</script>
<div style="height: 1500px"></div>
<iframe name=same src="/inner" width=400 height=300></iframe>
<script>
  document.write(`<iframe name=other src="http://localhost:${location.port}/inner" width=400 height=300></iframe>`);
</script>"#;

#[tokio::test]
async fn frames_from_this_site_and_others_can_be_read_and_acted_on() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(OUTER)), ("/inner", html(INNER))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let frames = page.frames().await.unwrap();
    let names: Vec<&str> = frames.iter().map(|f| f.name()).collect();
    assert_eq!(names, ["", "same", "other"], "the page's own frame first");
    assert!(frames[0].parent_id().is_none());
    assert!(
        frames[2].url().starts_with("http://localhost:"),
        "{}",
        frames[2].url()
    );
    assert_eq!(
        page.evaluate("(() => { try { return !frames[1].document } catch { return true } })()")
            .await
            .unwrap(),
        json!(true),
        "the other frame is cross-origin"
    );

    for frame in &frames[1..] {
        let name = frame.name();
        assert_eq!(frame.title().await.unwrap(), "inner", "{name}");
        assert!(
            frame.content().await.unwrap().contains("hover me"),
            "{name}"
        );
        assert_eq!(
            frame.evaluate("where").await.unwrap(),
            json!("inner"),
            "{name}"
        );
        frame.click("#b", None).await.unwrap();
        frame.click("#b", None).await.unwrap();
        assert_eq!(frame.evaluate("clicks").await.unwrap(), json!(2), "{name}");
        frame.fill("#t", "typed", None).await.unwrap();
        assert_eq!(
            frame
                .evaluate("document.querySelector('#t').value")
                .await
                .unwrap(),
            json!("typed"),
            "{name}"
        );
        assert_eq!(
            frame
                .select_option("#s", &Pick::Values(vec!["b".into()]), None)
                .await
                .unwrap(),
            ["b"],
            "{name}"
        );
        frame.set_checked("#c", true, None).await.unwrap();
        frame.hover("#h", None).await.unwrap();
        assert_eq!(
            frame.evaluate("hovered").await.unwrap(),
            json!(true),
            "{name}"
        );
        frame.wait_for("#b", "visible", None).await.unwrap();
        let err = frame.click("#missing", SHORT).await.unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{name}: {err}");
    }
    // Clicks in the frames reached nothing in the page.
    assert_eq!(page.evaluate("where").await.unwrap(), json!("outer"));
    let sent = browser.methods_sent();
    for forbidden in [
        "Runtime.enable",
        "Console.enable",
        "Log.enable",
        "DOM.enable",
    ] {
        assert!(!sent.iter().any(|m| m == forbidden), "{forbidden} was sent");
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_given_profile_keeps_its_cookies_between_launches_and_stays() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let server = serve(vec![(
        "/",
        Reply {
            status: 200,
            headers: vec![
                ("Content-Type", "text/html".into()),
                ("Set-Cookie", "kept=yes; Path=/; Max-Age=3600".into()),
            ],
            body: "<p>hi</p>".into(),
        },
    )]);
    let dir = std::env::temp_dir().join(format!("netweir-kept-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let options = LaunchOptions {
        executable: Some(executable),
        user_data_dir: Some(dir.clone()),
        ..LaunchOptions::default()
    };
    let first = common::launch(options.clone()).await;
    assert_eq!(first.profile_dir(), Some(dir.as_path()));
    let page = first.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    first.close().await.unwrap();
    drop(first);
    assert!(dir.is_dir(), "the profile was kept");

    let second = common::launch(options).await;
    let page = second.new_page().await.unwrap();
    let cookies = page.cookies().await.unwrap();
    assert!(
        cookies.iter().any(|c| c.name == "kept" && c.value == "yes"),
        "{cookies:?}"
    );
    second.close().await.unwrap();
    drop(second);
    assert!(dir.is_dir());
    // Chrome's helpers can hold files a moment after it exits.
    for _ in 0..100 {
        if std::fs::remove_dir_all(&dir).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits until the frame named `name` shows a URL that `ready` accepts.
async fn frame_reaches(page: &netweir_browser::Page, name: &str, ready: impl Fn(&str) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let frames = page.frames().await.unwrap();
        if frames.iter().any(|f| f.name() == name && ready(f.url())) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the frame never got there"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_frame_is_followed_when_it_moves_to_another_site_and_back() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(r#"<!DOCTYPE html><iframe name=f src="/inner" width=400 height=300></iframe>"#),
        ),
        ("/inner", html(INNER)),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let frames = page.frames().await.unwrap();
    let frame = frames.iter().find(|f| f.name() == "f").unwrap().clone();
    frame.click("#b", None).await.unwrap();
    assert_eq!(frame.evaluate("clicks").await.unwrap(), json!(1));

    page.evaluate(
        "document.querySelector('iframe').src = `http://localhost:${location.port}/inner`",
    )
    .await
    .unwrap();
    frame_reaches(&page, "f", |u| u.starts_with("http://localhost:")).await;
    frame.click("#b", None).await.unwrap();
    assert_eq!(
        frame.evaluate("[location.hostname, clicks]").await.unwrap(),
        json!(["localhost", 1])
    );

    page.evaluate("document.querySelector('iframe').src = '/inner'")
        .await
        .unwrap();
    frame_reaches(&page, "f", |u| u.starts_with("http://127.0.0.1:")).await;
    frame.click("#b", None).await.unwrap();
    assert_eq!(
        frame.evaluate("[location.hostname, clicks]").await.unwrap(),
        json!(["127.0.0.1", 1])
    );

    page.evaluate("document.querySelector('iframe').remove()")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let err = frame.click("#b", None).await.unwrap_err();
    assert!(matches!(&err, Error::Gone(_)), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a removed frame fails at once"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn scrolling_to_the_bottom_gets_past_a_box_in_the_middle_of_the_page() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<div style="height: 5000px"></div>
<div id=box style="position: fixed; left: 50%; top: 50%; transform: translate(-50%, -50%);
  width: 600px; height: 400px; overflow: auto"><div style="height: 3000px"></div></div>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.scroll(None, 0.0, None, None, Duration::from_millis(300), None)
        .await
        .unwrap();
    assert_eq!(
        page.evaluate("scrollY + innerHeight >= document.documentElement.scrollHeight - 1")
            .await
            .unwrap(),
        json!(true)
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn the_wheel_turns_a_notch_at_a_time() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<script>
  window.biggest = 0;
  addEventListener('wheel', (e) => biggest = Math.max(biggest, Math.abs(e.deltaY)));
</script>
<div style="height: 20000px"></div><p id=far>far</p>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.scroll(Some("#far"), 0.0, None, None, Duration::ZERO, None)
        .await
        .unwrap();
    page.evaluate("scrollTo(0, 0)").await.unwrap();
    page.scroll(None, 0.0, None, None, Duration::from_millis(200), None)
        .await
        .unwrap();
    let biggest = page.evaluate("biggest").await.unwrap().as_f64().unwrap();
    assert!(biggest > 0.0 && biggest <= 120.0, "{biggest}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn timeout_bounds_the_whole_scroll_to_the_bottom() {
    let Some(browser) = browser().await else {
        return;
    };
    // Grows forever: every time you near the end, more is added.
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html><div id=feed style="height: 3000px"></div>
<script>
  addEventListener('scroll', () => {
    if (scrollY + innerHeight > document.documentElement.scrollHeight - 300) {
      setTimeout(() => feed.style.height = (feed.offsetHeight + 2000) + 'px', 50);
    }
  });
</script>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let started = std::time::Instant::now();
    let err = page
        .scroll(
            None,
            0.0,
            None,
            None,
            Duration::from_secs(2),
            Some(Duration::from_secs(3)),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::ScrollTimeout { message, rounds } if message.contains("bottom") && *rounds >= 1),
        "{err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn checking_waits_for_the_box_and_says_to_use_its_label() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<label id=styled><input id=hidden type=checkbox style="display: none"><span>fancy</span></label>
<label for=later>later</label><input id=later type=checkbox disabled>
<script>setTimeout(() => document.querySelector('#later').disabled = false, 300)</script>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let err = page.set_checked("#hidden", true, SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.starts_with("timed out: check #hidden") || m.starts_with("check #hidden")),
        "{err}"
    );
    assert!(err.to_string().contains("label"), "{err}");
    page.set_checked("#styled", true, None).await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#hidden').checked")
            .await
            .unwrap(),
        json!(true)
    );
    page.set_checked("label[for=later]", true, None)
        .await
        .unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#later').checked")
            .await
            .unwrap(),
        json!(true)
    );
    let err = page
        .set_input_files("#hidden", &[std::env::temp_dir()], SHORT)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Invalid(m) if m.contains("directory")),
        "{err}"
    );
    browser.close().await.unwrap();
}

/// Whether the page is scrolled to its bottom.
const AT_BOTTOM: &str = "scrollY + innerHeight >= document.documentElement.scrollHeight - 1";

#[tokio::test]
async fn scrolling_to_the_bottom_gets_past_a_tall_frame_that_scrolls_itself() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<!DOCTYPE html><style>body { margin: 0 }</style>
<div style="height: 1200px"></div>
<iframe src="/long" style="display: block; width: 100%; height: 700px; border: 0"></iframe>
<div style="height: 3000px"></div>"#,
            ),
        ),
        ("/long", html(r#"<div style="height: 4000px">long</div>"#)),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.scroll(None, 0.0, None, None, Duration::from_millis(300), None)
        .await
        .unwrap();
    assert_eq!(page.evaluate(AT_BOTTOM).await.unwrap(), json!(true));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn scrolling_to_the_bottom_gets_past_a_box_that_keeps_its_scroll_to_itself() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html><style>body { margin: 0 }</style>
<div style="height: 1200px"></div>
<div style="height: 300px; overflow: auto; overscroll-behavior: contain">
  <div style="height: 3000px">boxed</div>
</div>
<div style="height: 3000px"></div>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.scroll(None, 0.0, None, None, Duration::from_millis(300), None)
        .await
        .unwrap();
    assert_eq!(page.evaluate(AT_BOTTOM).await.unwrap(), json!(true));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn times_is_the_limit_even_past_the_browsers_timeout() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        timeout: Duration::from_secs(3),
        ..LaunchOptions::default()
    })
    .await;
    // Each page of the feed takes a second and a half to come.
    let slow = |n: usize| {
        let names: Vec<String> = (0..10)
            .map(|i| format!("\"item {}\"", n * 10 + i))
            .collect();
        let mut reply = json_reply(&format!("[{}]", names.join(",")));
        reply.headers.push(("X-Delay-Ms", "1500".into()));
        reply
    };
    let server = serve(vec![
        ("/", html(FEED)),
        (
            "/items?page=1",
            json_reply(
                r#"["item 0", "item 1", "item 2", "item 3", "item 4", "item 5", "item 6", "item 7", "item 8", "item 9"]"#,
            ),
        ),
        ("/items?page=2", slow(1)),
        ("/items?page=3", slow(2)),
        ("/items?page=4", slow(3)),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.wait_for("li", "visible", None).await.unwrap();
    let started = std::time::Instant::now();
    let grew = page
        .scroll(None, 0.0, None, Some(3), Duration::from_secs(4), None)
        .await
        .unwrap();
    assert_eq!(grew, 3);
    assert!(
        started.elapsed() > Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let _ = browser.close().await;
}

#[tokio::test]
async fn a_timeout_on_the_first_trip_down_says_so() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(r#"<div style="height: 200000px"></div>"#))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let err = page
        .scroll(
            None,
            0.0,
            None,
            None,
            Duration::from_secs(1),
            Some(Duration::from_millis(500)),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::ScrollTimeout { message, rounds: 0 } if message.contains("still scrolling down") && !message.contains("growing")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn into_view_moves_the_page_when_the_box_holding_it_runs_out() {
    let Some(browser) = browser().await else {
        return;
    };
    // The box ends just below the window; at its furthest, the element in
    // it has its middle in view and its bottom still out of it.
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html><style>body { margin: 0 }</style>
<div style="height: 3000px"></div>
<div id=box style="height: 200px; overflow: auto">
  <div style="height: 2000px"></div>
  <p id=deep style="height: 40px; margin: 0">deep</p>
</div>
<div style="height: 3000px"></div>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.evaluate("scrollTo(0, box.offsetTop + 200 - innerHeight - 10)")
        .await
        .unwrap();
    page.scroll(Some("#deep"), 0.0, None, None, Duration::ZERO, None)
        .await
        .unwrap();
    let r = page
        .evaluate("(() => { const r = deep.getBoundingClientRect(); return [r.top, r.bottom, innerHeight]; })()")
        .await
        .unwrap();
    let (top, bottom, height) = (
        r[0].as_f64().unwrap(),
        r[1].as_f64().unwrap(),
        r[2].as_f64().unwrap(),
    );
    assert!(top >= 0.0 && bottom <= height, "{r}");
    browser.close().await.unwrap();
}
