//! What a page tells its listeners: what it fetches, and the dialogs,
//! popups and downloads it starts. The design of the last three is in
//! docs/design/dialogs.md.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::browser::str_of;
use crate::conn::Connection;
use crate::network::went_away;
use crate::{Error, Page, Request, Response, Result};

/// What the page told its listeners about.
#[derive(Clone)]
pub enum PageEvent {
    Request(Request),
    Response(Response),
    /// A page it opened: `window.open()`, or a link with a target. It's
    /// running, with the opener's rules, on whatever document Chrome
    /// shows first; [`Page::wait_for_load`] waits for the one it was
    /// opened for.
    Popup(Page),
    Download(Download),
}

/// An `alert()`, `confirm()`, `prompt()` or `beforeunload` the page is
/// waiting on, from [`Page::dialogs`]. Answered once; one nobody answers,
/// dropped or held past the browser's timeout, is dismissed
/// (`beforeunload` is accepted). No answer reaches the page sooner than a
/// person's would.
#[derive(Clone)]
pub struct Dialog {
    inner: Arc<DialogInner>,
}

struct DialogInner {
    kind: String,
    message: String,
    default_value: String,
    url: String,
    conn: Connection,
    session: String,
    timeout: Duration,
    /// A moment after it opened: no answer goes sooner.
    not_before: Instant,
    answered: AtomicBool,
    runtime: tokio::runtime::Handle,
}

/// How long a person takes, at least, to read a dialog and answer it:
/// half a second, and up to a second more, at random.
fn pause() -> Duration {
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    Duration::from_millis(500 + random % 1000)
}

impl Dialog {
    /// For `Page.javascriptDialogOpening` on `session`.
    pub(crate) fn opened(
        conn: &Connection,
        session: &str,
        params: &Value,
        timeout: Duration,
        runtime: &tokio::runtime::Handle,
    ) -> Dialog {
        let dialog = Dialog {
            inner: Arc::new(DialogInner {
                kind: str_of(params, "type"),
                message: str_of(params, "message"),
                default_value: str_of(params, "defaultPrompt"),
                url: str_of(params, "url"),
                conn: conn.clone(),
                session: session.to_string(),
                timeout,
                not_before: Instant::now() + pause(),
                answered: AtomicBool::new(false),
                runtime: runtime.clone(),
            }),
        };
        // A listener that never answers gets the browser's timeout.
        let waiting = Arc::downgrade(&dialog.inner);
        runtime.spawn(async move {
            tokio::time::sleep(timeout).await;
            if let Some(dialog) = waiting.upgrade() {
                dialog.release();
            }
        });
        dialog
    }

    /// "alert", "confirm", "prompt" or "beforeunload".
    pub fn kind(&self) -> &str {
        &self.inner.kind
    }

    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// The text a prompt offers; empty for the others.
    pub fn default_value(&self) -> &str {
        &self.inner.default_value
    }

    /// The URL of the frame that opened it.
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    pub fn is_answered(&self) -> bool {
        self.inner.answered.load(Ordering::Relaxed)
    }

    /// Presses OK, with `text` in a prompt's box (its default if None);
    /// for `beforeunload`, leaves. It counts as answered from the moment
    /// this is called; the answer reaches the page once a person could
    /// have given it.
    pub fn accept(&self, text: Option<&str>) -> impl Future<Output = Result<()>> + Send + 'static {
        self.answer(true, text)
    }

    /// Presses Cancel; for `beforeunload`, stays.
    pub fn dismiss(&self) -> impl Future<Output = Result<()>> + Send + 'static {
        self.answer(false, None)
    }

    /// Answers it the default way now, unless it's been answered.
    pub fn release(&self) {
        self.inner.release();
    }

    fn answer(
        &self,
        accept: bool,
        text: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let inner = self.inner.clone();
        let answered = inner.answered.swap(true, Ordering::Relaxed);
        let mut params = json!({"accept": accept});
        if let Some(text) = text {
            params["promptText"] = Value::from(text);
        }
        async move {
            if answered {
                return Err(Error::Gone(format!(
                    "the {} \"{}\" was already answered, or answered the default way when \
                     nobody had in time",
                    inner.kind, inner.message
                )));
            }
            tokio::time::sleep_until(inner.not_before).await;
            match inner
                .conn
                .call_within(
                    inner.timeout,
                    &inner.session,
                    "Page.handleJavaScriptDialog",
                    params,
                )
                .await
            {
                Ok(_) => Ok(()),
                Err(e) if went_away(&e) => Ok(()),
                // Answered in Chrome's window, or the page moved on:
                // nothing waits for it any more.
                Err(Error::Protocol { .. }) => Ok(()),
                Err(e) => Err(e),
            }
        }
    }
}

impl DialogInner {
    fn release(&self) {
        if self.answered.swap(true, Ordering::Relaxed) {
            return;
        }
        // Leaving is what netweir's own goto() asked for; anything else
        // is Escape.
        let accept = self.kind == "beforeunload";
        let (conn, session, at) = (self.conn.clone(), self.session.clone(), self.not_before);
        self.runtime.spawn(async move {
            tokio::time::sleep_until(at).await;
            conn.send(
                &session,
                "Page.handleJavaScriptDialog",
                json!({"accept": accept}),
            );
        });
    }
}

impl Drop for DialogInner {
    fn drop(&mut self) {
        self.release();
    }
}

/// A file the page downloaded, or is downloading, into its context's own
/// folder, where Chrome names it after its GUID. It's removed when the
/// context closes.
#[derive(Clone)]
pub struct Download {
    inner: Arc<DownloadInner>,
}

struct DownloadInner {
    guid: String,
    url: String,
    suggested_filename: String,
    /// Where Chrome writes it; None for a page whose context has no
    /// folder of netweir's.
    file: Option<PathBuf>,
    context: Option<String>,
    conn: Connection,
    timeout: Duration,
    /// None while it's under way; then whether it finished, or why not.
    done: tokio::sync::watch::Sender<Option<std::result::Result<(), String>>>,
}

impl Download {
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// The name the server or the page gave it.
    pub fn suggested_filename(&self) -> &str {
        &self.inner.suggested_filename
    }

    /// Waits for it to end: Ok once the whole file is there, or why not
    /// ("canceled", which is also what Chrome says of one whose
    /// connection broke).
    async fn finished(&self) -> std::result::Result<(), String> {
        let mut done = self.inner.done.subscribe();
        match done.wait_for(Option::is_some).await {
            Ok(end) => end.clone().expect("waited for"),
            Err(_) => Err("the browser closed".into()),
        }
    }

    /// Waits for it to end; None if it finished, or why it didn't.
    pub async fn failure(&self) -> Option<String> {
        self.finished().await.err()
    }

    /// Waits for it to finish, and returns the file Chrome wrote.
    pub async fn path(&self) -> Result<PathBuf> {
        let inner = &self.inner;
        if let Err(why) = self.finished().await {
            return Err(Error::Gone(format!(
                "the download of {} didn't finish: {why}",
                inner.url
            )));
        }
        inner.file.clone().ok_or_else(|| {
            Error::Gone(format!(
                "the download of {} went where Chrome keeps them, not to a folder of netweir's",
                inner.url
            ))
        })
    }

    /// Stops it, if it hasn't finished.
    pub async fn cancel(&self) -> Result<()> {
        let inner = &self.inner;
        let mut params = json!({"guid": inner.guid});
        if let Some(id) = &inner.context {
            params["browserContextId"] = Value::from(id.as_str());
        }
        match inner
            .conn
            .call_within(inner.timeout, "", "Browser.cancelDownload", params)
            .await
        {
            // Already over.
            Ok(_) | Err(Error::Protocol { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Every download under way, and the folders they go to: one for each
/// context, under one the browser makes the first time it's needed,
/// which a launch's sweep takes if the program dies.
#[derive(Default)]
pub(crate) struct Downloads {
    root: Mutex<Option<crate::launch::TempProfile>>,
    under_way: Mutex<HashMap<String, Weak<DownloadInner>>>,
}

impl Downloads {
    /// The folder for `context`'s downloads (None for the default
    /// context), made if need be.
    pub(crate) fn folder(&self, context: Option<&str>) -> std::io::Result<PathBuf> {
        let mut root = self.root.lock().unwrap_or_else(|e| e.into_inner());
        if root.is_none() {
            *root = Some(crate::launch::TempProfile::make()?);
        }
        let folder = root
            .as_ref()
            .expect("made above")
            .dir()
            .join(context.unwrap_or("default"));
        std::fs::create_dir_all(&folder)?;
        Ok(folder)
    }

    /// The folder for `context`'s downloads, if it has one.
    pub(crate) fn folder_of(&self, context: Option<&str>) -> Option<PathBuf> {
        let root = self.root.lock().unwrap_or_else(|e| e.into_inner());
        let folder = root.as_ref()?.dir().join(context.unwrap_or("default"));
        folder.is_dir().then_some(folder)
    }

    /// Removes `context`'s folder, and what's in it.
    pub(crate) fn remove(&self, context: Option<&str>) {
        if let Some(folder) = self.folder_of(context) {
            let _ = std::fs::remove_dir_all(folder);
        }
    }

    /// Removes every folder; for the browser closing.
    pub(crate) fn remove_all(&self) {
        self.root.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// For `Page.downloadWillBegin`: the download, or None if it's been
    /// heard of already (from another of the page's sessions).
    pub(crate) fn begin(
        &self,
        conn: &Connection,
        timeout: Duration,
        context: Option<&str>,
        params: &Value,
    ) -> Option<Download> {
        let guid = str_of(params, "guid");
        let mut under_way = self.under_way.lock().unwrap_or_else(|e| e.into_inner());
        under_way.retain(|_, d| d.strong_count() > 0);
        if under_way.contains_key(&guid) {
            return None;
        }
        let download = Download {
            inner: Arc::new(DownloadInner {
                file: self.folder_of(context).map(|f| f.join(&guid)),
                guid: guid.clone(),
                url: str_of(params, "url"),
                suggested_filename: str_of(params, "suggestedFilename"),
                context: context.map(str::to_string),
                conn: conn.clone(),
                timeout,
                done: tokio::sync::watch::Sender::new(None),
            }),
        };
        under_way.insert(guid, Arc::downgrade(&download.inner));
        Some(download)
    }

    /// For `Browser.downloadProgress`.
    pub(crate) fn progress(&self, params: &Value) {
        let end = match params["state"].as_str() {
            Some("completed") => Ok(()),
            Some("canceled") => Err("canceled".to_string()),
            _ => return,
        };
        let guid = str_of(params, "guid");
        let download = self
            .under_way
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&guid)
            .and_then(|d| d.upgrade());
        if let Some(download) = download {
            download.done.send_replace(Some(end));
        }
    }

    /// The browser has gone: nothing under way will finish.
    pub(crate) fn gone(&self) {
        let under_way =
            std::mem::take(&mut *self.under_way.lock().unwrap_or_else(|e| e.into_inner()));
        for download in under_way.into_values().filter_map(|d| d.upgrade()) {
            download
                .done
                .send_replace(Some(Err("the browser closed".into())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_is_as_long_as_a_person_takes() {
        for _ in 0..100 {
            let p = pause();
            assert!(p >= Duration::from_millis(500) && p < Duration::from_millis(1500));
        }
        let all: std::collections::HashSet<_> = (0..100).map(|_| pause()).collect();
        assert!(all.len() > 10, "picked at random");
    }

    #[test]
    fn each_context_has_a_folder_of_its_own_that_goes_with_it() {
        let downloads = Downloads::default();
        assert_eq!(downloads.folder_of(Some("a")), None);
        let a = downloads.folder(Some("a")).unwrap();
        let b = downloads.folder(None).unwrap();
        assert_ne!(a, b);
        assert_eq!(downloads.folder_of(Some("a")), Some(a.clone()));
        let root = a.parent().unwrap().to_path_buf();
        assert!(b.starts_with(&root));
        downloads.remove(Some("a"));
        assert!(!a.exists() && b.exists());
        downloads.remove_all();
        assert!(!root.exists());
    }
}
