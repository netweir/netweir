//! The screen a page is on and the window it's shown in. Both are real:
//! headless Chrome's own virtual screens, and the page's own window sized
//! to its viewport, so the page, its frames from other sites and its
//! workers all read the same screen, ratio and window. Metrics emulation
//! is only for a phone, whose layout nothing else gives; why, with what
//! was measured, is in docs/design/environment.md.

use serde_json::{Value, json};

use crate::environment::{Environment, Size};
use crate::page::Page;
use crate::{Error, Result};

/// The screen a headless Chrome netweir launches is given, unless its
/// environment names one: the commonest desktop screen. Chrome's own
/// headless screen is 800 by 600, smaller than the windows it opens.
pub(crate) const DEFAULT_SCREEN: Size = Size {
    width: 1920,
    height: 1080,
};

/// A page's viewport when nothing names one, where its screen has room.
pub(crate) const DEFAULT_VIEWPORT: Size = Size {
    width: 1280,
    height: 720,
};

/// How a screen shows itself beyond its size: what the system takes of it
/// (in CSS pixels, top and bottom), its colour depth, whether it's the
/// machine's own, and its name, as `getScreenDetails()` gives it.
#[derive(Debug, Clone, PartialEq)]
struct Look {
    top: u32,
    bottom: u32,
    depth: u32,
    internal: bool,
    label: &'static str,
}

/// A Mac's own screens, by the size they show at a ratio of 2, and
/// whether there's a notch, which makes the menu bar taller.
const MAC_BUILT_IN: [(u32, u32, bool); 7] = [
    (1512, 982, true),
    (1728, 1117, true),
    (1470, 956, true),
    (1710, 1112, true),
    (1440, 900, false),
    (1280, 800, false),
    (1536, 960, false),
];

/// What a screen a person might have is called: a common monitor's name
/// for its size, as its EDID gives it.
fn monitor(size: Size) -> &'static str {
    match (size.width, size.height) {
        (2560, 1440) => "DELL S2721DGF",
        (3840, 2160) => "LG HDR 4K",
        (1366, 768) => "Generic PnP Monitor",
        (1440, 900) | (1680, 1050) => "DELL P2217",
        _ => "DELL P2422H",
    }
}

/// How the screen of `size` at `scale` looks on this system, the first
/// (`primary`) or one beside it, or a phone's.
///
/// - macOS, measured on 26 with Google Chrome 155: a built-in screen is
///   "Built-in Retina Display", internal and 30 bits deep, its menu bar
///   33 beside a notch and the Dock 60 below; an external one's bar is 30,
///   its depth 24. The Dock is on the first screen only. Before macOS 26
///   the bar was 24 points and a line (25), 37 beside a notch (not
///   measured here).
/// - Windows: the taskbar, 48 on Windows 11 and 40 on 10, on every
///   screen, as Windows shows it by default (not measured here).
/// - Linux: the whole screen. Its desktops differ (a top bar, a dock, a
///   panel at any edge, or none on a server), so no one shape is more
///   like a person's than another.
fn look(size: Size, scale: f64, primary: bool, phone: bool) -> Look {
    let external = Look {
        top: 0,
        bottom: 0,
        depth: 24,
        internal: false,
        label: monitor(size),
    };
    if phone {
        return Look {
            internal: true,
            label: "",
            ..external
        };
    }
    if cfg!(target_os = "macos") {
        let built_in = MAC_BUILT_IN
            .iter()
            .find(|(w, h, _)| *w == size.width && *h == size.height && scale == 2.0);
        let new = os_major() >= 26;
        let notch = built_in.is_some_and(|b| b.2);
        let top = match (new, notch) {
            (true, true) => 33,
            (true, false) => 30,
            (false, true) => 37,
            (false, false) => 25,
        };
        let bottom = if primary { 60 } else { 0 };
        return match built_in {
            Some(_) => Look {
                top,
                bottom,
                depth: 30,
                internal: true,
                label: "Built-in Retina Display",
            },
            None => Look {
                top,
                bottom,
                ..external
            },
        };
    }
    if cfg!(windows) {
        let bottom = if os_major() >= 22000 { 48 } else { 40 };
        return Look { bottom, ..external };
    }
    external
}

/// The system's version, as far as the screen's shape goes: macOS's major
/// version, or Windows' build number (22000 and up is Windows 11). Zero
/// if it can't be read.
fn os_major() -> u32 {
    static MAJOR: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MAJOR.get_or_init(|| {
        if cfg!(target_os = "macos") {
            let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
                .unwrap_or_default();
            plist
                .split("<key>ProductVersion</key>")
                .nth(1)
                .and_then(|rest| rest.split("<string>").nth(1))
                .and_then(|v| v.split('.').next())
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0)
        } else if cfg!(windows) {
            // "Microsoft Windows [Version 10.0.22631.4602]"
            std::process::Command::new("cmd")
                .args(["/c", "ver"])
                .output()
                .ok()
                .and_then(|o| {
                    let said = String::from_utf8_lossy(&o.stdout).to_string();
                    said.split('.').nth(2).and_then(|b| b.trim().parse().ok())
                })
                .unwrap_or(0)
        } else {
            0
        }
    })
}

/// The switches that give a launched headless Chrome its screen: the
/// environment's, or the default. A ratio that isn't whole can only be
/// given this way, for the whole browser.
pub(crate) fn launch_switches(env: &Environment, args: &[String]) -> Vec<String> {
    if args.iter().any(|a| a.starts_with("--screen-info")) {
        return Vec::new();
    }
    let size = screen_of(env);
    let scale = env.device_scale_factor.unwrap_or(1.0);
    let look = look(size, scale, true, env.is_mobile());
    let (top, bottom) = (look.top, look.bottom);
    let px = |css: u32| (css as f64 * scale).round() as u64;
    let mut info = format!("{{{}x{}", px(size.width), px(size.height));
    if !look.label.is_empty() {
        info.push_str(&format!(" label='{}'", look.label));
    }
    info.push_str(&format!(" colorDepth={}", look.depth));
    if look.internal {
        info.push_str(" isInternal=true");
    }
    let mut out = Vec::new();
    if scale.fract() == 0.0 {
        info.push_str(&format!(" devicePixelRatio={scale}"));
    } else if !args
        .iter()
        .any(|a| a.starts_with("--force-device-scale-factor"))
    {
        out.push(format!("--force-device-scale-factor={scale}"));
    }
    if top > 0 {
        info.push_str(&format!(" workAreaTop={}", px(top)));
    }
    if bottom > 0 {
        info.push_str(&format!(" workAreaBottom={}", px(bottom)));
    }
    info.push('}');
    out.push(format!("--screen-info={info}"));
    out
}

/// For a phone, `Emulation.setDeviceMetricsOverride`'s parameters: the
/// only way to have Chrome lay a page out as a phone does, with its
/// viewport meta tag. Its screen is a real one of the same size too (see
/// `arrange`), which frames from other sites read. `scale` is the ratio
/// when the environment gives none.
pub(crate) fn mobile_metrics(env: &Environment, scale: f64) -> Option<Value> {
    if !env.is_mobile() {
        return None;
    }
    let screen = screen_of(env);
    let viewport = env.viewport.unwrap_or(screen);
    let portrait = viewport.height >= viewport.width;
    Some(json!({
        "width": viewport.width,
        "height": viewport.height,
        "deviceScaleFactor": env.device_scale_factor.unwrap_or(scale),
        "mobile": true,
        "screenWidth": screen.width,
        "screenHeight": screen.height,
        "screenOrientation": if portrait {
            json!({"type": "portraitPrimary", "angle": 0})
        } else {
            json!({"type": "landscapePrimary", "angle": 90})
        },
    }))
}

/// The screen an environment is on: the one it names, a phone's viewport
/// for a phone without one, or the default.
fn screen_of(env: &Environment) -> Size {
    match (env.screen, env.is_mobile(), env.viewport) {
        (Some(s), _, _) => s,
        (None, true, Some(v)) => v,
        _ => DEFAULT_SCREEN,
    }
}

/// One of Chrome's screens, in CSS pixels.
#[derive(Debug, Clone)]
struct Screen {
    left: i64,
    width: u32,
    height: u32,
    scale: f64,
    avail: (i64, i64, i64, i64),
}

impl Screen {
    fn from(info: &Value) -> Screen {
        let n = |k: &str| info[k].as_i64().unwrap_or(0);
        Screen {
            left: n("left"),
            width: n("width").max(0) as u32,
            height: n("height").max(0) as u32,
            scale: info["devicePixelRatio"].as_f64().unwrap_or(1.0),
            avail: (
                n("availLeft"),
                n("availTop"),
                n("availWidth"),
                n("availHeight"),
            ),
        }
    }

    fn is(&self, size: Size, scale: f64) -> bool {
        self.width == size.width && self.height == size.height && self.scale == scale
    }
}

impl Page {
    /// Puts the page's window on the screen its environment names, sized
    /// so its viewport is the environment's, and for a phone, has Chrome
    /// lay the page out as one. A popup keeps the size its opener asked
    /// for (`resize` false). Only for a headless Chrome netweir launched,
    /// whose screens and windows nobody else sees; elsewhere a viewport
    /// resizes the real window, and anything about the screen is refused.
    pub(crate) async fn arrange(&self, resize: bool) -> Result<()> {
        let env = self.environment();
        let browser = &self.inner.browser;
        if !browser.arranges {
            if env.screen.is_some() || env.device_scale_factor.is_some() || env.is_mobile() {
                return Err(Error::Invalid(
                    "a screen, pixel ratio or phone needs a headless Chrome netweir launched: \
                     another's real screen can't be changed, and emulating one would leave frames \
                     from other sites on the real one"
                        .into(),
                ));
            }
            if let (Some(v), true) = (env.viewport, resize) {
                let (window, _) = self.window().await?;
                let insets = self.insets(&window).await?;
                self.bounds(
                    &window,
                    json!({"width": v.width as i64 + insets.0, "height": v.height as i64 + insets.1}),
                )
                .await?;
                *self.inner.shown.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
            }
            return Ok(());
        }
        let (window, bounds) = self.window().await?;
        let insets = self.insets(&window).await?;
        let scale = env.device_scale_factor.unwrap_or(browser.screen_scale);
        let mobile = env.is_mobile();
        // A phone's window is its viewport: its browser has no frame
        // around the page, and its metrics say the window is that size.
        let frame = if mobile { (0, 0) } else { insets };
        let size = match (env.screen, mobile) {
            (Some(s), _) => s,
            (None, true) => screen_of(&env),
            (None, false) => browser.screen,
        };
        let fits = |s: &Screen, v: Size| {
            v.width as i64 + frame.0 <= s.avail.2 && v.height as i64 + frame.1 <= s.avail.3
        };
        let screen = self.screen(size, scale, mobile).await?;
        // Chrome sized from a --window-size switch, unless the
        // environment names a viewport.
        // A window bigger than the work area is made to fit it, as Chrome
        // makes a window of its own.
        let switched = browser.window_size.map(|(w, h)| Size {
            width: (w as i64).min(screen.avail.2).saturating_sub(frame.0).max(1) as u32,
            height: (h as i64).min(screen.avail.3).saturating_sub(frame.1).max(1) as u32,
        });
        let viewport = match env.viewport.or(switched) {
            Some(v) if fits(&screen, v) => v,
            Some(v) => {
                // Not moved to a bigger screen: that would put a second one
                // beside it, and screen.isExtended with it.
                return Err(Error::Invalid(format!(
                    "a {}x{} viewport, in a window {} by {} pixels bigger, doesn't fit the {}x{} \
                     the screen's work area leaves it: give a smaller viewport, or a screen \
                     that fits it",
                    v.width, v.height, frame.0, frame.1, screen.avail.2, screen.avail.3
                )));
            }
            None if fits(&screen, DEFAULT_VIEWPORT) => DEFAULT_VIEWPORT,
            // As a maximised window.
            None => Size {
                width: (screen.avail.2 - frame.0).max(1) as u32,
                height: (screen.avail.3 - frame.1).max(1) as u32,
            },
        };
        let mut place = json!({"left": screen.avail.0, "top": screen.avail.1});
        if resize {
            place["width"] = Value::from(viewport.width as i64 + frame.0);
            place["height"] = Value::from(viewport.height as i64 + frame.1);
        } else {
            // A popup at its own size, moved onto the screen.
            let w = bounds["width"].as_i64().unwrap_or(0).min(screen.avail.2);
            let h = bounds["height"].as_i64().unwrap_or(0).min(screen.avail.3);
            place["width"] = Value::from(w);
            place["height"] = Value::from(h);
        }
        self.bounds(&window, place).await?;
        if resize {
            *self.inner.shown.lock().unwrap_or_else(|e| e.into_inner()) = Some(viewport);
        }
        Ok(())
    }

    /// The window's id and bounds.
    async fn window(&self) -> Result<(Value, Value)> {
        let r = self
            .inner
            .browser
            .call(
                "",
                "Browser.getWindowForTarget",
                json!({"targetId": self.inner.target}),
            )
            .await?;
        Ok((r["windowId"].clone(), r["bounds"].clone()))
    }

    /// How much bigger the window is than the page's viewport, measured
    /// once, before anything emulates other metrics.
    async fn insets(&self, window: &Value) -> Result<(i64, i64)> {
        if let Some(known) = *self.inner.insets.lock().unwrap_or_else(|e| e.into_inner()) {
            return Ok(known);
        }
        let bounds = self
            .inner
            .browser
            .call("", "Browser.getWindowBounds", json!({"windowId": window}))
            .await?;
        let metrics = self.call("Page.getLayoutMetrics", json!({})).await?;
        let inner = &metrics["cssLayoutViewport"];
        let n = |v: &Value| v.as_f64().unwrap_or(0.0).round() as i64;
        let insets = (
            (n(&bounds["bounds"]["width"]) - n(&inner["clientWidth"])).max(0),
            (n(&bounds["bounds"]["height"]) - n(&inner["clientHeight"])).max(0),
        );
        *self.inner.insets.lock().unwrap_or_else(|e| e.into_inner()) = Some(insets);
        Ok(insets)
    }

    async fn bounds(&self, window: &Value, bounds: Value) -> Result<()> {
        let browser = &self.inner.browser;
        // A window that's maximised, say, takes no bounds.
        browser
            .call(
                "",
                "Browser.setWindowBounds",
                json!({"windowId": window, "bounds": {"windowState": "normal"}}),
            )
            .await?;
        browser
            .call(
                "",
                "Browser.setWindowBounds",
                json!({"windowId": window, "bounds": bounds}),
            )
            .await?;
        Ok(())
    }

    /// The screen of that size and ratio, made (to the right of the others)
    /// if Chrome hasn't one yet. Screens stay until the browser closes.
    async fn screen(&self, size: Size, scale: f64, phone: bool) -> Result<Screen> {
        let browser = &self.inner.browser;
        let _one_at_a_time = browser.screens.lock().await;
        let infos = self.call("Emulation.getScreenInfos", json!({})).await?;
        let screens: Vec<Screen> = infos["screenInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .map(Screen::from)
            .collect();
        if let Some(s) = screens.iter().find(|s| s.is(size, scale)) {
            return Ok(s.clone());
        }
        let left = screens
            .iter()
            .map(|s| s.left + s.width as i64)
            .max()
            .unwrap_or(0);
        let px = |css: u32| (css as f64 * scale).round() as i64;
        let look = look(size, scale, false, phone);
        let mut params = json!({
            "left": left,
            "top": 0,
            "width": px(size.width),
            "height": px(size.height),
            "devicePixelRatio": scale,
            "colorDepth": look.depth,
            "isInternal": look.internal,
            "label": look.label,
        });
        if look.top > 0 || look.bottom > 0 {
            params["workAreaInsets"] = json!({"top": px(look.top), "bottom": px(look.bottom)});
        }
        let added = self.call("Emulation.addScreen", params).await?;
        Ok(Screen::from(&added["screenInfo"]))
    }

    /// Changes the viewport, as [`Environment::viewport`] gives it.
    pub async fn set_viewport(&self, viewport: Size) -> Result<()> {
        let mut env = self.environment();
        env.viewport = Some(viewport);
        env.validate()?;
        let metrics = mobile_metrics(&env, self.inner.browser.screen_scale);
        let before = self.replace_environment(env);
        let mut arranged = self.arrange(true).await;
        if let (Ok(()), Some(metrics)) = (&arranged, metrics) {
            arranged = self
                .call("Emulation.setDeviceMetricsOverride", metrics)
                .await
                .map(drop);
        }
        if arranged.is_err() {
            self.replace_environment(before);
        }
        arranged
    }

    /// The page's viewport, as netweir last sized it: None for a page
    /// whose window is Chrome's (a popup, or a browser netweir didn't
    /// launch headless, given no viewport).
    pub fn viewport(&self) -> Option<Size> {
        *self.inner.shown.lock().unwrap_or_else(|e| e.into_inner())
    }
}
