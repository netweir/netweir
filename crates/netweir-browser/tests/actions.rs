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
    let rounds = page
        .scroll(None, 0.0, None, None, Duration::from_millis(300), None)
        .await
        .unwrap();
    assert_at_bottom(&page, rounds).await;
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
    // Near the end as the page itself scrolls it, with room to spare:
    // Windows stops a few pixels short of what heights alone say.
    const s = document.scrollingElement;
    if (scrollY > s.scrollHeight - s.clientHeight - 100) {
      setTimeout(() => feed.style.height = (feed.offsetHeight + 2000) + 'px', 50);
    }
  });
</script>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    // Two notches from the bottom, so the first round is a moment's work
    // however slow the machine, and every round after it a long way down:
    // the time runs out with at least one round done.
    page.evaluate("scrollTo(0, document.scrollingElement.scrollHeight - document.scrollingElement.clientHeight - 200)")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let outcome = page
        .scroll(
            None,
            0.0,
            None,
            None,
            Duration::from_secs(2),
            Some(Duration::from_secs(3)),
        )
        .await;
    let err = match outcome {
        Ok(rounds) => panic!(
            "finished after {rounds} rounds, though the page never ends: {}",
            scroll_state(&page).await
        ),
        Err(e) => e,
    };
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

/// Fails, saying where everything was, unless the page is scrolled to its
/// bottom: as far as the browser itself will scroll it, found by asking it
/// to go further and seeing it doesn't, not by arithmetic on heights a
/// platform may count differently.
async fn assert_at_bottom(page: &netweir_browser::Page, rounds: u32) {
    let state = scroll_state(page).await;
    assert_eq!(
        state["at_bottom"],
        json!(true),
        "rounds {rounds}: {state}\ntrip:\n{}",
        page.scroll_trace().join("\n")
    );
}

/// Where the page is scrolled to and every height that says how far it
/// can go; `at_bottom` is whether the browser takes it no further.
async fn scroll_state(page: &netweir_browser::Page) -> serde_json::Value {
    page
        .evaluate(
            "(() => { const s = document.scrollingElement, d = document.documentElement, b = document.body; \
             const box = document.querySelector('#box, [style*=contain]'); const before = scrollY; \
             scrollTo(scrollX, 1e9); const after = scrollY; const cs = getComputedStyle(b); \
             return {at_bottom: before === after, before, after, innerWidth, innerHeight, dpr: devicePixelRatio, \
             scrolling: {scrollHeight: s.scrollHeight, clientHeight: s.clientHeight, clientWidth: s.clientWidth}, \
             html: {scrollHeight: d.scrollHeight, offsetHeight: d.offsetHeight}, \
             body: {scrollHeight: b.scrollHeight, offsetHeight: b.offsetHeight, marginTop: cs.marginTop, marginBottom: cs.marginBottom}, \
             box: box && {scrollTop: box.scrollTop, max: box.scrollHeight - box.clientHeight, rect: box.getBoundingClientRect().toJSON()}}; })()",
        )
        .await
        .unwrap()
}

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
    let rounds = page
        .scroll(None, 0.0, None, None, Duration::from_millis(300), None)
        .await
        .unwrap();
    assert_at_bottom(&page, rounds).await;
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
    let rounds = page
        .scroll(None, 0.0, None, None, Duration::from_millis(300), None)
        .await
        .unwrap();
    assert_at_bottom(&page, rounds).await;
    browser.close().await.unwrap();
}

#[tokio::test]
async fn times_is_the_limit_even_past_the_browsers_timeout() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        timeout: Duration::from_secs(6),
        ..LaunchOptions::default()
    })
    .await;
    // Each page of the feed takes 3 s to come, so three of them run
    // past the browser's 6 s. The browser gets 6 s, not 3, as a loaded
    // Windows runner has taken over 3 s to answer one turn of the wheel.
    let slow = |n: usize| {
        let names: Vec<String> = (0..10)
            .map(|i| format!("\"item {}\"", n * 10 + i))
            .collect();
        let mut reply = json_reply(&format!("[{}]", names.join(",")));
        reply.headers.push(("X-Delay-Ms", "3000".into()));
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
    // The browser's 6 s is for scroll, the step under test: the page
    // loading first gets a limit of its own, as a cold Chrome on a loaded
    // Windows runner has taken longer than 3 s to load it.
    let setup = Some(Duration::from_secs(30));
    page.goto(&server.url, WaitUntil::Load, setup)
        .await
        .unwrap();
    page.wait_for("li", "visible", setup).await.unwrap();
    let started = std::time::Instant::now();
    let grew = page
        .scroll(None, 0.0, None, Some(3), Duration::from_secs(6), None)
        .await
        .unwrap();
    assert_eq!(grew, 3);
    assert!(
        started.elapsed() > Duration::from_secs(6),
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

#[tokio::test]
async fn scrolling_to_the_bottom_turns_whole_notches_as_a_mouse_wheel_does() {
    let Some(browser) = browser().await else {
        return;
    };
    // An awkward height, so the last stretch isn't a whole number of
    // notches.
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<script>
  window.deltas = [];
  addEventListener('wheel', (e) => deltas.push(e.deltaY), {passive: true});
</script>
<div style="height: 4321px"></div>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let rounds = page
        .scroll(None, 0.0, None, None, Duration::from_millis(300), None)
        .await
        .unwrap();
    assert_at_bottom(&page, rounds).await;
    let deltas = page.evaluate("deltas").await.unwrap();
    let all: Vec<f64> = deltas
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_f64().unwrap())
        .collect();
    assert!(
        !all.is_empty() && all.iter().all(|d| *d == 100.0),
        "{all:?}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn the_last_pixels_the_wheel_wont_take_are_scrolled_all_the_same() {
    let Some(browser) = browser().await else {
        return;
    };
    // The wheel can't take the page its last few pixels, as on Windows,
    // where the last notch with 8 px to go moved nothing though a script
    // could; something waiting at the very bottom still has to see the
    // page get there.
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<script>
  window.reached = false;
  const bottom = () => {
    const s = document.scrollingElement;
    return s.scrollHeight - s.clientHeight;
  };
  addEventListener('wheel', (e) => {
    if (scrollY > bottom() - 60) e.preventDefault();
  }, {passive: false});
  addEventListener('scroll', () => {
    if (scrollY >= bottom() - 1) reached = true;
  });
</script>
<div style="height: 4321px"></div>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let rounds = page
        .scroll(None, 0.0, None, None, Duration::from_millis(300), None)
        .await
        .unwrap();
    // Read before the check below scrolls by script itself.
    let reached = page.evaluate("reached").await.unwrap();
    assert_at_bottom(&page, rounds).await;
    assert_eq!(reached, json!(true));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn hover_returns_once_the_page_has_seen_the_mouse_arrive() {
    let Some(browser) = browser().await else {
        return;
    };
    // A page whose every frame is busy, so the mouse's arrival is handled
    // late, as on a loaded machine.
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<style>#menu { width: 100px; height: 30px; }</style>
<div style="height: 2000px"></div>
<div id=menu onmouseover="window.over = (window.over || 0) + 1">menu</div>
<script>
  const spin = () => {
    const t = performance.now();
    while (performance.now() - t < 80) {}
    requestAnimationFrame(spin);
  };
  requestAnimationFrame(spin);
</script>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    for _ in 0..5 {
        page.evaluate("window.over = 0").await.unwrap();
        page.evaluate("scrollTo(0, 0)").await.unwrap();
        page.hover("#menu", None).await.unwrap();
        // At least one: the scroll that brings it back under the mouse can
        // have Chrome send one of its own, as for a person whose mouse
        // stays put while the page moves under it.
        let over = page.evaluate("over").await.unwrap().as_u64().unwrap();
        assert!(over >= 1, "{over}");
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn hover_reaches_a_disabled_control_that_needs_no_scrolling() {
    let Some(browser) = browser().await else {
        return;
    };
    // A disabled control can still have a tooltip, though Chrome doesn't
    // count it as under the mouse.
    let server = serve(vec![(
        "/",
        html(r#"<button id=off disabled style="width: 100px; height: 40px">off</button>"#),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.hover("#off", SHORT).await.unwrap();
    browser.close().await.unwrap();
}

#[tokio::test]
async fn hover_sends_mouseover_even_with_the_mouse_already_on_the_spot() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<style>#menu { width: 100px; height: 30px; }</style>
<div id=menu onmouseover="window.over = (window.over || 0) + 1">menu</div>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.hover("#menu", None).await.unwrap();
    for _ in 0..3 {
        // Away from under the mouse and back, without the mouse moving: the
        // next hover lands where the mouse already is.
        let frames = "new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))";
        page.evaluate(&format!(
            "() => {{ menu.style.display = 'none'; return {frames}; }}"
        ))
        .await
        .unwrap();
        page.evaluate(&format!(
            "() => {{ menu.style.display = ''; return {frames}; }}"
        ))
        .await
        .unwrap();
        page.evaluate("window.over = 0").await.unwrap();
        page.hover("#menu", None).await.unwrap();
        assert_eq!(page.evaluate("over").await.unwrap(), json!(1));
    }
    browser.close().await.unwrap();
}

/// A page that says it runs on `system` ("Windows" or "Mac"), in the
/// Chrome running.
async fn page_on(browser: &netweir_browser::Browser, system: &str) -> netweir_browser::Page {
    let major = browser
        .version()
        .split('/')
        .nth(1)
        .and_then(|v| v.split('.').next())
        .unwrap()
        .to_string();
    let os = match system {
        "Windows" => "Windows NT 10.0; Win64; x64",
        _ => "Macintosh; Intel Mac OS X 10_15_7",
    };
    browser
        .new_page_with(netweir_browser::Environment {
            user_agent: Some(format!(
                "Mozilla/5.0 ({os}) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/{major}.0.0.0 Safari/537.36"
            )),
            ..Default::default()
        })
        .await
        .unwrap()
}

/// A field, and every key event the page hears as "type key modifiers",
/// the modifiers as C, S, A and M for Control, Shift, Alt and Meta.
const KEYS: &str = r#"<!DOCTYPE html>
<input id=a><input id=b><input id=c value="one two three">
<script>
  window.heard = [];
  for (const type of ['keydown', 'keyup']) {
    addEventListener(type, e => heard.push(`${type} ${e.key} ` +
      (e.ctrlKey ? 'C' : '') + (e.shiftKey ? 'S' : '') + (e.altKey ? 'A' : '') + (e.metaKey ? 'M' : '')));
  }
</script>"#;

#[tokio::test]
async fn control_or_meta_a_selects_the_field_on_each_system_and_backspace_empties_it() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(KEYS))]);
    for (system, held) in [("Windows", "C"), ("Mac", "M")] {
        let page = page_on(&browser, system).await;
        page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
        page.click("#c", None).await.unwrap();
        page.evaluate("heard.length = 0").await.unwrap();
        page.press("ControlOrMeta+A").await.unwrap();
        let down = page.evaluate("heard[1]").await.unwrap();
        assert_eq!(down, json!(format!("keydown a {held}")), "{system}");
        page.press("Backspace").await.unwrap();
        assert_eq!(
            page.evaluate("document.querySelector('#c').value")
                .await
                .unwrap(),
            json!(""),
            "{system}"
        );
        page.close().await.unwrap();
    }
    // Control-A is select-all off a Mac, as it is on Windows.
    let page = page_on(&browser, "Windows").await;
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#c", None).await.unwrap();
    page.press("Control+A").await.unwrap();
    page.press("Delete").await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#c').value")
            .await
            .unwrap(),
        json!("")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn word_and_line_shortcuts_edit_the_field_as_each_system_does() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(KEYS))]);
    let value = "document.querySelector('#c').value";
    let windows = page_on(&browser, "Windows").await;
    windows
        .goto(&server.url, WaitUntil::Load, None)
        .await
        .unwrap();
    windows.click("#c", None).await.unwrap();
    windows.press("End").await.unwrap();
    windows.press("Control+Backspace").await.unwrap();
    assert_eq!(windows.evaluate(value).await.unwrap(), json!("one two "));
    windows.press("Control+Shift+ArrowLeft").await.unwrap();
    windows.press("Delete").await.unwrap();
    assert_eq!(windows.evaluate(value).await.unwrap(), json!("one "));

    let mac = page_on(&browser, "Mac").await;
    mac.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    mac.click("#c", None).await.unwrap();
    mac.press("End").await.unwrap();
    mac.press("Alt+Backspace").await.unwrap();
    assert_eq!(mac.evaluate(value).await.unwrap(), json!("one two "));
    mac.press("Meta+Shift+ArrowLeft").await.unwrap();
    mac.press("Backspace").await.unwrap();
    assert_eq!(mac.evaluate(value).await.unwrap(), json!(""));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn modifiers_go_down_in_order_and_up_in_reverse_and_the_page_sees_them() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(KEYS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#a", None).await.unwrap();
    page.evaluate("heard.length = 0").await.unwrap();
    page.press("Meta+Shift+ArrowLeft").await.unwrap();
    page.press("Control+Alt+x").await.unwrap();
    assert_eq!(
        page.evaluate("heard").await.unwrap(),
        json!([
            "keydown Meta M",
            "keydown Shift SM",
            "keydown ArrowLeft SM",
            "keyup ArrowLeft SM",
            "keyup Shift M",
            "keyup Meta ",
            "keydown Control C",
            "keydown Alt CA",
            "keydown x CA",
            "keyup x CA",
            "keyup Alt C",
            "keyup Control ",
        ])
    );
    // Shift with a letter types its capital; "+" is a key of its own.
    page.evaluate("heard.length = 0").await.unwrap();
    page.press("Shift+a").await.unwrap();
    page.press("+").await.unwrap();
    page.press("Shift++").await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#a').value")
            .await
            .unwrap(),
        json!("A++")
    );
    assert_eq!(
        page.evaluate("heard[1]").await.unwrap(),
        json!("keydown A S")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn shift_tab_moves_focus_backwards() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(KEYS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#c", None).await.unwrap();
    page.press("Shift+Tab").await.unwrap();
    assert_eq!(
        page.evaluate("document.activeElement.id").await.unwrap(),
        json!("b")
    );
    page.press("Tab").await.unwrap();
    assert_eq!(
        page.evaluate("document.activeElement.id").await.unwrap(),
        json!("c")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn an_unknown_modifier_or_key_is_refused_before_anything_is_pressed() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(KEYS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    for (keys, says) in [
        ("Hyper+a", "unknown modifier \"Hyper\""),
        ("Control+Nope", "unknown key \"Nope\""),
        ("Control+", "unknown key \"\""),
        ("Shift+é", "isn't on the keyboard"),
        ("++", "a modifier's name is missing"),
        ("shift+a", "names are case-sensitive"),
    ] {
        let err = page.press(keys).await.unwrap_err();
        assert!(
            matches!(&err, Error::Invalid(m) if m.contains(says)),
            "{keys}: {err}"
        );
    }
    assert_eq!(page.evaluate("heard").await.unwrap(), json!([]));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn drag_drops_an_item_by_html5_drag_and_drop() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<style>
  #item { width: 60px; height: 30px; background: #ccc; }
  #bin { width: 200px; height: 100px; margin-top: 150px; background: #eee; }
</style>
<div id=item draggable=true>item</div>
<div id=bin></div>
<button id=after onclick="seen.push('click')">after</button>
<script>
  window.seen = [];
  item.addEventListener('dragstart', e => {
    seen.push('dragstart');
    e.dataTransfer.setData('text/plain', 'item-1');
  });
  bin.addEventListener('dragenter', e => { e.preventDefault(); seen.push('dragenter'); });
  bin.addEventListener('dragover', e => {
    e.preventDefault();
    if (seen.at(-1) !== 'dragover') seen.push('dragover');
  });
  bin.addEventListener('drop', e => {
    e.preventDefault();
    seen.push('drop ' + e.dataTransfer.getData('text/plain'));
    bin.appendChild(item);
  });
  item.addEventListener('dragend', () => seen.push('dragend'));
</script>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.drag("#item", "#bin", None).await.unwrap();
    assert_eq!(
        page.evaluate("item.parentElement.id").await.unwrap(),
        json!("bin")
    );
    // The mouse is free again after the drop.
    page.click("#after", None).await.unwrap();
    assert_eq!(
        page.evaluate("seen").await.unwrap(),
        json!([
            "dragstart",
            "dragenter",
            "dragover",
            "drop item-1",
            "dragend",
            "click"
        ])
    );
    let err = page.drag("#item", "#nowhere", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("drag #nowhere")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn drag_moves_a_handle_that_follows_the_mouse() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<style>
  #track { position: relative; width: 300px; height: 20px; margin: 50px; background: #eee; }
  #handle { position: absolute; left: 0; top: 0; width: 20px; height: 20px; background: #333; }
  #mark { position: absolute; left: 290px; top: 100px; width: 20px; height: 20px; }
</style>
<div id=track><div id=handle></div></div>
<div id=mark></div>
<script>
  let grab = null;
  window.moves = 0;
  window.ups = 0;
  handle.addEventListener('mousedown', e => { grab = e.clientX - handle.offsetLeft; });
  addEventListener('mousemove', e => {
    if (grab === null) return;
    moves++;
    handle.style.left = Math.max(0, Math.min(280, e.clientX - grab)) + 'px';
  });
  addEventListener('mouseup', () => { grab = null; ups++; });
</script>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.drag("#handle", "#mark", None).await.unwrap();
    let r = page
        .evaluate(
            "() => { const h = handle.getBoundingClientRect(), m = mark.getBoundingClientRect(); \
             return [Math.round(h.x + h.width / 2 - (m.x + m.width / 2)), moves > 3, ups]; }",
        )
        .await
        .unwrap();
    assert_eq!(
        r,
        json!([0, true, 1]),
        "the handle's centre is on the mark's"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn shift_with_a_digit_or_symbol_presses_the_character_above_it() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(KEYS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#a", None).await.unwrap();
    page.evaluate("heard.length = 0").await.unwrap();
    for keys in ["Shift+1", "Shift+/", "Shift+="] {
        page.press(keys).await.unwrap();
    }
    assert_eq!(
        page.evaluate("document.querySelector('#a').value")
            .await
            .unwrap(),
        json!("!?+")
    );
    assert_eq!(
        page.evaluate("heard[1]").await.unwrap(),
        json!("keydown ! S")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn modifiers_alone_are_pressed_and_released() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(KEYS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#a", None).await.unwrap();
    page.evaluate("heard.length = 0").await.unwrap();
    page.press("Shift").await.unwrap();
    page.press("Control+Shift").await.unwrap();
    assert_eq!(
        page.evaluate("heard").await.unwrap(),
        json!([
            "keydown Shift S",
            "keyup Shift ",
            "keydown Control C",
            "keydown Shift CS",
            "keyup Shift C",
            "keyup Control ",
        ])
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn undo_and_redo_take_back_typing_on_each_system() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(KEYS))]);
    let value = "document.querySelector('#a').value";
    for (system, redo) in [
        ("Windows", "Control+Y"),
        ("Windows", "Control+Shift+Z"),
        ("Mac", "Meta+Shift+Z"),
    ] {
        let page = page_on(&browser, system).await;
        page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
        page.type_text(
            "#a",
            "abc",
            (Duration::from_millis(5), Duration::from_millis(10)),
            None,
        )
        .await
        .unwrap();
        page.press("ControlOrMeta+Z").await.unwrap();
        assert_eq!(page.evaluate(value).await.unwrap(), json!(""), "{system}");
        page.press(redo).await.unwrap();
        assert_eq!(page.evaluate(value).await.unwrap(), json!("abc"), "{redo}");
        page.close().await.unwrap();
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn drag_to_a_target_out_of_view_with_the_source_says_so_and_presses_nothing() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<div id=item draggable=true style="width: 60px; height: 30px">item</div>
<div style="height: 3000px"></div>
<div id=bin style="width: 200px; height: 100px">bin</div>
<script>
  window.seen = [];
  for (const type of ['mousedown', 'dragstart', 'drop']) addEventListener(type, () => seen.push(type));
</script>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let err = page.drag("#item", "#bin", None).await.unwrap_err();
    assert!(
        matches!(&err, Error::Invalid(m) if m.contains("drag #item to #bin") && m.contains("in view")),
        "{err}"
    );
    assert_eq!(page.evaluate("seen").await.unwrap(), json!([]));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_drag_cut_short_is_cancelled_not_left_hanging() {
    let Some(executable) = common::chrome() else {
        return;
    };
    // Chrome's answers wait 5 s at most, so a page that stops answering
    // for longer fails the drag partway. A launch keeps to the same limit,
    // and a loaded macOS runner's Chrome has missed a 1 s one three times.
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .await;
    let server = serve(vec![(
        "/",
        html(
            r#"<!DOCTYPE html>
<div id=item draggable=true style="width: 60px; height: 30px">item</div>
<div id=bin style="width: 200px; height: 100px; margin-top: 150px">bin</div>
<script>
  window.seen = [];
  item.ondragstart = e => { seen.push('dragstart'); e.dataTransfer.setData('text/plain', 'x'); };
  item.ondragend = () => seen.push('dragend');
  // Busy for longer than Chrome's answers wait, the first time.
  let busy = true;
  bin.ondragenter = e => {
    e.preventDefault();
    if (busy) { busy = false; const end = Date.now() + 8000; while (Date.now() < end); }
  };
  bin.ondragover = e => e.preventDefault();
  bin.ondrop = e => { e.preventDefault(); seen.push('drop'); };
</script>"#,
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let err = page
        .drag("#item", "#bin", Some(Duration::from_secs(30)))
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Timeout(_)), "{err}");
    // Once the page answers again, the drag is called off: no drop.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let seen = page.evaluate("seen").await;
        if let Ok(seen) = &seen
            && seen.as_array().is_some_and(|s| s.len() >= 2)
        {
            assert_eq!(*seen, json!(["dragstart", "dragend"]));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the drag was never ended: {seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_frame_drags_within_itself_wherever_it_sits_in_the_page() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                r#"<!DOCTYPE html>
<iframe name=inner src="/inner" style="position: absolute; left: 200px; top: 150px; width: 400px; height: 300px; border: 0"></iframe>"#,
            ),
        ),
        (
            "/inner",
            html(
                r#"<!DOCTYPE html>
<div id=item draggable=true style="width: 60px; height: 30px">item</div>
<div id=bin style="width: 200px; height: 80px; margin-top: 100px">bin</div>
<script>
  item.ondragstart = e => e.dataTransfer.setData('text/plain', 'in a frame');
  bin.ondragover = e => e.preventDefault();
  bin.ondrop = e => { e.preventDefault(); window.dropped = e.dataTransfer.getData('text/plain'); };
</script>"#,
            ),
        ),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    frame_reaches(&page, "inner", |u| u.ends_with("/inner")).await;
    let frame = page
        .frames()
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.name() == "inner")
        .unwrap();
    frame.drag("#item", "#bin", None).await.unwrap();
    assert_eq!(
        frame.evaluate("window.dropped").await.unwrap(),
        json!("in a frame")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn text_and_role_selectors_find_an_element_by_what_it_says() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(concat!(
            "<script>var said = []; var my = 'Hidden gem';</script>",
            "<div style='display:none'><button onclick=\"said.push('hidden')\">Sign  in</button></div>",
            "<button onclick=\"said.push('sign in')\"><span>Sign</span>  <b>in</b></button>",
            "<button onclick=\"said.push('sign in now')\">Sign in now</button>",
            "<input type=submit value='Send it' onclick=\"said.push('input');return false\">",
            "<a href='#x' onclick=\"said.push('link')\">Read more</a>",
            "<div role=button aria-label='Close it' onclick=\"said.push('close')\">x</div>",
            "<a role=button href='#y' onclick=\"said.push('as a button')\">Odd</a>",
            "<div aria-hidden=true><button onclick=\"said.push('aria')\">Unheard</button></div>",
            "<label for=mail>Email</label><input id=mail>",
            "<button disabled id=off><span>Off</span></button>",
            "<h2 onclick=\"said.push('heading')\">Sign in now</h2>",
        )),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let quick = Some(Duration::from_secs(1));
    for selector in [
        "text=sign IN",     // the visible one, across its tags
        "text=\"Sign in\"", // not "Sign in now"
        "text=Sign in now", // the first holding it
        "text=Send it",     // a button input's value
        "role=button[name=\"Sign in\"]",
        "role=button[name=sign in now]",
        "role=button[name=Send]",
        "role=link",
        "role=button[name='Close it']",
        "role=button[name=Odd]", // the attribute beats the tag
        "role=heading[name=Sign in]",
    ] {
        page.click(selector, quick).await.unwrap();
    }
    assert_eq!(
        page.evaluate("said").await.unwrap(),
        serde_json::json!([
            "sign in",
            "sign in",
            "sign in now",
            "input",
            "sign in",
            "sign in now",
            "input",
            "link",
            "close",
            "as a button",
            "heading"
        ])
    );
    page.fill("role=textbox[name=Email]", "ada", quick)
        .await
        .unwrap();
    assert_eq!(
        page.evaluate("mail.value").await.unwrap(),
        serde_json::json!("ada")
    );

    // Script text isn't text; hidden from assistive technology isn't a role;
    // and an <a> with a role of its own isn't a link.
    for selector in [
        "text=Hidden gem",
        "role=button[name=Unheard]",
        "role=link[name=Odd]",
    ] {
        let err = page.click(selector, quick).await.unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{selector}: {err}");
    }
    // The text inside a disabled button isn't something to click.
    let err = page.click("text=Off", quick).await.unwrap_err();
    assert!(err.to_string().contains("enabled"), "{err}");
    // A selector that can't mean anything says so at once.
    for selector in ["text=", "role=button[name=]", "role=but ton"] {
        let err = page.click(selector, None).await.unwrap_err();
        assert!(matches!(err, Error::Script(_)), "{selector}: {err}");
    }
    browser.close().await.unwrap();
}
