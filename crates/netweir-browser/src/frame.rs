//! A frame of a page: the page's own document, or one in an `<iframe>`.
//! Everything that finds elements by selector runs in a frame, in
//! netweir's isolated world for that frame's document.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::browser::str_of;
use crate::page::{NOTCH, Page};
use crate::{Error, Result};

/// netweir's helpers, installed in each document's isolated world. The
/// page can't see them: the world shares the DOM, not the globals.
const HELPERS: &str = include_str!("helpers.js");

/// How many isolated worlds a page keeps track of. Past that (a page that
/// has had hundreds of frames), the record starts again, and worlds are
/// made afresh as they're needed.
const WORLDS: usize = 64;

/// Which options [`Frame::select_option`] picks.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    Values(Vec<String>),
    /// By the text the option shows (its `label`, or its text).
    Labels(Vec<String>),
    Indexes(Vec<u32>),
}

/// A frame of a page. Cheap to clone. Its name and URL are as they were
/// when it was found; its actions work on the document it shows now.
#[derive(Clone)]
pub struct Frame {
    page: Page,
    /// The session that reached it when it was found: the page's, or, for
    /// a frame Chrome runs in a process of its own, that frame's. A frame
    /// that moves to another site, or back, moves process and session, so
    /// [`Frame::session`] looks again.
    session: String,
    id: String,
    parent: Option<String>,
    name: String,
    url: String,
}

impl Frame {
    pub(crate) fn new(
        page: Page,
        session: String,
        id: String,
        parent: Option<String>,
        name: String,
        url: String,
    ) -> Frame {
        Frame {
            page,
            session,
            id,
            parent,
            name,
            url,
        }
    }

    /// Chrome's id for the frame.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The frame's `name` attribute; empty for the page's own.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The id of the frame it's in; None for the page's own.
    pub fn parent_id(&self) -> Option<&str> {
        self.parent.as_deref()
    }

    pub fn page(&self) -> &Page {
        &self.page
    }

    fn is_main(&self) -> bool {
        self.parent.is_none() && self.session == self.page.inner.session
    }

    /// The session that reaches the frame now. A frame Chrome has just
    /// moved to a process of its own has a session named for it; one whose
    /// session has gone (it came back to the page's process, say) is
    /// looked for in the frame tree.
    async fn session(&self) -> Result<String> {
        if self.is_main() {
            return Ok(self.session.clone());
        }
        {
            let state = self.page.state();
            // The newest, if a frame moved from one other site to another.
            if let Some((s, _)) = state.children.iter().rfind(|(_, f)| *f == self.id) {
                return Ok(s.clone());
            }
            let alive = self.session == self.page.inner.session
                || state.children.iter().any(|(s, _)| *s == self.session);
            if alive {
                return Ok(self.session.clone());
            }
        }
        self.find().await
    }

    /// The session the frame tree has the frame in now; Gone once it has
    /// been removed from the page.
    async fn find(&self) -> Result<String> {
        let frames = self.page.frames().await?;
        match frames.into_iter().find(|f| f.id == self.id) {
            Some(f) => Ok(f.session),
            None => Err(Error::Gone(format!(
                "the frame at {} isn't in the page any more",
                self.url
            ))),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let session = self.session().await?;
        self.page.call_on(&session, method, params).await
    }

    async fn call_unbounded(&self, method: &str, params: Value) -> Result<Value> {
        let session = self.session().await?;
        self.page.call_unbounded_on(&session, method, params).await
    }

    /// The isolated world of the frame's document, made (with the helpers
    /// in it) the first time it's needed. The main frame's goes with its
    /// document; another frame's is made again once it's found gone.
    async fn world(&self, fresh: bool) -> Result<i64> {
        let mut worlds = self.page.inner.worlds.lock().await;
        let loader = if self.is_main() {
            self.page.state().loader.clone()
        } else {
            String::new()
        };
        let mut session = self.session().await?;
        if let Some((l, id)) = worlds.get(&(session.clone(), self.id.clone()))
            && *l == loader
            && !fresh
        {
            return Ok(*id);
        }
        let create = json!({"frameId": self.id});
        let r = match self
            .page
            .call_on(&session, "Page.createIsolatedWorld", create.clone())
            .await
        {
            Ok(r) => r,
            // Between processes, or gone: the frame tree says which.
            Err(Error::Protocol { .. }) if !self.is_main() => {
                let found = self.find().await?;
                if found == session {
                    return Err(Error::Gone(format!(
                        "the frame at {} isn't in the page any more",
                        self.url
                    )));
                }
                session = found;
                self.page
                    .call_on(&session, "Page.createIsolatedWorld", create)
                    .await?
            }
            Err(e) => return Err(e),
        };
        let key = (session, self.id.clone());
        let id = r["executionContextId"].as_i64().unwrap_or_default();
        self.run(Some(id), HELPERS, true).await?;
        if worlds.len() >= WORLDS {
            worlds.clear();
        }
        worlds.insert(key, (loader, id));
        Ok(id)
    }

    /// Runs `expression` in netweir's isolated world, awaiting a promise.
    async fn helper(&self, expression: &str) -> Result<Value> {
        let id = self.world(false).await?;
        match self.run(Some(id), expression, true).await {
            // The document changed under it, or the frame moved to another
            // process; once more in the new one.
            Err(Error::Protocol { message, .. })
                if message.contains("context") || !self.is_main() =>
            {
                let id = self.world(true).await?;
                self.run(Some(id), expression, true).await
            }
            other => other,
        }
    }

    /// `Runtime.evaluate`, which works without `Runtime.enable`. With no
    /// context, it runs in the main world of the session's own frame.
    /// `bounded` holds it to the browser's timeout.
    async fn run(&self, context: Option<i64>, expression: &str, bounded: bool) -> Result<Value> {
        let mut params = json!({
            "expression": expression,
            "awaitPromise": true,
            "returnByValue": true,
        });
        if let Some(id) = context {
            params["contextId"] = Value::from(id);
        }
        let r = if bounded {
            self.call("Runtime.evaluate", params).await?
        } else {
            self.call_unbounded("Runtime.evaluate", params).await?
        };
        value_of(&r)
    }

    /// Lets go of an object Chrome holds for netweir.
    async fn release(&self, object: &str) {
        let _ = self
            .call("Runtime.releaseObject", json!({"objectId": object}))
            .await;
    }

    /// Runs `js` in the frame's main world, where the page's own variables
    /// are: an expression, or a function, which is called. A promise is
    /// awaited. The result comes back as JSON.
    pub async fn evaluate(&self, js: &str) -> Result<Value> {
        let body =
            format!("const v = ({js}\n); return typeof v === 'function' ? await v() : await v;");
        if self.is_main() {
            // No timeout of its own, as documented: the caller's script may
            // rightly take as long as it likes.
            return self
                .run(None, &format!("(async () => {{ {body} }})()"), false)
                .await;
        }
        // Only a session's own frame has a main world Runtime.evaluate can
        // name without Runtime.enable. Any other is reached through its
        // document: found in netweir's world, and resolved again in the
        // main one, where the function is called on it.
        let document = self.main_world_document().await?;
        let r = self
            .call_unbounded(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": document,
                    "functionDeclaration": format!("async function() {{ {body} }}"),
                    "awaitPromise": true,
                    "returnByValue": true,
                }),
            )
            .await;
        self.release(&document).await;
        value_of(&r?)
    }

    /// The frame's document, as an object in its main world.
    async fn main_world_document(&self) -> Result<String> {
        let mut fresh = false;
        loop {
            let id = self.world(fresh).await?;
            let found = self
                .call(
                    "Runtime.evaluate",
                    json!({"expression": "document", "contextId": id}),
                )
                .await;
            let object = match found {
                Ok(r) => str_of(&r["result"], "objectId"),
                Err(Error::Protocol { .. }) if !fresh => {
                    fresh = true;
                    continue;
                }
                Err(e) => return Err(e),
            };
            let node = self
                .call("DOM.describeNode", json!({"objectId": object}))
                .await;
            self.release(&object).await;
            let backend = node?["node"]["backendNodeId"].clone();
            let r = self
                .call("DOM.resolveNode", json!({"backendNodeId": backend}))
                .await?;
            return Ok(str_of(&r["object"], "objectId"));
        }
    }

    /// The document as Chrome now has it, doctype included.
    pub async fn content(&self) -> Result<String> {
        let html = self.helper("__netweir.content()").await?;
        Ok(html.as_str().unwrap_or_default().to_string())
    }

    pub async fn title(&self) -> Result<String> {
        Ok(self
            .helper("document.title")
            .await?
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// The frames between this one and the page's own, innermost first:
    /// each by its id, with the frame whose `<iframe>` holds it.
    async fn ancestry(&self) -> Result<Vec<(String, Frame)>> {
        if self.parent.is_none() {
            return Ok(Vec::new());
        }
        let frames = self.page.frames().await?;
        let mut chain: Vec<(String, Frame)> = Vec::new();
        let mut child = self.clone();
        while let Some(parent) = child.parent.clone() {
            let Some(found) = frames.iter().find(|f| f.id == parent) else {
                return Err(Error::Gone(format!(
                    "the frame at {} isn't in the page any more",
                    self.url
                )));
            };
            // A frame can't be its own ancestor; Chrome would never say so,
            // but a loop here would never end.
            if chain.len() > frames.len() {
                break;
            }
            chain.push((child.id.clone(), found.clone()));
            child = found.clone();
        }
        Ok(chain)
    }

    /// Where the point (x, y) of this frame's viewport is in the page's,
    /// where the mouse goes, through each `<iframe>` holding it; None if,
    /// with `hit`, one of those is covered there.
    async fn to_page(
        chain: &[(String, Frame)],
        mut x: f64,
        mut y: f64,
        hit: bool,
    ) -> Result<Option<(f64, f64)>> {
        for (child, parent) in chain {
            let owner = parent.owner(child).await?;
            let r = parent
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": owner,
                        "functionDeclaration": "function(x, y, hit) { return __netweir.offset(this, x, y, hit); }",
                        "arguments": [{"value": x}, {"value": y}, {"value": hit}],
                        "returnByValue": true,
                    }),
                )
                .await;
            parent.release(&owner).await;
            let r = value_of(&r?)?;
            if r.get("failed").is_some() {
                return Ok(None);
            }
            x = r["x"].as_f64().unwrap_or(0.0);
            y = r["y"].as_f64().unwrap_or(0.0);
        }
        Ok(Some((x, y)))
    }

    /// The `<iframe>` in this frame that holds the frame `child`, as an
    /// object in netweir's world, where its helpers can work on it.
    async fn owner(&self, child: &str) -> Result<String> {
        let owner = self
            .call("DOM.getFrameOwner", json!({"frameId": child}))
            .await?;
        let mut fresh = false;
        loop {
            let id = self.world(fresh).await?;
            match self
                .call(
                    "DOM.resolveNode",
                    json!({"backendNodeId": owner["backendNodeId"], "executionContextId": id}),
                )
                .await
            {
                Ok(r) => return Ok(str_of(&r["object"], "objectId")),
                Err(Error::Protocol { .. }) if !fresh => fresh = true,
                Err(e) => return Err(e),
            }
        }
    }

    /// Runs the helper `call` until it stops reporting a check that failed,
    /// and returns what it says then. One that reports an error fails at
    /// once. With `point`, its x and y are moved into the page's viewport,
    /// where the mouse goes, and the point must reach this frame if
    /// `point` is true; that's a check too.
    async fn attempt(
        &self,
        action: &str,
        selector: &str,
        call: &str,
        point: Option<bool>,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let deadline = Instant::now() + self.page.timeout(timeout);
        let chain = match point {
            Some(_) => self.ancestry().await?,
            None => Vec::new(),
        };
        let mut pause = Duration::from_millis(20);
        loop {
            let mut r = self.helper(call).await?;
            if let Some(e) = r["error"].as_str() {
                return Err(Error::Script(format!("{action} {selector}: {e}")));
            }
            let mut failed = r["failed"].as_str().map(str::to_string);
            if failed.is_none()
                && let Some(hit) = point
            {
                let (x, y) = (
                    r["x"].as_f64().unwrap_or(0.0),
                    r["y"].as_f64().unwrap_or(0.0),
                );
                match Frame::to_page(&chain, x, y, hit).await? {
                    Some((x, y)) => {
                        r["x"] = Value::from(x);
                        r["y"] = Value::from(y);
                    }
                    None => failed = Some("receives events".into()),
                }
            }
            let Some(failed) = failed else {
                return Ok(r);
            };
            if Instant::now() + pause >= deadline {
                let why = match r["why"].as_str() {
                    Some(why) => why.to_string(),
                    None => never(&failed).to_string(),
                };
                return Err(Error::Timeout(format!("{action} {selector}: {why}")));
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(200));
        }
    }

    /// Waits until the element `selector` finds passes `checks`, scrolling
    /// it into view first if `scroll`. Returns its centre in the page's
    /// viewport.
    async fn actionable(
        &self,
        action: &str,
        selector: &str,
        checks: &[&str],
        scroll: bool,
        timeout: Option<Duration>,
    ) -> Result<(f64, f64)> {
        let call = format!(
            "__netweir.check({}, {}, {scroll})",
            Value::from(selector),
            json!(checks)
        );
        let hit = checks.contains(&"receives events");
        let r = self
            .attempt(action, selector, &call, Some(hit), timeout)
            .await?;
        Ok((
            r["x"].as_f64().unwrap_or(0.0),
            r["y"].as_f64().unwrap_or(0.0),
        ))
    }

    /// Clicks the element's centre with the mouse, once it's visible,
    /// still, enabled and not covered.
    pub async fn click(&self, selector: &str, timeout: Option<Duration>) -> Result<()> {
        self.click_as("click", selector, timeout).await
    }

    /// `click`, its errors naming `action`, the action it's part of.
    async fn click_as(
        &self,
        action: &str,
        selector: &str,
        timeout: Option<Duration>,
    ) -> Result<()> {
        let checks = [
            "attached",
            "visible",
            "stable",
            "enabled",
            "receives events",
        ];
        let (x, y) = self
            .actionable(action, selector, &checks, true, timeout)
            .await?;
        self.page.mouse_click(x, y).await
    }

    /// Moves the mouse to the element's centre, once it's visible, still
    /// and not covered, so the page sees it come over the element.
    pub async fn hover(&self, selector: &str, timeout: Option<Duration>) -> Result<()> {
        let checks = ["attached", "visible", "stable", "receives events"];
        let (x, y) = self
            .actionable("hover", selector, &checks, true, timeout)
            .await?;
        self.page.mouse_move(x, y).await
    }

    /// Replaces the element's contents with `text`, as typing would.
    pub async fn fill(&self, selector: &str, text: &str, timeout: Option<Duration>) -> Result<()> {
        let checks = ["attached", "visible", "enabled", "editable"];
        let call = format!(
            "__netweir.check({}, {}, true)",
            Value::from(selector),
            json!(checks)
        );
        self.attempt("fill", selector, &call, None, timeout).await?;
        let focused = self
            .helper(&format!("__netweir.focus({})", Value::from(selector)))
            .await?;
        if focused != Value::Bool(true) {
            return Err(Error::Script(format!(
                "fill {selector}: it couldn't be focused"
            )));
        }
        if text.is_empty() {
            self.page.press("Delete").await
        } else {
            self.page
                .call_on(
                    &self.page.inner.session,
                    "Input.insertText",
                    json!({"text": text}),
                )
                .await?;
            Ok(())
        }
    }

    /// Waits until `selector` is "attached", "visible", "hidden" (or gone)
    /// or "detached".
    pub async fn wait_for(
        &self,
        selector: &str,
        state: &str,
        timeout: Option<Duration>,
    ) -> Result<()> {
        if !["attached", "visible", "hidden", "detached"].contains(&state) {
            return Err(Error::Invalid(format!(
                "state must be \"attached\", \"visible\", \"hidden\" or \"detached\", not {state:?}"
            )));
        }
        let deadline = Instant::now() + self.page.timeout(timeout);
        let call = format!("__netweir.state({})", Value::from(selector));
        let mut pause = Duration::from_millis(20);
        loop {
            let now = self.helper(&call).await?;
            let now = now.as_str().unwrap_or("detached");
            let done = match state {
                "attached" => now != "detached",
                "visible" => now == "visible",
                "hidden" => now != "visible",
                _ => now == "detached",
            };
            if done {
                return Ok(());
            }
            if Instant::now() + pause >= deadline {
                return Err(Error::Timeout(format!(
                    "waiting for {selector} to be {state}"
                )));
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(200));
        }
    }

    /// Chooses the options `pick` names in the `<select>` `selector` finds,
    /// waiting for it to be visible and enabled and for the options to be
    /// there. Returns the values selected.
    pub async fn select_option(
        &self,
        selector: &str,
        pick: &Pick,
        timeout: Option<Duration>,
    ) -> Result<Vec<String>> {
        let (by, wanted) = match pick {
            Pick::Values(v) => ("value", json!(v)),
            Pick::Labels(v) => ("label", json!(v)),
            Pick::Indexes(v) => ("index", json!(v)),
        };
        let call = format!(
            "__netweir.select({}, {}, {wanted})",
            Value::from(selector),
            Value::from(by)
        );
        let r = self
            .attempt("select_option", selector, &call, None, timeout)
            .await?;
        Ok(r["values"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect())
    }

    /// Checks (`on`) or unchecks a checkbox or radio button by clicking it,
    /// if it isn't that way already, and makes sure the click did it. A
    /// box that has to change waits to be enabled first.
    pub async fn set_checked(
        &self,
        selector: &str,
        on: bool,
        timeout: Option<Duration>,
    ) -> Result<()> {
        let action = if on { "check" } else { "uncheck" };
        let deadline = Instant::now() + self.page.timeout(timeout);
        let call = format!("__netweir.checked({}, {on})", Value::from(selector));
        let r = self.attempt(action, selector, &call, None, timeout).await?;
        if r["checked"] == on {
            return Ok(());
        }
        if !on && r["radio"] == true {
            return Err(Error::Script(format!(
                "uncheck {selector}: a radio button is unchecked by checking another in its group"
            )));
        }
        match self.click_as(action, selector, left(deadline)).await {
            // The usual reason: a styled box in front of a hidden real one.
            Err(Error::Timeout(m)) if m.ends_with(never("visible")) => {
                return Err(Error::Timeout(format!(
                    "{m}; if the page hides the real box behind a styled one, {action} its <label> \
                     instead"
                )));
            }
            other => other?,
        }
        let r = self
            .helper(&format!(
                "__netweir.checked({}, null)",
                Value::from(selector)
            ))
            .await?;
        if r["checked"] != on {
            return Err(Error::Script(format!(
                "{action} {selector}: clicking it didn't {action} it"
            )));
        }
        Ok(())
    }

    /// Gives the `<input type=file>` `selector` finds these files, as
    /// choosing them would; the page hears input and change. No files
    /// clears it.
    pub async fn set_input_files(
        &self,
        selector: &str,
        files: &[PathBuf],
        timeout: Option<Duration>,
    ) -> Result<()> {
        let mut paths = Vec::new();
        for file in files {
            // Chrome wants whole paths, and says nothing of a missing file
            // until the page reads it.
            let path = std::fs::canonicalize(file)
                .map_err(|_| Error::Invalid(format!("no file at {}", file.display())))?;
            if !path.is_file() {
                return Err(Error::Invalid(format!(
                    "{} is a directory, not a file",
                    file.display()
                )));
            }
            paths.push(path_text(&path));
        }
        let call = format!(
            "__netweir.fileInput({}, {})",
            Value::from(selector),
            paths.len()
        );
        self.attempt("set_input_files", selector, &call, None, timeout)
            .await?;
        if paths.is_empty() {
            // Chrome won't set no files; emptying the value is how a page
            // clears one itself.
            self.helper(&format!("__netweir.clearFiles({})", Value::from(selector)))
                .await?;
            return Ok(());
        }
        let id = self.world(false).await?;
        let r = self
            .call(
                "Runtime.evaluate",
                json!({"expression": format!("document.querySelector({})", Value::from(selector)), "contextId": id}),
            )
            .await?;
        let input = str_of(&r["result"], "objectId");
        if input.is_empty() {
            return Err(Error::Script(format!(
                "set_input_files {selector}: it went away"
            )));
        }
        let set = self
            .call(
                "DOM.setFileInputFiles",
                json!({"files": paths, "objectId": input}),
            )
            .await;
        self.release(&input).await;
        set.map(|_| ())
    }

    /// Scrolls with the mouse wheel, as a person would. With a selector
    /// and no distance, wheels the element into view; with a distance,
    /// wheels it over the element (or over the page, without one) by `x`
    /// and `y` pixels. With neither, scrolls to the bottom, waits up to
    /// `idle` for more to load, and goes on while more does, at most
    /// `times` times. Returns how many times more loaded. `timeout` is for
    /// the whole of it.
    pub async fn scroll(
        &self,
        selector: Option<&str>,
        x: f64,
        y: Option<f64>,
        times: Option<u32>,
        idle: Duration,
        timeout: Option<Duration>,
    ) -> Result<u32> {
        let deadline = Instant::now() + self.page.timeout(timeout);
        let (dx, dy) = (x, y.unwrap_or(0.0));
        match selector {
            None if y.is_none() && x == 0.0 => {
                // `times` is a limit of its own; only a timeout the caller
                // gave cuts it short.
                let deadline = (times.is_none() || timeout.is_some()).then_some(deadline);
                self.to_bottom(times, idle, deadline).await
            }
            Some(selector) => {
                let (px, py) = self.bring_into_view(selector, deadline).await?;
                if dx != 0.0 || dy != 0.0 {
                    self.wheel(Some(selector), px, py, dx, dy).await?;
                }
                Ok(0)
            }
            None => {
                let (px, py) = self.page_point(0, Some(deadline)).await?;
                self.wheel(None, px, py, dx, dy).await?;
                Ok(0)
            }
        }
    }

    /// Where a wheel scrolls the frame's document, in the page's viewport:
    /// the middle, unless a box that scrolls by itself is there, which
    /// would take the wheel instead. `skip` passes over that many places,
    /// for when one moved nothing.
    async fn page_point(&self, skip: u32, deadline: Option<Instant>) -> Result<(f64, f64)> {
        let r = self
            .attempt(
                "scroll",
                "the page",
                &format!("__netweir.pagePoint({skip})"),
                Some(false),
                deadline.and_then(left),
            )
            .await?;
        Ok((
            r["x"].as_f64().unwrap_or(0.0),
            r["y"].as_f64().unwrap_or(0.0),
        ))
    }

    /// Turns the wheel at (x, y) by (dx, dy) and waits for the scroll to
    /// finish. Returns whether anything moved: the page, whatever scrolls
    /// under the wheel, or the element `selector` finds.
    async fn wheel(
        &self,
        selector: Option<&str>,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    ) -> Result<bool> {
        let selector = selector.map_or(Value::Null, Value::from);
        let before = self.helper(&format!("__netweir.where({selector})")).await?;
        self.page.mouse_wheel(x, y, dx, dy).await?;
        let moved = self
            .helper(&format!("__netweir.settle({selector}, {before})"))
            .await?;
        Ok(moved == true)
    }

    /// Wheels the element into view: over the box that scrolls it, by as
    /// much as brings it to the middle, until it's there. Where the wheel
    /// can't move it (the box that holds it is out of view, say), it's
    /// scrolled into view as click does. Returns its middle in the page's
    /// viewport.
    async fn bring_into_view(&self, selector: &str, deadline: Instant) -> Result<(f64, f64)> {
        let call = format!("__netweir.aim({})", Value::from(selector));
        let reveal = format!("__netweir.reveal({})", Value::from(selector));
        let mut revealed = false;
        // A few turns of the wheel bring most things into view; past that,
        // something keeps it from moving the way the wheel goes.
        for turn in 0..=20 {
            if turn == 20 && !revealed {
                self.helper(&reveal).await?;
                revealed = true;
            }
            let r = self
                .attempt("scroll", selector, &call, Some(false), left(deadline))
                .await?;
            let (x, y) = (
                r["x"].as_f64().unwrap_or(0.0),
                r["y"].as_f64().unwrap_or(0.0),
            );
            if r["done"] == true || revealed {
                return Ok((x, y));
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(format!(
                    "scroll {selector}: it never came into view"
                )));
            }
            let (dx, dy) = (
                r["dx"].as_f64().unwrap_or(0.0),
                r["dy"].as_f64().unwrap_or(0.0),
            );
            if r["stuck"] == true || !self.wheel(Some(selector), x, y, dx, dy).await? {
                self.helper(&reveal).await?;
                revealed = true;
            }
        }
        unreachable!("the last turn returns")
    }

    /// Wheels down to the bottom, then waits up to `idle` for the page to
    /// grow; while it does, goes round again, up to `times` times. Returns
    /// how many rounds the page grew in. Without a deadline, each step is
    /// still bounded: every call to Chrome by the browser's timeout, each
    /// wait for more by `idle`, and the trip down by the stall limit.
    async fn to_bottom(
        &self,
        times: Option<u32>,
        idle: Duration,
        deadline: Option<Instant>,
    ) -> Result<u32> {
        if times == Some(0) {
            return Ok(0);
        }
        let height = |m: &Value| m["height"].as_f64().unwrap_or(0.0);
        let past = || deadline.is_some_and(|d| Instant::now() >= d);
        // Out of time on the way down, or waiting at the bottom for more.
        let late = |rounds: u32, waiting: bool| Error::ScrollTimeout {
            message: match (waiting, rounds) {
                (false, 0) => "scrolling to the bottom: still scrolling down, with no rounds \
                               finished"
                    .to_string(),
                (false, n) => format!(
                    "scrolling to the bottom: still scrolling down after {n} rounds of more loaded"
                ),
                (true, n) => format!(
                    "scrolling to the bottom: still waiting for more after {n} rounds of more \
                     loaded"
                ),
            },
            rounds,
        };
        let mut grew = 0;
        loop {
            let mut m = self.helper("__netweir.metrics()").await?;
            let start = height(&m);
            // To where the bottom was when the round began: what the page
            // adds on the way is the next round's, so each round is one
            // trip to the bottom.
            let max = m["max"].as_f64().unwrap_or(0.0);
            // Turns in a row that moved something, but not the page: a box
            // or a frame under the wheel that won't pass the scroll on.
            let mut stalled = 0;
            // Whether the last sliver has been scrolled by script.
            let mut nudged = false;
            loop {
                let at = m["y"].as_f64().unwrap_or(0.0);
                if at >= max - 1.0 {
                    break;
                }
                if past() {
                    return Err(late(grew, false));
                }
                // Whole notches, as a mouse wheel turns: about a window's
                // worth, and at the end as many as cover what's left, never
                // a fraction of one. How far a notch scrolls is Chrome's to
                // say (on Windows it differs from the delta), so a turn cut
                // to land exactly can come up short, and the sliver of a
                // turn after it can move nothing.
                let view = (m["view"].as_f64().unwrap_or(600.0) / NOTCH).floor();
                let left = ((max - at) / NOTCH).ceil();
                let step = view.min(left).max(1.0) * NOTCH;
                // The place is picked again each turn, since the page moves
                // what's under it; where a turn moves nothing, a few others
                // are tried before the page is taken to have stopped.
                let mut moved = false;
                for skip in 0..SPOTS {
                    let (x, y) = self.page_point(skip, deadline).await?;
                    if self.wheel(None, x, y, 0.0, step).await? {
                        moved = true;
                        break;
                    }
                }
                if !moved {
                    // The wheel may not take the page its last few pixels:
                    // on Windows CI the last notch, with 8 px to go, moved
                    // nothing, though a script took it the rest of the way.
                    // Something waiting at the very bottom has to see the
                    // page get there, so that sliver (less than a notch, as
                    // far as the page really goes) is scrolled by script, as
                    // click scrolls, which a page can tell apart. Once per
                    // trip, after any scroll still running has finished, so
                    // it can't carry the page back.
                    m = self.helper("__netweir.metrics()").await?;
                    let (now, end) = (
                        m["y"].as_f64().unwrap_or(0.0),
                        m["max"].as_f64().unwrap_or(0.0),
                    );
                    if !nudged && end - now > 0.5 && end - now < NOTCH {
                        nudged = true;
                        self.helper("__netweir.settle(null, __netweir.where(null))")
                            .await?;
                        self.helper("__netweir.toEnd()").await?;
                        self.helper("__netweir.settle(null, __netweir.where(null))")
                            .await?;
                        m = self.helper("__netweir.metrics()").await?;
                        continue;
                    }
                    // Otherwise as far as the browser will take the page.
                    // Known limit: a page that scrolls in a box of its own,
                    // not as a whole, isn't scrolled by this; wheel the box
                    // with a selector instead.
                    break;
                }
                m = self.helper("__netweir.metrics()").await?;
                if m["y"].as_f64().unwrap_or(0.0) > at {
                    stalled = 0;
                } else {
                    stalled += 1;
                    if stalled >= STALLED {
                        break;
                    }
                }
            }
            // Waiting for what the page fetches as it comes into view: the
            // only sign there'll be no more is that none comes.
            let quiet = Instant::now() + idle;
            while height(&m) <= start && Instant::now() < quiet {
                if past() {
                    return Err(late(grew, true));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                m = self.helper("__netweir.metrics()").await?;
            }
            if height(&m) <= start {
                return Ok(grew);
            }
            grew += 1;
            if times.is_some_and(|t| grew >= t) {
                return Ok(grew);
            }
        }
    }
}

/// How many places a turn of the wheel is tried at before the page is
/// taken to have stopped scrolling.
const SPOTS: u32 = 4;

/// How many turns in a row may move something under the wheel but not the
/// page before scrolling to the bottom stops: each turn is up to a viewport,
/// so enough to run a box or a frame dozens of screens tall to its end,
/// where its scroll passes on to the page.
const STALLED: u32 = 40;

/// What's left until `deadline`, as a timeout for one step of a longer
/// action; never quite nothing, so a step past it fails on its own terms.
fn left(deadline: Instant) -> Option<Duration> {
    Some(
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1)),
    )
}

/// The value a `Runtime.evaluate` or `Runtime.callFunctionOn` returned, or
/// the error the script threw.
fn value_of(r: &Value) -> Result<Value> {
    if let Some(details) = r.get("exceptionDetails") {
        let message = details["exception"]["description"]
            .as_str()
            .or_else(|| details["text"].as_str())
            .unwrap_or("script error");
        return Err(Error::Script(message.to_string()));
    }
    let result = &r["result"];
    // NaN, Infinity, -0 and BigInts aren't JSON; they come as text.
    if let Some(text) = result["unserializableValue"].as_str() {
        return Ok(Value::from(text.trim_end_matches('n')));
    }
    Ok(result.get("value").cloned().unwrap_or(Value::Null))
}

/// A path as Chrome takes it. Windows' canonical paths start `\\?\`,
/// which Chrome doesn't read as a file.
fn path_text(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
        _ => text,
    }
}

/// How a failed check reads in a timeout.
fn never(check: &str) -> &'static str {
    match check {
        "attached" => "nothing matched it",
        "visible" => "it never became visible",
        "stable" => "it never stopped moving",
        "enabled" => "it never became enabled",
        "editable" => "it never became editable",
        "receives events" => "something else was always at its centre, covering it",
        _ => "it never became actionable",
    }
}
