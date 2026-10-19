//! A page's environment: the screen and window it's shown in, the device
//! it says it is, its language, clock and place, and what it may do.
//!
//! Every setting is made through Chrome's own machinery, so each surface a
//! page can read agrees with the others: the screen is a real (virtual)
//! screen of headless Chrome's, the viewport is the real window's, the
//! language goes through Chrome's own user agent override, and so on. Why
//! each is done the way it is, with what was measured, is in
//! docs/design/environment.md.

use serde_json::{Value, json};

use crate::{Error, Result};

/// A width and a height in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// Where the page is, as `navigator.geolocation` tells it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geolocation {
    pub latitude: f64,
    pub longitude: f64,
    /// In metres.
    pub accuracy: f64,
}

/// What `prefers-color-scheme` matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
    NoPreference,
}

/// What `prefers-reduced-motion` matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReducedMotion {
    Reduce,
    NoPreference,
}

impl std::str::FromStr for ColorScheme {
    type Err = Error;

    fn from_str(s: &str) -> Result<ColorScheme> {
        match s {
            "light" => Ok(ColorScheme::Light),
            "dark" => Ok(ColorScheme::Dark),
            "no-preference" => Ok(ColorScheme::NoPreference),
            _ => Err(Error::Invalid(format!(
                "color_scheme must be \"light\", \"dark\" or \"no-preference\", not {s:?}"
            ))),
        }
    }
}

impl std::str::FromStr for ReducedMotion {
    type Err = Error;

    fn from_str(s: &str) -> Result<ReducedMotion> {
        match s {
            "reduce" => Ok(ReducedMotion::Reduce),
            "no-preference" => Ok(ReducedMotion::NoPreference),
            _ => Err(Error::Invalid(format!(
                "reduced_motion must be \"reduce\" or \"no-preference\", not {s:?}"
            ))),
        }
    }
}

/// The phone a page says it's on: what its client hints and its
/// navigator name, beyond the screen and window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phone {
    /// The model its high-entropy client hints name, such as "SM-S911B".
    pub model: String,
    /// The Android version, as the hints give it: "14.0.0".
    pub platform_version: String,
    /// `navigator.hardwareConcurrency`.
    pub cores: u32,
    /// `navigator.maxTouchPoints`.
    pub touch_points: u32,
}

/// What a page, a context's pages or all of a browser's pages are shown in
/// and say about themselves. Unset settings are Chrome's own, or come from
/// the context or browser the page is in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Environment {
    /// The window's inside: `innerWidth` and `innerHeight`. Unset, a page
    /// shows 1280 by 720 where its screen has room, and fills the screen's
    /// work area where it hasn't.
    pub viewport: Option<Size>,
    /// The display the window is on: `screen.width` and `screen.height`.
    pub screen: Option<Size>,
    /// `devicePixelRatio`, the screen's.
    pub device_scale_factor: Option<f64>,
    /// A phone's layout: the viewport meta tag counts, and the page is a
    /// phone's in its client hints. Needs [`Environment::phone`] or a
    /// phone's [`Environment::user_agent`].
    pub mobile: Option<bool>,
    /// A touch screen: touch events, `maxTouchPoints`, and a coarse pointer.
    pub has_touch: Option<bool>,
    /// Such as "de-DE": `Intl`, `toLocaleString`, `navigator.language` and
    /// the `Accept-Language` header.
    pub locale: Option<String>,
    /// An IANA time zone, such as "Europe/Berlin".
    pub timezone: Option<String>,
    pub geolocation: Option<Geolocation>,
    /// Permissions granted to every origin, such as "geolocation".
    pub permissions: Vec<String>,
    pub color_scheme: Option<ColorScheme>,
    pub reduced_motion: Option<ReducedMotion>,
    /// Sent with every request, after Chrome's own.
    pub extra_headers: Vec<(String, String)>,
    pub offline: Option<bool>,
    /// A user agent of the Chrome running, perhaps for another system.
    pub user_agent: Option<String>,
    /// Run in each document of the page and of its frames, before the
    /// document's own scripts.
    pub init_scripts: Vec<String>,
    /// The phone it is, for a phone's preset.
    pub phone: Option<Phone>,
}

/// A device preset: its name and its environment.
struct Preset {
    name: &'static str,
    screen: (u32, u32),
    /// None fills the screen's work area, as a maximised window does.
    viewport: Option<(u32, u32)>,
    scale: f64,
    /// (model, Android version, cores) for a phone.
    phone: Option<(&'static str, &'static str, u32)>,
}

/// Every preset is a real screen with a whole pixel ratio: headless Chrome
/// can only make screens like that, and a frame from another site reads
/// the real one (see the design note). That rules out the Pixels, whose
/// ratio is 2.625 or 2.75. A phone's viewport is its screen less Chrome's
/// status and address bars, 76 pixels.
const PRESETS: &[Preset] = &[
    Preset {
        name: "Desktop 1920x1080",
        screen: (1920, 1080),
        viewport: None,
        scale: 1.0,
        phone: None,
    },
    Preset {
        name: "Desktop 2560x1440",
        screen: (2560, 1440),
        viewport: None,
        scale: 1.0,
        phone: None,
    },
    Preset {
        name: "Desktop 1366x768",
        screen: (1366, 768),
        viewport: None,
        scale: 1.0,
        phone: None,
    },
    Preset {
        name: "Desktop 1440x900 HiDPI",
        screen: (1440, 900),
        viewport: None,
        scale: 2.0,
        phone: None,
    },
    Preset {
        name: "Desktop 1512x982 HiDPI",
        screen: (1512, 982),
        viewport: None,
        scale: 2.0,
        phone: None,
    },
    Preset {
        name: "Galaxy S21",
        screen: (360, 800),
        viewport: Some((360, 724)),
        scale: 3.0,
        phone: Some(("SM-G991B", "14.0.0", 8)),
    },
    Preset {
        name: "Galaxy S22",
        screen: (360, 780),
        viewport: Some((360, 704)),
        scale: 3.0,
        phone: Some(("SM-S901B", "14.0.0", 8)),
    },
    Preset {
        name: "Galaxy S23",
        screen: (360, 780),
        viewport: Some((360, 704)),
        scale: 3.0,
        phone: Some(("SM-S911B", "14.0.0", 8)),
    },
];

/// The names of the device presets, for [`Environment::device`].
pub fn device_names() -> Vec<&'static str> {
    PRESETS.iter().map(|p| p.name).collect()
}

/// What a page on a phone says its `navigator.platform` is.
const PHONE_PLATFORM: &str = "Linux armv81";

impl Environment {
    /// A device preset by name (see [`device_names`]): a desktop's screen,
    /// or a phone's screen, viewport, touch and identity.
    pub fn device(name: &str) -> Result<Environment> {
        let Some(p) = PRESETS.iter().find(|p| p.name == name) else {
            return Err(Error::Invalid(format!(
                "no device preset called {name:?}; there are {}",
                device_names().join(", ")
            )));
        };
        let size = |(width, height)| Size { width, height };
        let mut env = Environment {
            screen: Some(size(p.screen)),
            viewport: p.viewport.map(size),
            device_scale_factor: Some(p.scale),
            mobile: Some(p.phone.is_some()),
            has_touch: Some(p.phone.is_some()),
            ..Environment::default()
        };
        if let Some((model, version, cores)) = p.phone {
            env.phone = Some(Phone {
                model: model.into(),
                platform_version: version.into(),
                cores,
                touch_points: 5,
            });
        }
        Ok(env)
    }

    /// This environment, with what it leaves unset taken from `base`:
    /// init scripts and permissions are added to the base's, and extra
    /// headers replace the base's of the same name.
    pub fn over(&self, base: &Environment) -> Environment {
        let mut extra_headers = base.extra_headers.clone();
        for (name, value) in &self.extra_headers {
            extra_headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
            extra_headers.push((name.clone(), value.clone()));
        }
        let mut permissions = base.permissions.clone();
        for p in &self.permissions {
            if !permissions.contains(p) {
                permissions.push(p.clone());
            }
        }
        Environment {
            viewport: self.viewport.or(base.viewport),
            screen: self.screen.or(base.screen),
            device_scale_factor: self.device_scale_factor.or(base.device_scale_factor),
            mobile: self.mobile.or(base.mobile),
            has_touch: self.has_touch.or(base.has_touch),
            locale: self.locale.clone().or_else(|| base.locale.clone()),
            timezone: self.timezone.clone().or_else(|| base.timezone.clone()),
            geolocation: self.geolocation.or(base.geolocation),
            permissions,
            color_scheme: self.color_scheme.or(base.color_scheme),
            reduced_motion: self.reduced_motion.or(base.reduced_motion),
            extra_headers,
            offline: self.offline.or(base.offline),
            user_agent: self.user_agent.clone().or_else(|| base.user_agent.clone()),
            init_scripts: base
                .init_scripts
                .iter()
                .chain(&self.init_scripts)
                .cloned()
                .collect(),
            phone: self.phone.clone().or_else(|| base.phone.clone()),
        }
    }

    /// Whether this says nothing at all.
    pub fn is_empty(&self) -> bool {
        *self == Environment::default()
    }

    pub(crate) fn is_mobile(&self) -> bool {
        self.mobile == Some(true)
    }

    /// Whether the page is touched rather than clicked: a phone's. A
    /// desktop with a touch screen still has its mouse.
    pub(crate) fn touches(&self) -> bool {
        self.is_mobile() && self.has_touch == Some(true)
    }

    /// Checks the settings agree with each other and with what Chrome can
    /// show, so a page is never made that contradicts itself.
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(Error::Invalid(m));
        for (what, size) in [("viewport", self.viewport), ("screen", self.screen)] {
            if let Some(s) = size
                && (!(1..=10_000).contains(&s.width) || !(1..=10_000).contains(&s.height))
            {
                return bad(format!(
                    "{what} must be 1 to 10000 pixels each way, not {}x{}",
                    s.width, s.height
                ));
            }
        }
        if let (Some(v), Some(s)) = (self.viewport, self.screen)
            && (v.width > s.width || v.height > s.height)
        {
            return bad(format!(
                "a {}x{} viewport doesn't fit on a {}x{} screen",
                v.width, v.height, s.width, s.height
            ));
        }
        if let Some(scale) = self.device_scale_factor
            && !(scale > 0.0 && scale <= 10.0)
        {
            return bad(format!(
                "device_scale_factor must be more than 0 and at most 10, not {scale}"
            ));
        }
        let android = self
            .user_agent
            .as_deref()
            .is_some_and(|ua| ua.contains("Android"));
        if self.is_mobile() && self.phone.is_none() && !android {
            return bad(
                "mobile needs a phone's identity, from a device preset or an Android user_agent: \
                 a phone's layout with a desktop's client hints is a contradiction pages can see"
                    .into(),
            );
        }
        if self.mobile == Some(false) && (self.phone.is_some() || is_phone_agent(self)) {
            return bad("a phone's user agent needs mobile left on".into());
        }
        if let Some(locale) = &self.locale
            && (locale.is_empty()
                || !locale
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
                || locale.starts_with('-')
                || locale.ends_with('-'))
        {
            return bad(format!(
                "locale must be a language tag such as \"de-DE\", not {locale:?}"
            ));
        }
        if self
            .timezone
            .as_deref()
            .is_some_and(|t| t.trim().is_empty())
        {
            return bad("timezone must name a time zone, such as \"Europe/Berlin\"".into());
        }
        if let Some(g) = self.geolocation
            && !((-90.0..=90.0).contains(&g.latitude)
                && (-180.0..=180.0).contains(&g.longitude)
                && g.accuracy >= 0.0)
        {
            return bad(format!(
                "geolocation needs a latitude from -90 to 90, a longitude from -180 to 180 and \
                 an accuracy of 0 or more, not {}, {}, {}",
                g.latitude, g.longitude, g.accuracy
            ));
        }
        for (name, value) in &self.extra_headers {
            let token = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
            if !token || value.contains(['\r', '\n', '\0']) {
                return bad(format!("not a header Chrome can send: {name:?}: {value:?}"));
            }
        }
        if let Some(ua) = &self.user_agent
            && (ua.contains("Headless") || !ua.contains("Chrome/"))
        {
            return bad(format!(
                "user_agent must be Chrome's, without Headless, as the running Chrome's client \
                 hints say: {ua:?}"
            ));
        }
        Ok(())
    }

    /// Checks the screen can be shown at this level: a pixel ratio that
    /// isn't a whole number can only be the whole browser's, set at
    /// launch, since a screen added later is rounded to a whole ratio.
    pub(crate) fn validate_added_screen(&self) -> Result<()> {
        if let Some(scale) = self.device_scale_factor
            && scale.fract() != 0.0
        {
            return Err(Error::Invalid(format!(
                "a device_scale_factor of {scale} can only be the whole browser's, given at \
                 launch: Chrome rounds a screen added later to a whole ratio, which a frame from \
                 another site would show"
            )));
        }
        Ok(())
    }

    /// The `Accept-Language` list for the locale: the tag and its language
    /// alone ("de-DE,de"), which Chrome sends as "de-DE,de;q=0.9" for a
    /// person whose language that is.
    pub(crate) fn languages(&self) -> Option<String> {
        let locale = self.locale.as_deref()?;
        let base = locale.split('-').next().unwrap_or(locale);
        Some(if base.eq_ignore_ascii_case(locale) {
            locale.to_string()
        } else {
            format!("{locale},{base}")
        })
    }

    /// The `Accept-Language` header Chrome sends with this locale, if it
    /// sets one.
    pub fn accept_language(&self) -> Option<String> {
        let languages = self.languages()?;
        Some(
            languages
                .split(',')
                .enumerate()
                .map(|(i, l)| match i {
                    0 => l.to_string(),
                    // Chrome's q-values: 0.9, 0.8 ..., never below 0.1.
                    _ => format!("{l};q={:.1}", (10 - i.min(9)) as f64 / 10.0),
                })
                .collect::<Vec<_>>()
                .join(","),
        )
    }

    /// Whether the user agent override has more to say than Chrome's own
    /// identity: a phone, another user agent, or languages Chrome wasn't
    /// launched with (those it was are left to it: then it sends them
    /// exactly as it would for a person).
    pub(crate) fn changes_user_agent(&self, launched: Option<&str>) -> bool {
        self.phone.is_some()
            || self.user_agent.is_some()
            || self
                .languages()
                .is_some_and(|l| Some(l.as_str()) != launched)
    }

    /// The request headers whose values this environment changes from what
    /// a person's Chrome `version` (its product, such as
    /// "Chrome/154.0.8037.92") sends on the system it runs on: its user
    /// agent and client hints for a phone or another user agent, and its
    /// `Accept-Language`. For an HTTP client that's to look like the same
    /// Chrome, as a crawl's does when Chrome hands it a site's cookies.
    pub fn request_headers(&self, version: &str) -> Vec<(String, String)> {
        let full = version
            .trim_start_matches("HeadlessChrome/")
            .trim_start_matches("Chrome/");
        let mut out = Vec::new();
        if self.phone.is_some() || self.user_agent.is_some() {
            let blank = json!({"platform": ""});
            if let Ok(Some(o)) = self.user_agent_override(Some(("", &blank)), full, None) {
                let m = &o["userAgentMetadata"];
                let mobile = if m["mobile"] == true { "?1" } else { "?0" };
                out.push((
                    "User-Agent".to_string(),
                    o["userAgent"].as_str().unwrap_or_default().to_string(),
                ));
                out.push(("sec-ch-ua-mobile".into(), mobile.into()));
                out.push((
                    "sec-ch-ua-platform".into(),
                    format!("\"{}\"", m["platform"].as_str().unwrap_or_default()),
                ));
            }
        }
        if let Some(languages) = self.accept_language() {
            out.push(("Accept-Language".into(), languages));
        }
        out
    }

    /// `Network.setUserAgentOverride`'s parameters for this environment,
    /// on top of `base` (Chrome's own user agent, without Headless, and its
    /// client hints), for a Chrome whose full version is `full` (such as
    /// "154.0.8037.92") and which was launched with `launched` as its own
    /// languages, if it was. None when nothing needs overriding.
    pub(crate) fn user_agent_override(
        &self,
        base: Option<(&str, &Value)>,
        full: &str,
        launched: Option<&str>,
    ) -> Result<Option<Value>> {
        let major = full.split('.').next().unwrap_or_default();
        let languages = self.languages().filter(|l| Some(l.as_str()) != launched);
        let wanted = self.changes_user_agent(launched);
        let Some((agent, metadata)) = base else {
            if wanted {
                return Err(Error::Invalid(
                    "this browser's own client hints aren't known, so its user agent can't be \
                     changed without contradicting them"
                        .into(),
                ));
            }
            return Ok(None);
        };
        let mut agent = agent.to_string();
        let mut metadata = metadata.clone();
        let mut platform = None;
        if let Some(phone) = &self.phone {
            // Chrome's reduced user agent on Android, as every phone sends it.
            agent = format!(
                "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/{major}.0.0.0 Mobile Safari/537.36"
            );
            set_os(&mut metadata, "Android", &phone.platform_version, "", "");
            metadata["model"] = Value::from(phone.model.as_str());
            metadata["mobile"] = Value::Bool(true);
            metadata["formFactors"] = json!(["Mobile"]);
            platform = Some(PHONE_PLATFORM);
        } else if let Some(ua) = &self.user_agent {
            let named = ua
                .split("Chrome/")
                .nth(1)
                .and_then(|v| v.split('.').next())
                .unwrap_or_default();
            if named != major {
                return Err(Error::Invalid(format!(
                    "user_agent names Chrome {named}, but this is Chrome {major}: its features and \
                     client hints would say otherwise"
                )));
            }
            let (os, navigator) = os_of(ua);
            if os != metadata["platform"] {
                match os {
                    "Windows" => set_os(&mut metadata, os, "15.0.0", "x86", "64"),
                    "macOS" => set_os(&mut metadata, os, "15.0.0", "arm", "64"),
                    "Linux" => set_os(&mut metadata, os, "6.8.0", "x86", "64"),
                    "Android" => {
                        set_os(&mut metadata, os, "14.0.0", "", "");
                        metadata["model"] = Value::from("");
                    }
                    _ => {}
                }
                platform = Some(navigator);
            }
            let phone = os == "Android" && ua.contains("Mobile");
            metadata["mobile"] = Value::Bool(phone);
            metadata["formFactors"] = json!([match (os, phone) {
                ("Android", true) => "Mobile",
                ("Android", false) => "Tablet",
                _ => "Desktop",
            }]);
            agent = ua.clone();
        } else if !wanted {
            return Ok(None);
        }
        let mut params = json!({"userAgent": agent, "userAgentMetadata": metadata});
        if let Some(platform) = platform {
            params["platform"] = Value::from(platform);
        }
        if let Some(languages) = languages {
            params["acceptLanguage"] = Value::from(languages);
        }
        Ok(Some(params))
    }

    /// The CDP commands that set this environment up in a session of the
    /// given kind, beyond the user agent and what only a page's own
    /// session does (its window, screen and metrics, and geolocation).
    pub(crate) fn commands(&self, kind: Kind) -> Vec<(&'static str, Value)> {
        let mut out = Vec::new();
        let page = kind != Kind::Worker;
        if let Some(locale) = &self.locale {
            out.push(("Emulation.setLocaleOverride", json!({"locale": locale})));
        }
        if let Some(tz) = &self.timezone {
            out.push(("Emulation.setTimezoneOverride", json!({"timezoneId": tz})));
        }
        if page {
            let mut features = Vec::new();
            if let Some(c) = self.color_scheme {
                let value = match c {
                    ColorScheme::Light => "light",
                    ColorScheme::Dark => "dark",
                    ColorScheme::NoPreference => "no-preference",
                };
                features.push(json!({"name": "prefers-color-scheme", "value": value}));
            }
            if let Some(m) = self.reduced_motion {
                let value = match m {
                    ReducedMotion::Reduce => "reduce",
                    ReducedMotion::NoPreference => "no-preference",
                };
                features.push(json!({"name": "prefers-reduced-motion", "value": value}));
            }
            if !features.is_empty() {
                out.push(("Emulation.setEmulatedMedia", json!({"features": features})));
            }
            if self.has_touch == Some(true) {
                let points = self.phone.as_ref().map_or(10, |p| p.touch_points);
                out.push((
                    "Emulation.setTouchEmulationEnabled",
                    json!({"enabled": true, "maxTouchPoints": points}),
                ));
            }
        }
        if !self.extra_headers.is_empty() {
            out.push(("Network.setExtraHTTPHeaders", self.headers_param()));
        }
        if self.offline == Some(true) {
            out.extend(offline(true));
        }
        if let Some(phone) = &self.phone {
            out.push((
                "Emulation.setHardwareConcurrencyOverride",
                json!({"hardwareConcurrency": phone.cores}),
            ));
        }
        if page {
            for script in &self.init_scripts {
                out.push((
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({"source": script}),
                ));
            }
        }
        out
    }

    pub(crate) fn headers_param(&self) -> Value {
        let headers: serde_json::Map<String, Value> = self
            .extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
            .collect();
        json!({"headers": headers})
    }

    /// Whether a service worker needs its session kept to have this
    /// environment: what Chrome lets a service worker's session override.
    pub(crate) fn reaches_service_workers(&self) -> bool {
        self.locale.is_some()
            || self.timezone.is_some()
            || !self.extra_headers.is_empty()
            || self.offline == Some(true)
            || self.phone.is_some()
    }
}

/// The kinds of session an environment is set up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A page's own, or a frame's from another site.
    Page,
    /// A dedicated or service worker's.
    Worker,
}

/// What turns offline on or off, in a session: requests fail, and
/// `navigator.onLine` says so. The one without the other is what a page
/// would see in no real browser (a fetch failing while onLine is true).
pub(crate) fn offline(on: bool) -> Vec<(&'static str, Value)> {
    let conditions = if on {
        json!([{"urlPattern": "", "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1}])
    } else {
        json!([])
    };
    vec![
        (
            "Network.emulateNetworkConditionsByRule",
            json!({"offline": on, "emulateOfflineServiceWorker": on, "matchedNetworkConditions": conditions}),
        ),
        (
            "Network.overrideNetworkState",
            json!({"offline": on, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1}),
        ),
    ]
}

/// A permission's name as Chrome's `Browser.setPermission` takes it.
pub(crate) fn permission(name: &str) -> Result<Value> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
        return Err(Error::Invalid(format!(
            "not a permission's name: {name:?} (such as \"geolocation\")"
        )));
    }
    Ok(match name {
        "midi-sysex" => json!({"name": "midi", "sysex": true}),
        "push" => json!({"name": "push", "userVisibleOnly": true}),
        "camera-pan-tilt-zoom" => json!({"name": "camera", "panTiltZoom": true}),
        _ => json!({"name": name}),
    })
}

/// Whether the user agent is a phone's.
fn is_phone_agent(env: &Environment) -> bool {
    env.user_agent
        .as_deref()
        .is_some_and(|ua| ua.contains("Android") && ua.contains("Mobile"))
}

/// The client hints' platform for a user agent, and its
/// `navigator.platform`.
fn os_of(ua: &str) -> (&'static str, &'static str) {
    if ua.contains("Android") {
        ("Android", PHONE_PLATFORM)
    } else if ua.contains("Windows") {
        ("Windows", "Win32")
    } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
        ("macOS", "MacIntel")
    } else if ua.contains("CrOS") {
        ("Chrome OS", "Linux x86_64")
    } else {
        ("Linux", "Linux x86_64")
    }
}

fn set_os(metadata: &mut Value, os: &str, version: &str, arch: &str, bitness: &str) {
    metadata["platform"] = Value::from(os);
    metadata["platformVersion"] = Value::from(version);
    metadata["architecture"] = Value::from(arch);
    metadata["bitness"] = Value::from(bitness);
    metadata["wow64"] = Value::Bool(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> Value {
        json!({
            "brands": [{"brand": "Chromium", "version": "154"}],
            "fullVersionList": [{"brand": "Chromium", "version": "154.0.8037.92"}],
            "platform": "macOS", "platformVersion": "15.5.0", "architecture": "arm",
            "model": "", "mobile": false, "bitness": "64", "wow64": false,
            "formFactors": ["Desktop"],
        })
    }

    const MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                       (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36";

    #[test]
    fn a_locale_is_sent_as_chrome_sends_its_language() {
        let env = Environment {
            locale: Some("de-DE".into()),
            ..Default::default()
        };
        assert_eq!(env.languages().as_deref(), Some("de-DE,de"));
        assert_eq!(env.accept_language().as_deref(), Some("de-DE,de;q=0.9"));
        let ja = Environment {
            locale: Some("ja".into()),
            ..Default::default()
        };
        assert_eq!(ja.accept_language().as_deref(), Some("ja"));
    }

    #[test]
    fn a_phone_says_it_is_one_in_every_hint() {
        let env = Environment::device("Galaxy S23").unwrap();
        env.validate().unwrap();
        let o = env
            .user_agent_override(Some((MAC, &metadata())), "154.0.8037.92", None)
            .unwrap()
            .unwrap();
        let ua = o["userAgent"].as_str().unwrap();
        assert!(ua.contains("Android") && ua.contains("Mobile") && ua.contains("Chrome/154.0.0.0"));
        let m = &o["userAgentMetadata"];
        assert_eq!(m["mobile"], true);
        assert_eq!(m["platform"], "Android");
        assert_eq!(m["model"], "SM-S911B");
        assert_eq!(m["formFactors"], json!(["Mobile"]));
        // The brands are still the running Chrome's.
        assert_eq!(m["brands"], metadata()["brands"]);
        assert_eq!(o["platform"], PHONE_PLATFORM);
        assert!(
            env.commands(Kind::Page)
                .iter()
                .any(|(m, _)| *m == "Emulation.setTouchEmulationEnabled")
        );
    }

    #[test]
    fn nothing_contradictory_is_accepted() {
        let mobile_desktop = Environment {
            mobile: Some(true),
            ..Default::default()
        };
        assert!(mobile_desktop.validate().is_err());
        let too_big = Environment {
            viewport: Some(Size {
                width: 1920,
                height: 1080,
            }),
            screen: Some(Size {
                width: 1366,
                height: 768,
            }),
            ..Default::default()
        };
        assert!(too_big.validate().is_err());
        let other_version = Environment {
            user_agent: Some(MAC.replace("154", "120")),
            ..Default::default()
        };
        other_version.validate().unwrap();
        assert!(
            other_version
                .user_agent_override(Some((MAC, &metadata())), "154.0.8037.92", None)
                .is_err()
        );
        let headless = Environment {
            user_agent: Some(MAC.replace("Chrome/", "HeadlessChrome/")),
            ..Default::default()
        };
        assert!(headless.validate().is_err());
        let fractional = Environment {
            device_scale_factor: Some(2.625),
            ..Default::default()
        };
        fractional.validate().unwrap();
        assert!(fractional.validate_added_screen().is_err());
        assert!(Environment::device("Pixel 7").is_err());
    }

    #[test]
    fn a_windows_agent_brings_windows_hints() {
        let env = Environment {
            user_agent: Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/154.0.0.0 Safari/537.36"
                    .into(),
            ),
            ..Default::default()
        };
        let o = env
            .user_agent_override(Some((MAC, &metadata())), "154.0.8037.92", None)
            .unwrap()
            .unwrap();
        assert_eq!(o["userAgentMetadata"]["platform"], "Windows");
        assert_eq!(o["userAgentMetadata"]["architecture"], "x86");
        assert_eq!(o["platform"], "Win32");
        assert_eq!(o["userAgentMetadata"]["mobile"], false);
    }

    #[test]
    fn a_page_overrides_its_context_and_adds_to_it() {
        let context = Environment {
            locale: Some("fr-FR".into()),
            init_scripts: vec!["a".into()],
            extra_headers: vec![("X-A".into(), "1".into()), ("X-B".into(), "1".into())],
            ..Default::default()
        };
        let page = Environment {
            timezone: Some("Asia/Tokyo".into()),
            init_scripts: vec!["b".into()],
            extra_headers: vec![("x-a".into(), "2".into())],
            ..Default::default()
        };
        let both = page.over(&context);
        assert_eq!(both.locale.as_deref(), Some("fr-FR"));
        assert_eq!(both.timezone.as_deref(), Some("Asia/Tokyo"));
        assert_eq!(both.init_scripts, ["a", "b"]);
        assert_eq!(
            both.extra_headers,
            [
                ("X-B".to_string(), "1".to_string()),
                ("x-a".into(), "2".into())
            ]
        );
    }

    #[test]
    fn an_http_client_can_say_what_the_page_said() {
        let mut env = Environment::device("Galaxy S23").unwrap();
        env.locale = Some("pt-BR".into());
        let headers = env.request_headers("Chrome/154.0.8037.92");
        let get = |n: &str| {
            headers
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.as_str())
        };
        assert!(get("User-Agent").unwrap().contains("Android"));
        assert_eq!(get("sec-ch-ua-mobile"), Some("?1"));
        assert_eq!(get("sec-ch-ua-platform"), Some("\"Android\""));
        assert_eq!(get("Accept-Language"), Some("pt-BR,pt;q=0.9"));
        assert!(
            Environment::default()
                .request_headers("Chrome/154.0.8037.92")
                .is_empty()
        );
    }

    #[test]
    fn nothing_is_overridden_for_nothing() {
        let o = Environment::default()
            .user_agent_override(Some((MAC, &metadata())), "154.0.8037.92", None)
            .unwrap();
        assert_eq!(o, None);
    }
}
