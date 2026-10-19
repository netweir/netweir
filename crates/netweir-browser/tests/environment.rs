//! A page's environment, read back from every surface a site can read: the
//! page, a dedicated worker, a frame from another site and that frame's
//! worker, and the headers of every request each of them makes. Then a
//! page that tries the tricks detection scripts use to catch settings
//! that don't agree.

mod common;

use std::time::Duration;

use common::{Reply, html, serve};
use netweir_browser::{
    ColorScheme, Environment, Geolocation, LaunchOptions, ReducedMotion, Size, WaitUntil,
};
use serde_json::{Value, json};

/// Reads what page script can, in a window or a worker.
const READ: &str = r#"
function read() {
  const o = {};
  const t = (k, f) => { try { o[k] = f(); } catch (e) { o[k] = 'threw ' + e.message; } };
  const d = new Date(Date.UTC(2026, 0, 15, 12));
  t('language', () => navigator.language);
  t('languages', () => navigator.languages.join(','));
  t('intlLocale', () => Intl.DateTimeFormat().resolvedOptions().locale);
  t('numberLocale', () => Intl.NumberFormat().resolvedOptions().locale);
  t('timeZone', () => Intl.DateTimeFormat().resolvedOptions().timeZone);
  t('offsets', () => [new Date(2026, 0, 15).getTimezoneOffset(), new Date(2026, 6, 15).getTimezoneOffset()].join('/'));
  t('dateString', () => d.toString());
  t('localeString', () => d.toLocaleString());
  t('userAgent', () => navigator.userAgent);
  t('platform', () => navigator.platform);
  t('uaMobile', () => navigator.userAgentData.mobile);
  t('uaPlatform', () => navigator.userAgentData.platform);
  t('cores', () => navigator.hardwareConcurrency);
  t('onLine', () => navigator.onLine);
  t('init', () => self.initRan || 0);
  if (typeof window !== 'undefined') {
    t('touchPoints', () => navigator.maxTouchPoints);
    t('screen', () => screen.width + 'x' + screen.height);
    t('avail', () => screen.availWidth + 'x' + screen.availHeight);
    t('dpr', () => devicePixelRatio);
    t('inner', () => innerWidth + 'x' + innerHeight);
    t('outer', () => outerWidth + 'x' + outerHeight);
    t('touchEvents', () => 'ontouchstart' in window);
    const mq = (q) => matchMedia(q).matches;
    t('coarse', () => mq('(pointer: coarse)'));
    t('noHover', () => mq('(hover: none)'));
    t('dark', () => mq('(prefers-color-scheme: dark)'));
    t('reduce', () => mq('(prefers-reduced-motion: reduce)'));
    t('deviceSize', () => mq('(device-width: ' + screen.width + 'px)') && mq('(device-height: ' + screen.height + 'px)'));
    t('resolution', () => mq('(resolution: ' + devicePixelRatio + 'dppx)'));
  }
  return o;
}
async function hints() {
  const h = await navigator.userAgentData.getHighEntropyValues(['model', 'platformVersion', 'formFactors']);
  return { model: h.model, platformVersion: h.platformVersion, formFactors: (h.formFactors || []).join(',') };
}
"#;

/// The page: what it reads itself, and what its worker and its frame from
/// another site read, with that frame's own worker. The frame is from
/// localhost, another site than 127.0.0.1, so Chrome runs it in a process
/// of its own.
fn probe() -> String {
    format!(
        r#"<!doctype html><meta name=viewport content="width=device-width"><link rel=stylesheet href=/style.css><script>{READ}
window.early = read();
// Read once loaded: during its first parse, a document Chrome has just
// put in a new process doesn't know its window yet (outerWidth is 0, and
// a frame's media queries can be the defaults), headful or headless.
const loaded = () => new Promise((ok) => document.readyState === 'complete' ? ok() : addEventListener('load', ok))
  .then(() => new Promise((ok) => requestAnimationFrame(() => requestAnimationFrame(ok))));
window.surfaces = (async () => {{
  await loaded();
  const main = read();
  main.hints = await hints();
  const worker = await new Promise((ok) => {{ const w = new Worker('/worker.js'); w.onmessage = (m) => ok(m.data); w.onerror = (e) => ok('worker failed'); }});
  const frame = await new Promise((ok) => {{
    addEventListener('message', (m) => ok(m.data));
    const f = document.createElement('iframe');
    f.src = location.origin.replace('127.0.0.1', 'localhost') + '/frame.html';
    f.allow = 'geolocation';
    document.documentElement.appendChild(f);
  }});
  return {{ main, worker, frame: frame.me, frameWorker: frame.worker }};
}})();
</script><body>"#
    )
}

fn frame_page() -> String {
    format!(
        r#"<!doctype html><script>{READ}
(async () => {{
  await new Promise((ok) => document.readyState === 'complete' ? ok() : addEventListener('load', ok));
  await new Promise((ok) => requestAnimationFrame(() => requestAnimationFrame(ok)));
  const me = read();
  me.hints = await hints();
  const worker = await new Promise((ok) => {{ const w = new Worker('/frame-worker.js'); w.onmessage = (m) => ok(m.data); }});
  parent.postMessage({{ me, worker }}, '*');
}})();
</script>"#
    )
}

fn worker(fetching: &str) -> String {
    format!(
        "{READ}\nfetch('{fetching}').catch(() => {{}}).finally(async () => {{ const o = read(); o.hints = await hints(); postMessage(o); }});"
    )
}

fn js(body: String) -> Reply {
    Reply {
        status: 200,
        headers: vec![("Content-Type", "application/javascript".into())],
        body,
    }
}

fn text(body: &str, kind: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("Content-Type", kind.into())],
        body: body.into(),
    }
}

/// The test server, with the probe page and what it loads.
fn site() -> common::Server {
    let mut page = html(&probe());
    // The model is a high-entropy hint, sent only once asked for.
    page.headers.push((
        "Accept-CH",
        "Sec-CH-UA-Model, Sec-CH-UA-Platform-Version".into(),
    ));
    serve(vec![
        ("/", page),
        ("/frame.html", html(&frame_page())),
        ("/worker.js", js(worker("/worker-fetch"))),
        ("/frame-worker.js", js(worker("/frame-fetch"))),
        ("/worker-fetch", text("x", "text/plain")),
        ("/frame-fetch", text("x", "text/plain")),
        ("/style.css", text("body{}", "text/css")),
        ("/sw.js", js(SERVICE_WORKER.into())),
        ("/sw.html", html(SW_PAGE)),
    ])
}

const SERVICE_WORKER: &str = "self.onmessage = (e) => e.source.postMessage([navigator.language, \
    Intl.DateTimeFormat().resolvedOptions().timeZone, Intl.DateTimeFormat().resolvedOptions().locale]);";

const SW_PAGE: &str = r#"<!doctype html><script>
window.sw = (async () => {
  const reg = await navigator.serviceWorker.register('/sw.js');
  await navigator.serviceWorker.ready;
  return await new Promise((ok) => { navigator.serviceWorker.onmessage = (m) => ok(m.data); reg.active.postMessage('?'); });
})();
</script>"#;

/// The probe pages' server.
struct Probe {
    server: common::Server,
}

impl Probe {
    fn new() -> Probe {
        Probe { server: site() }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server.url)
    }

    /// The header `name` of the request for `path`.
    fn header(&self, path: &str, name: &str) -> Option<String> {
        self.server
            .headers_for(path)
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }
}

/// Loads the probe in `page` and returns what each surface read.
async fn surfaces(page: &netweir_browser::Page, probe: &Probe) -> Value {
    page.goto(&probe.url("/"), WaitUntil::Load, None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), page.evaluate("surfaces"))
        .await
        .expect("the surfaces were never read")
        .unwrap()
}

fn launch_options() -> Option<LaunchOptions> {
    let executable = common::chrome()?;
    let brands: netweir_browser::Brands = std::sync::Arc::new(|major: &str| {
        Some(format!(
            "\"Google Chrome\";v=\"{major}\", \"Chromium\";v=\"{major}\", \"Not)A;Brand\";v=\"24\""
        ))
    });
    Some(LaunchOptions {
        executable: Some(executable),
        brands: Some(brands),
        timeout: Duration::from_secs(30),
        ..Default::default()
    })
}

fn desktop() -> Environment {
    Environment {
        viewport: Some(Size {
            width: 1200,
            height: 700,
        }),
        screen: Some(Size {
            width: 1440,
            height: 900,
        }),
        device_scale_factor: Some(2.0),
        locale: Some("de-DE".into()),
        timezone: Some("Asia/Tokyo".into()),
        color_scheme: Some(ColorScheme::Dark),
        reduced_motion: Some(ReducedMotion::Reduce),
        extra_headers: vec![("X-Extra".into(), "yes".into())],
        init_scripts: vec!["self.initRan = (self.initRan || 0) + 1;".into()],
        ..Default::default()
    }
}

/// Each surface's value of `key`, for those that have it.
fn each<'a>(s: &'a Value, key: &str) -> Vec<(&'static str, &'a Value)> {
    ["main", "worker", "frame", "frameWorker"]
        .into_iter()
        .filter(|w| !s[w][key].is_null())
        .map(|w| (w, &s[w][key]))
        .collect()
}

fn all_are(s: &Value, key: &str, want: Value) {
    let got = each(s, key);
    assert!(!got.is_empty(), "nothing read {key}: {s:#}");
    for (w, v) in got {
        assert_eq!(*v, want, "{key} in the {w}: {s:#}");
    }
}

#[tokio::test]
async fn a_desktop_environment_is_the_same_on_every_surface() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    let page = browser.new_page_with(desktop()).await.unwrap();
    let s = surfaces(&page, &probe).await;

    all_are(&s, "language", json!("de-DE"));
    all_are(&s, "languages", json!("de-DE,de"));
    all_are(&s, "intlLocale", json!("de-DE"));
    all_are(&s, "numberLocale", json!("de-DE"));
    all_are(&s, "timeZone", json!("Asia/Tokyo"));
    all_are(&s, "offsets", json!("-540/-540"));
    let date = s["main"]["dateString"].clone();
    assert!(date.as_str().unwrap().contains("GMT+0900"), "{date}");
    all_are(&s, "dateString", date);
    all_are(&s, "localeString", s["main"]["localeString"].clone());
    // The frame reads the real screen, as the page does.
    all_are(&s, "screen", json!("1440x900"));
    all_are(&s, "dpr", json!(2));
    all_are(&s, "deviceSize", json!(true));
    all_are(&s, "resolution", json!(true));
    all_are(&s, "dark", json!(true));
    all_are(&s, "reduce", json!(true));
    all_are(&s, "coarse", json!(false));
    all_are(&s, "uaMobile", json!(false));
    all_are(&s, "userAgent", s["main"]["userAgent"].clone());
    all_are(&s, "platform", s["main"]["platform"].clone());
    assert_eq!(s["main"]["inner"], "1200x700", "{s:#}");
    // The window is a real one around the viewport, on that screen.
    let outer = s["main"]["outer"].as_str().unwrap().to_string();
    assert_eq!(s["frame"]["outer"], outer.as_str(), "{s:#}");
    let (w, h) = outer.split_once('x').unwrap();
    let (w, h): (u32, u32) = (w.parse().unwrap(), h.parse().unwrap());
    assert!(
        (1200..=1440).contains(&w) && (700..=900).contains(&h),
        "{outer}"
    );
    // Init scripts ran in the page and in the frame from another site,
    // before their own scripts; Chrome has no way to run one in a worker.
    assert_eq!(s["main"]["init"], 1, "{s:#}");
    assert_eq!(s["frame"]["init"], 1, "{s:#}");
    assert_eq!(s["worker"]["init"], 0, "{s:#}");

    for path in [
        "/",
        "/style.css",
        "/worker.js",
        "/worker-fetch",
        "/frame.html",
        "/frame-worker.js",
        "/frame-fetch",
    ] {
        assert_eq!(
            probe.header(path, "accept-language").as_deref(),
            Some("de-DE,de;q=0.9"),
            "{path}"
        );
        assert_eq!(
            probe.header(path, "x-extra").as_deref(),
            Some("yes"),
            "{path}"
        );
    }
    // Chrome's own emulation of metrics was never used for a desktop.
    let sent = browser.methods_sent();
    assert!(
        !sent
            .iter()
            .any(|m| m == "Emulation.setDeviceMetricsOverride"),
        "{sent:?}"
    );
    for forbidden in ["Runtime.enable", "Console.enable", "Log.enable"] {
        assert!(!sent.iter().any(|m| m == forbidden), "{forbidden} was sent");
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_phone_is_a_phone_on_every_surface_and_in_every_header() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    let phone = Environment::device("Galaxy S23").unwrap();
    let page = browser.new_page_with(phone).await.unwrap();
    let s = surfaces(&page, &probe).await;

    all_are(&s, "uaMobile", json!(true));
    all_are(&s, "uaPlatform", json!("Android"));
    // A worker's navigator.platform is the system's whatever CDP says (a
    // known limit; see the design note).
    assert_eq!(s["main"]["platform"], "Linux armv81");
    assert_eq!(s["frame"]["platform"], "Linux armv81");
    all_are(&s, "cores", json!(8));
    all_are(&s, "userAgent", s["main"]["userAgent"].clone());
    let ua = s["main"]["userAgent"].as_str().unwrap();
    assert!(
        ua.contains("Android") && ua.contains("Mobile Safari"),
        "{ua}"
    );
    for w in ["main", "worker", "frame", "frameWorker"] {
        assert_eq!(s[w]["hints"]["model"], "SM-S911B", "{w}: {s:#}");
        assert_eq!(s[w]["hints"]["platformVersion"], "14.0.0", "{w}: {s:#}");
        assert_eq!(s[w]["hints"]["formFactors"], "Mobile", "{w}: {s:#}");
    }
    all_are(&s, "screen", json!("360x780"));
    all_are(&s, "dpr", json!(3));
    all_are(&s, "touchPoints", json!(5));
    all_are(&s, "touchEvents", json!(true));
    all_are(&s, "coarse", json!(true));
    all_are(&s, "noHover", json!(true));
    all_are(&s, "deviceSize", json!(true));
    assert_eq!(s["main"]["inner"], "360x704", "{s:#}");
    assert_eq!(s["main"]["outer"], "360x704", "{s:#}");

    for path in [
        "/",
        "/style.css",
        "/worker.js",
        "/worker-fetch",
        "/frame.html",
        "/frame-worker.js",
        "/frame-fetch",
    ] {
        let agent = probe.header(path, "user-agent").unwrap();
        assert_eq!(agent, ua, "{path}");
        // Chrome sends no client hints with what a worker fetches, its
        // own script included.
        if path.contains("worker") || path.ends_with("-fetch") {
            assert_eq!(probe.header(path, "sec-ch-ua-mobile"), None, "{path}");
            continue;
        }
        assert_eq!(
            probe.header(path, "sec-ch-ua-mobile").as_deref(),
            Some("?1"),
            "{path}"
        );
        assert_eq!(
            probe.header(path, "sec-ch-ua-platform").as_deref(),
            Some("\"Android\""),
            "{path}"
        );
    }
    // Asked for by the page, the model goes with what it loads.
    assert_eq!(
        probe.header("/style.css", "sec-ch-ua-model").as_deref(),
        Some("\"SM-S911B\"")
    );
    browser.close().await.unwrap();
}

/// What a detection script looks for: settings that disagree with each
/// other, with the worker's and the frame's, or that were patched in by
/// script rather than set by the browser.
const DETECT: &str = r#"
(s) => {
  const found = [];
  const m = s.main;
  const flag = (what) => found.push(what);
  // Intl's offset for its zone against Date's.
  const tz = Intl.DateTimeFormat().resolvedOptions().timeZone;
  for (const at of [new Date(2026, 0, 15), new Date(2026, 6, 15)]) {
    const part = new Intl.DateTimeFormat('en-US', { timeZone: tz, timeZoneName: 'longOffset' }).formatToParts(at).find((p) => p.type === 'timeZoneName').value;
    const g = /GMT(?:([+-])(\d\d):(\d\d))?/.exec(part);
    const minutes = g[1] ? (g[1] === '-' ? 1 : -1) * (+g[2] * 60 + +g[3]) : 0;
    if (minutes !== at.getTimezoneOffset()) flag('Intl offset ' + minutes + ' vs Date ' + at.getTimezoneOffset());
  }
  if (!new Date().toString().includes(new Intl.DateTimeFormat('en-US', { timeZone: tz, timeZoneName: 'longOffset' }).formatToParts(new Date()).find((p) => p.type === 'timeZoneName').value.replace(':', '').replace(/^GMT$/, 'GMT+0000'))) flag('Date string zone vs Intl');
  if (navigator.language.split('-')[0] !== m.intlLocale.split('-')[0]) flag('language vs Intl');
  if (navigator.languages[0] !== navigator.language) flag('languages[0] vs language');
  // Screen against media queries and the window.
  if (!m.deviceSize) flag('screen vs device-width');
  if (!m.resolution) flag('devicePixelRatio vs resolution');
  if (outerWidth < innerWidth || outerHeight < innerHeight) flag('outer smaller than inner');
  if (outerWidth > screen.width || outerHeight > screen.height) flag('window bigger than the screen');
  if (screen.availWidth > screen.width || screen.availHeight > screen.height) flag('avail bigger than the screen');
  if (outerWidth === 0) flag('no window');
  // Touch, pointer and the client hints.
  const touch = navigator.maxTouchPoints > 0;
  if (touch !== ('ontouchstart' in window)) flag('maxTouchPoints vs ontouchstart');
  if (navigator.userAgentData.mobile && (!touch || !m.coarse)) flag('mobile without touch');
  if (navigator.userAgentData.mobile !== /Mobile/.test(navigator.userAgent)) flag('UA-CH mobile vs user agent');
  const platforms = { Android: /Android/, macOS: /Mac OS X/, Windows: /Windows/, Linux: /Linux/ };
  if (!platforms[navigator.userAgentData.platform].test(navigator.userAgent)) flag('UA-CH platform vs user agent');
  if (/Android/.test(navigator.userAgent) !== /arm|aarch/.test(navigator.platform)) flag('navigator.platform vs user agent');
  // The worker and the frame from another site.
  for (const [w, o] of [['worker', s.worker], ['frame', s.frame], ['frameWorker', s.frameWorker]]) {
    for (const k of ['language', 'languages', 'intlLocale', 'timeZone', 'offsets', 'dateString', 'userAgent', 'platform', 'uaMobile', 'uaPlatform', 'cores', 'onLine']) {
      if (JSON.stringify(o[k]) !== JSON.stringify(m[k])) flag(w + ' ' + k);
    }
    for (const k of ['screen', 'avail', 'dpr', 'touchPoints', 'coarse', 'dark', 'reduce']) {
      if (k in o && JSON.stringify(o[k]) !== JSON.stringify(m[k])) flag(w + ' ' + k);
    }
  }
  if (s.frame.outer !== m.outer) flag('frame outer');
  // Values a script put there, rather than the browser.
  for (const [o, names] of [[Navigator.prototype, ['language', 'languages', 'platform', 'userAgent', 'hardwareConcurrency', 'maxTouchPoints', 'onLine']], [Screen.prototype, ['width', 'height', 'availWidth', 'availHeight']]]) {
    for (const n of names) {
      const d = Object.getOwnPropertyDescriptor(o, n);
      if (!d || !/\[native code\]/.test(Function.prototype.toString.call(d.get))) flag('patched ' + n);
    }
  }
  if (Object.getOwnPropertyNames(navigator).length || Object.getOwnPropertyNames(screen).length) flag('own properties');
  if (!/\[native code\]/.test(Function.prototype.toString.call(Date.prototype.getTimezoneOffset))) flag('patched getTimezoneOffset');
  if (!/\[native code\]/.test(Function.prototype.toString.call(Intl.DateTimeFormat))) flag('patched Intl');
  if (!/\[native code\]/.test(Function.prototype.toString.call(window.matchMedia))) flag('patched matchMedia');
  return found;
}
"#;

async fn detected(page: &netweir_browser::Page, s: &Value) -> Vec<String> {
    let found = page.evaluate(&format!("({DETECT})({s})")).await.unwrap();
    serde_json::from_value(found).unwrap()
}

#[tokio::test]
async fn detection_tricks_find_nothing_on_a_desktop() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    for env in [Environment::default(), desktop()] {
        let page = browser.new_page_with(env.clone()).await.unwrap();
        let s = surfaces(&page, &probe).await;
        let found = detected(&page, &s).await;
        assert!(
            found.is_empty(),
            "{env:?} gave itself away: {found:?}\n{s:#}"
        );
        page.close().await.unwrap();
    }
    browser.close().await.unwrap();
}

#[tokio::test]
async fn detection_tricks_on_a_phone_find_only_the_known_limits() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    let page = browser
        .new_page_with(Environment::device("Galaxy S22").unwrap())
        .await
        .unwrap();
    let s = surfaces(&page, &probe).await;
    let found = detected(&page, &s).await;
    // What CDP can't reach: a worker's navigator.platform is always the
    // system Chrome runs on, and Chrome won't make a window narrower than
    // 500 pixels, which a frame from another site reads where the page
    // reads the phone's. The design note says more.
    assert_eq!(
        found,
        ["worker platform", "frameWorker platform", "frame outer"],
        "{s:#}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_context_gives_its_pages_its_environment() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    let context = browser
        .new_context_with(Environment {
            locale: Some("fr-FR".into()),
            timezone: Some("America/Sao_Paulo".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let page = context
        .new_page_with(Environment {
            color_scheme: Some(ColorScheme::Light),
            ..Default::default()
        })
        .await
        .unwrap();
    let s = surfaces(&page, &probe).await;
    all_are(&s, "language", json!("fr-FR"));
    all_are(&s, "timeZone", json!("America/Sao_Paulo"));
    all_are(&s, "dark", json!(false));
    // A page can't have a clock or language of its own in a shared
    // context: Chrome keeps one of each per renderer process.
    let err = context
        .new_page_with(Environment {
            timezone: Some("Asia/Tokyo".into()),
            ..Default::default()
        })
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("context"), "{err}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn geolocation_goes_through_the_permission_a_person_grants() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    let page = browser
        .new_page_with(Environment {
            geolocation: Some(Geolocation {
                latitude: 48.8566,
                longitude: 2.3522,
                accuracy: 20.0,
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    page.goto(&probe.url("/style.css"), WaitUntil::Load, None)
        .await
        .unwrap();
    let states = "(async () => { \
        const q = async (n) => (await navigator.permissions.query({name: n})).state; \
        return [await q('geolocation'), await q('notifications'), await q('camera'), Notification.permission]; })()";
    let ask = "(async () => { \
        const at = await new Promise((ok) => navigator.geolocation.getCurrentPosition((p) => ok([p.coords.latitude, p.coords.longitude, p.coords.accuracy]), (e) => ok('error ' + e.code), {timeout: 5000})); \
        const q = async (n) => (await navigator.permissions.query({name: n})).state; \
        return {states: [await q('geolocation'), await q('notifications'), await q('camera'), Notification.permission], at}; })()";
    assert_eq!(
        page.evaluate(states).await.unwrap(),
        json!(["prompt", "prompt", "prompt", "default"])
    );
    let origin = probe.server.url.clone();
    page.grant_permissions(&["geolocation".into()], Some(&origin))
        .await
        .unwrap();
    let granted = page.evaluate(ask).await.unwrap();
    // Only geolocation changed: the rest are as a new person's Chrome has
    // them, not "denied".
    assert_eq!(
        granted["states"],
        json!(["granted", "prompt", "prompt", "default"])
    );
    assert_eq!(granted["at"], json!([48.8566, 2.3522, 20]));
    page.set_geolocation(Some(Geolocation {
        latitude: -33.8688,
        longitude: 151.2093,
        accuracy: 5.0,
    }))
    .await
    .unwrap();
    let moved = page.evaluate(ask).await.unwrap();
    assert_eq!(moved["at"], json!([-33.8688, 151.2093, 5]));
    page.clear_permissions().await.unwrap();
    assert_eq!(
        page.evaluate(states).await.unwrap(),
        json!(["prompt", "prompt", "prompt", "default"])
    );
    // Asked without a grant, headless Chrome turns it down as a person
    // blocking it would, and the profile remembers that.
    let refused = page.evaluate(ask).await.unwrap();
    assert_eq!(refused["at"], "error 1");
    assert_eq!(refused["states"][0], "denied");
    let bad = page
        .grant_permissions(&["not a permission".into()], None)
        .await;
    assert!(bad.is_err());
    browser.close().await.unwrap();
}

#[tokio::test]
async fn offline_fails_requests_and_says_so_everywhere() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    let page = browser.new_page().await.unwrap();
    page.goto(&probe.url("/style.css"), WaitUntil::Load, None)
        .await
        .unwrap();
    page.evaluate(
        "window.w = new Worker(URL.createObjectURL(new Blob([\"onmessage = async (e) => postMessage([navigator.onLine, await fetch(e.data).then((r) => r.status, () => 'failed')])\"])))",
    )
    .await
    .unwrap();
    let net = |url: String| {
        format!(
            "(async () => [[navigator.onLine, await fetch('{url}').then((r) => r.status, () => 'failed')], \
             await new Promise((ok) => {{ w.onmessage = (m) => ok(m.data); w.postMessage('{url}'); }})])()"
        )
    };
    let fetched = probe.url("/worker-fetch");
    assert_eq!(
        page.evaluate(&net(fetched.clone())).await.unwrap(),
        json!([[true, 200], [true, 200]])
    );
    page.set_offline(true).await.unwrap();
    assert_eq!(
        page.evaluate(&net(fetched.clone())).await.unwrap(),
        json!([[false, "failed"], [false, "failed"]])
    );
    page.set_offline(false).await.unwrap();
    assert_eq!(
        page.evaluate(&net(fetched)).await.unwrap(),
        json!([[true, 200], [true, 200]])
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn settings_changed_later_reach_the_page() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    let page = browser.new_page().await.unwrap();
    page.goto(&probe.url("/style.css"), WaitUntil::Load, None)
        .await
        .unwrap();
    let inner = "[innerWidth, innerHeight, outerWidth - innerWidth, outerHeight - innerHeight]";
    let before = page.evaluate(inner).await.unwrap();
    // A page given nothing shows 1280 by 720 on a 1920 by 1080 screen.
    assert_eq!(&before.as_array().unwrap()[..2], [1280, 720], "{before}");
    assert_eq!(
        page.evaluate("[screen.width, screen.height]")
            .await
            .unwrap(),
        json!([1920, 1080])
    );
    page.set_viewport(Size {
        width: 1000,
        height: 600,
    })
    .await
    .unwrap();
    let after = page.evaluate(inner).await.unwrap();
    assert_eq!(after[0], 1000, "{after}");
    assert_eq!(after[1], 600, "{after}");
    // The window's frame stays what it was.
    assert_eq!(after[2], before[2]);
    assert_eq!(after[3], before[3]);
    assert_eq!(
        page.viewport(),
        Some(Size {
            width: 1000,
            height: 600
        })
    );

    page.emulate_media(Some(ColorScheme::Dark), None)
        .await
        .unwrap();
    assert_eq!(
        page.evaluate("matchMedia('(prefers-color-scheme: dark)').matches")
            .await
            .unwrap(),
        true
    );
    page.add_init_script("window.later = 'yes'").await.unwrap();
    page.set_extra_headers(&[("X-Later".into(), "1".into())])
        .await
        .unwrap();
    page.goto(&probe.url("/frame-fetch"), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(page.evaluate("window.later").await.unwrap(), "yes");
    assert_eq!(
        probe.header("/frame-fetch", "x-later").as_deref(),
        Some("1")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_service_worker_keeps_the_pages_language_and_clock() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    let page = browser
        .new_page_with(Environment {
            locale: Some("ja-JP".into()),
            timezone: Some("Asia/Tokyo".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    page.goto(&probe.url("/sw.html"), WaitUntil::Load, None)
        .await
        .unwrap();
    let said = tokio::time::timeout(Duration::from_secs(20), page.evaluate("sw"))
        .await
        .expect("the service worker never answered")
        .unwrap();
    assert_eq!(said, json!(["ja-JP", "Asia/Tokyo", "ja-JP"]));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_phone_is_tapped_and_scrolled_by_touch() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let server = serve(vec![(
        "/",
        html(
            "<meta name=viewport content='width=device-width'>\
             <button id=b style='margin-top:100px;width:200px;height:80px'>tap</button>\
             <div style='height:5000px'></div>\
             <script>window.seen = []; for (const k of ['pointerdown', 'touchstart', 'touchend', 'mousemove', 'mousedown', 'click', 'wheel']) \
             addEventListener(k, (e) => seen.push(k + (e.pointerType ? ':' + e.pointerType : '')), true);</script>",
        ),
    )]);
    let page = browser
        .new_page_with(Environment::device("Galaxy S21").unwrap())
        .await
        .unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#b", None).await.unwrap();
    let seen = page.evaluate("seen").await.unwrap();
    let seen: Vec<String> = serde_json::from_value(seen).unwrap();
    assert!(seen.contains(&"pointerdown:touch".to_string()), "{seen:?}");
    assert!(seen.contains(&"touchstart".to_string()), "{seen:?}");
    assert!(seen.contains(&"click:touch".to_string()), "{seen:?}");
    // No mouse came over the button first, as none does on a phone.
    let before_touch = seen.iter().position(|s| s == "touchstart").unwrap();
    assert!(
        !seen[..before_touch].iter().any(|s| s.starts_with("mouse")),
        "{seen:?}"
    );
    page.evaluate("seen.length = 0").await.unwrap();
    page.scroll(None, 0.0, Some(600.0), None, Duration::from_secs(1), None)
        .await
        .unwrap();
    let y = page.evaluate("scrollY").await.unwrap().as_f64().unwrap();
    assert!((500.0..=700.0).contains(&y), "scrolled to {y}");
    let seen: Vec<String> = serde_json::from_value(page.evaluate("seen").await.unwrap()).unwrap();
    assert!(!seen.iter().any(|s| s == "wheel"), "{seen:?}");
    assert!(seen.iter().any(|s| s == "touchstart"), "{seen:?}");
    let hover = page.hover("#b", None).await.unwrap_err();
    assert!(hover.to_string().contains("phone"), "{hover}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_browser_launched_with_an_environment_is_it_from_the_start() {
    let Some(mut options) = launch_options() else {
        return;
    };
    options.environment = Environment {
        locale: Some("de-DE".into()),
        screen: Some(Size {
            width: 1366,
            height: 768,
        }),
        ..Default::default()
    };
    let browser = common::launch(options).await;
    let probe = Probe::new();
    let page = browser.new_page().await.unwrap();
    let s = surfaces(&page, &probe).await;
    all_are(&s, "screen", json!("1366x768"));
    all_are(&s, "intlLocale", json!("de-DE"));
    // Chrome's own languages, as it shows them for a person: only the
    // first to page script, and the header expanded.
    all_are(&s, "language", json!("de-DE"));
    assert_eq!(
        probe.header("/", "accept-language").as_deref(),
        Some("de-DE,de;q=0.9")
    );
    let found = detected(&page, &s).await;
    assert!(found.is_empty(), "{found:?}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn what_chrome_cant_show_is_refused() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    // A pixel ratio that isn't whole can only be the whole browser's.
    let err = browser
        .new_page_with(Environment {
            device_scale_factor: Some(2.625),
            ..Default::default()
        })
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("launch"), "{err}");
    let err = browser
        .new_page_with(Environment {
            timezone: Some("Mars/Olympus_Mons".into()),
            ..Default::default()
        })
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("timezone"), "{err}");
    let err = browser
        .new_page_with(Environment {
            mobile: Some(true),
            ..Default::default()
        })
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("mobile"), "{err}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_phones_popup_is_the_same_phone_from_its_first_script() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let server = serve(vec![
        (
            "/",
            html("<a id=open href=/popup target=_blank style='font-size:40px'>open</a>"),
        ),
        (
            "/popup",
            html(
                "<meta name=viewport content='width=device-width'><script>window.first = \
                 [navigator.userAgentData.mobile, navigator.platform, devicePixelRatio, \
                 screen.width, innerWidth, navigator.language, \
                 Intl.DateTimeFormat().resolvedOptions().timeZone];</script>",
            ),
        ),
    ]);
    let mut phone = Environment::device("Galaxy S21").unwrap();
    phone.locale = Some("es-ES".into());
    phone.timezone = Some("Europe/Madrid".into());
    let page = browser.new_page_with(phone).await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let mut events = page.listen();
    // Tapped, as a person opens one: Chrome blocks a popup nobody asked for.
    page.click("#open", None).await.unwrap();
    let popup = loop {
        match tokio::time::timeout(Duration::from_secs(20), events.recv())
            .await
            .expect("no popup")
        {
            Some(netweir_browser::PageEvent::Popup(p)) => break p,
            Some(_) => {}
            None => panic!("the page went away"),
        }
    };
    popup.wait_for_load(WaitUntil::Load, None).await.unwrap();
    assert_eq!(
        popup.evaluate("first").await.unwrap(),
        json!([true, "Linux armv81", 3, 360, 360, "es-ES", "Europe/Madrid"])
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_frame_with_init_scripts_tells_of_its_dialog_once() {
    let Some(options) = launch_options() else {
        return;
    };
    let browser = common::launch(options).await;
    let server = serve(vec![
        (
            "/",
            html(
                "<script>const f = document.createElement('iframe'); \
                 f.src = location.origin.replace('127.0.0.1', 'localhost') + '/frame'; \
                 document.documentElement.appendChild(f); \
                 addEventListener('message', (m) => window.answered = m.data);</script>",
            ),
        ),
        (
            "/frame",
            html("<script>parent.postMessage(confirm('sure?'), '*');</script>"),
        ),
    ]);
    let page = browser
        .new_page_with(Environment {
            init_scripts: vec!["self.initRan = 1".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    let mut dialogs = page.dialogs();
    // The frame's dialog holds up the page's load.
    page.goto(&server.url, WaitUntil::DomContentLoaded, None)
        .await
        .unwrap();
    let dialog = tokio::time::timeout(Duration::from_secs(20), dialogs.recv())
        .await
        .expect("no dialog")
        .unwrap();
    assert_eq!(dialog.message(), "sure?");
    dialog.accept(None).await.unwrap();
    // No second telling of the same dialog.
    assert!(
        tokio::time::timeout(Duration::from_millis(500), dialogs.recv())
            .await
            .is_err()
    );
    let mut answered = Value::Null;
    for _ in 0..100 {
        answered = page.evaluate("window.answered ?? null").await.unwrap();
        if !answered.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(answered, true);
    browser.close().await.unwrap();
}
