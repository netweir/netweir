//! Keeping what a crawl fetched: each request and its response, written to
//! a WARC file, a HAR file, or both, as the crawl goes.
//!
//! A fetch hands its exchange to a writer thread through a channel bounded
//! by count and by bytes, and awaits room if the disk falls behind, so
//! records are never dropped and memory never fills. The thread streams
//! each record to its file from the exchange's own buffers, so a record
//! costs no copy of its body, and flushes it before the next; after a crash
//! a file holds whole records and at most one torn one at the end, which a
//! resumed crawl cuts off before it adds to the file. See
//! docs/design/archive.md.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use sha1::{Digest, Sha1};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

/// Exchanges waiting for the writer, beyond which a fetch waits its turn.
const QUEUE: usize = 64;
/// Bytes of bodies waiting for the writer, beyond which a fetch waits its
/// turn. A body larger than this goes on its own.
const ROOM: usize = 64 << 20;

/// One request netweir sent and the response it got.
#[derive(Debug)]
pub struct Exchange {
    /// When the request was sent.
    pub started: SystemTime,
    pub url: String,
    pub method: String,
    /// The header lines as the client wrote them, in order: names with the
    /// capitalisation sent, values as bytes.
    pub request_headers: Vec<(String, Vec<u8>)>,
    pub request_body: Option<Vec<u8>>,
    /// "HTTP/1.0", "HTTP/1.1", "HTTP/2" and so on.
    pub version: &'static str,
    pub status: u16,
    /// The server's reason phrase.
    pub reason: String,
    /// As the client keeps them: lowercase names, each name's values
    /// together, values byte for byte.
    pub response_headers: Vec<(String, Vec<u8>)>,
    /// Decompressed.
    pub body: Bytes,
    /// Only the start of the body was read (a redirect's).
    pub truncated: bool,
    /// From sending the request to its response's headers.
    pub wait: Duration,
    /// Reading the body.
    pub receive: Duration,
}

/// Where a crawl's exchanges are kept. Cheap to clone: clones write to the
/// same files. Nothing is written until `start`; exchanges recorded
/// before then wait for it.
#[derive(Clone)]
pub struct Archive {
    inner: Arc<Inner>,
}

struct Inner {
    /// Unbounded, so closing never waits for a place: what bounds it are
    /// `slots` and `room`, which a fetch takes before it sends, and which
    /// closing shuts so that fetches waiting for them give up at once.
    queue: mpsc::UnboundedSender<Message>,
    slots: Arc<Semaphore>,
    room: Arc<Semaphore>,
    /// What `start` opens, until it does.
    plan: Mutex<Option<Plan>>,
    writer: Mutex<Option<JoinHandle<Result<(), String>>>>,
    status: Arc<Status>,
    /// How many files each exchange goes to.
    files: u64,
}

struct Plan {
    warc: Option<PathBuf>,
    har: Option<PathBuf>,
    append: bool,
    inbox: mpsc::UnboundedReceiver<Message>,
}

/// What the writer tells the crawl.
#[derive(Default)]
struct Status {
    /// Exchanges not written to a file they should have gone to.
    errors: AtomicU64,
    warnings: Mutex<Vec<String>>,
}

enum Message {
    /// With its place in the queue and in the byte budget, given back
    /// once written.
    Record(Box<Exchange>, [OwnedSemaphorePermit; 2]),
    Close,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Archive")
    }
}

impl Archive {
    /// An archive to `warc` (gzipped if its name ends in `.gz`) and `har`,
    /// either of which may be None. Nothing is opened yet, only checked:
    /// each file's directory must exist, and with `append` a file that's
    /// there must be one netweir wrote. The two can't be one file.
    pub fn new(warc: Option<&Path>, har: Option<&Path>, append: bool) -> Result<Archive, String> {
        if let (Some(w), Some(h)) = (warc, har)
            && same_file(w, h)
        {
            return Err(format!(
                "the WARC and HAR files are both {}: each needs a file of its own",
                w.display()
            ));
        }
        for (path, start) in [(warc, warc.map(warc_start)), (har, Some(HAR_START))] {
            let Some(path) = path else { continue };
            let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
            if dir.is_some_and(|d| !d.is_dir()) {
                return Err(format!(
                    "can't write {}: the directory it goes in doesn't exist",
                    path.display()
                ));
            }
            if append && let Some(start) = start {
                check_start(path, start)?;
            }
        }
        let (queue, inbox) = mpsc::unbounded_channel();
        Ok(Archive {
            inner: Arc::new(Inner {
                queue,
                slots: Arc::new(Semaphore::new(QUEUE)),
                room: Arc::new(Semaphore::new(ROOM)),
                plan: Mutex::new(Some(Plan {
                    warc: warc.map(Path::to_path_buf),
                    har: har.map(Path::to_path_buf),
                    append,
                    inbox,
                })),
                writer: Mutex::new(None),
                status: Arc::default(),
                files: u64::from(warc.is_some()) + u64::from(har.is_some()),
            }),
        })
    }

    /// Opens the files (cutting off a torn last record when appending,
    /// starting them afresh otherwise) and starts writing, what was
    /// recorded so far first. Once only; later calls do nothing.
    pub fn start(&self) -> Result<(), String> {
        let Some(plan) = self
            .inner
            .plan
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return Ok(());
        };
        // Both files are judged before either is changed: a HAR file that
        // can't be added to leaves the WARC file as it was.
        let warc = plan
            .warc
            .as_deref()
            .map(|p| Warc::prepare(p, plan.append))
            .transpose()?;
        let har = plan
            .har
            .as_deref()
            .map(|p| Har::prepare(p, plan.append))
            .transpose()?;
        let files = Files {
            warc: warc.map(Warc::finish).transpose()?,
            har: har.map(Har::finish).transpose()?,
            status: self.inner.status.clone(),
            per_exchange: self.inner.files,
        };
        let inbox = plan.inbox;
        let writer = std::thread::Builder::new()
            .name("netweir-archive".into())
            .spawn(move || files.write_loop(inbox))
            .map_err(|e| format!("can't start the archive writer: {e}"))?;
        *self.inner.writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(writer);
        Ok(())
    }

    /// Hands `exchange` to the writer, waiting while it's behind. Once the
    /// archive is closed, counts it as not written.
    pub async fn record(&self, exchange: Exchange) {
        let size = exchange.body.len() + exchange.request_body.as_ref().map_or(0, Vec::len);
        let want = size.clamp(1, ROOM) as u32;
        let slot = self.inner.slots.clone().acquire_owned().await;
        let room = self.inner.room.clone().acquire_many_owned(want).await;
        let sent = match (slot, room) {
            (Ok(slot), Ok(room)) => self
                .inner
                .queue
                .send(Message::Record(Box::new(exchange), [slot, room]))
                .is_ok(),
            _ => false,
        };
        if !sent {
            self.inner
                .status
                .errors
                .fetch_add(self.inner.files, Ordering::Relaxed);
        }
    }

    /// Exchanges that didn't reach a file they should have, once for each
    /// file.
    pub fn errors(&self) -> u64 {
        self.inner.status.errors.load(Ordering::Relaxed)
    }

    /// What went wrong since last asked, once each.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .inner
                .status
                .warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Writes what's queued, ends the HAR file and syncs both to disk. An
    /// error if that last part failed. Exchanges recorded after this, or
    /// still on their way when it's called, aren't written and count as
    /// errors. An archive never started is left as it was.
    pub fn close(&self) -> Result<(), String> {
        if let Some(plan) = self
            .inner
            .plan
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            self.inner.slots.close();
            self.inner.room.close();
            let mut inbox = plan.inbox;
            inbox.close();
            while let Ok(message) = inbox.try_recv() {
                if let Message::Record(..) = message {
                    self.inner
                        .status
                        .errors
                        .fetch_add(self.inner.files, Ordering::Relaxed);
                }
            }
            return Ok(());
        }
        let Some(writer) = self
            .inner
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return Ok(());
        };
        // Fetches waiting for a place give up now, and count as not
        // written; what's queued ahead of Close is written. Nothing here
        // waits on a fetch, which may be a task on the very thread
        // that's closing.
        self.inner.slots.close();
        self.inner.room.close();
        let _ = self.inner.queue.send(Message::Close);
        writer
            .join()
            .unwrap_or_else(|_| Err("the archive writer panicked".into()))
    }
}

/// Whether `a` and `b` name one file: the same file on disk if both are
/// there, and otherwise the same name in the same directory, letter case
/// aside, since on a case-insensitive disk (macOS's and Windows' by
/// default) `crawl.warc` and `Crawl.WARC` are one.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
        use std::os::unix::fs::MetadataExt;
        return x.dev() == y.dev() && x.ino() == y.ino();
    }
    // Where it is: the file itself with every link followed, or its
    // directory's, for one not made yet.
    let place = |p: &Path| -> (PathBuf, String) {
        let full = p.canonicalize().unwrap_or_else(|_| {
            let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
            let dir = dir.unwrap_or(Path::new("."));
            dir.canonicalize()
                .unwrap_or_else(|_| dir.to_path_buf())
                .join(p.file_name().unwrap_or_default())
        });
        let name = full.file_name().unwrap_or_default().to_string_lossy();
        (
            full.parent().map(Path::to_path_buf).unwrap_or_default(),
            name.to_lowercase(),
        )
    };
    place(a) == place(b)
}

/// What a file this writer made starts with.
fn warc_start(path: &Path) -> &'static [u8] {
    if is_gzip(path) {
        &[0x1f, 0x8b]
    } else {
        b"WARC/"
    }
}

fn is_gzip(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "gz")
}

/// Refuses a file that's there and doesn't start with `start`.
fn check_start(path: &Path, start: &[u8]) -> Result<(), String> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let mut head = Vec::new();
    (&mut file)
        .take(start.len() as u64)
        .read_to_end(&mut head)
        .map_err(|e| format!("can't read {}: {e}", path.display()))?;
    if head.is_empty() || start.starts_with(&head) {
        Ok(())
    } else {
        Err(not_ours(
            path,
            &format!(
                "doesn't start as {} does",
                String::from_utf8_lossy(start).escape_debug()
            ),
        ))
    }
}

struct Files {
    warc: Option<Warc>,
    har: Option<Har>,
    status: Arc<Status>,
    per_exchange: u64,
}

impl Files {
    fn write_loop(mut self, mut inbox: mpsc::UnboundedReceiver<Message>) -> Result<(), String> {
        while let Some(message) = inbox.blocking_recv() {
            let Message::Record(exchange, _room) = message else {
                break;
            };
            if let Some(warc) = &mut self.warc {
                warc.write(&exchange, &self.status);
            }
            if let Some(har) = &mut self.har {
                har.write(&exchange, &self.status);
            }
        }
        // Closed, or every sender gone: the crawl is over. Exchanges still
        // on their way (fetches under way when it stopped) aren't written.
        inbox.close();
        while let Ok(message) = inbox.try_recv() {
            if let Message::Record(..) = message {
                self.status
                    .errors
                    .fetch_add(self.per_exchange, Ordering::Relaxed);
            }
        }
        let mut result = Ok(());
        if let Some(har) = &mut self.har
            && !har.sink.failed
        {
            result = har
                .sink
                .file
                .write_all(FOOT)
                .map_err(|e| har.sink.error(&e));
        }
        for sink in [
            self.warc.as_mut().map(|w| &mut w.sink),
            self.har.as_mut().map(|h| &mut h.sink),
        ]
        .into_iter()
        .flatten()
        {
            if let Err(e) = sink.file.sync_all() {
                result = result.and(Err(sink.error(&e)));
            }
        }
        result
    }
}

/// A file records go to. Each record goes through a buffer of its own and
/// is flushed to the file before the next.
struct Sink {
    path: PathBuf,
    file: File,
    /// A write failed part way, so the file may end in a torn record:
    /// anything written after it would be unreadable.
    failed: bool,
}

impl Sink {
    /// Writes one exchange's records with `write`, unless an earlier write
    /// failed; either way, a failure counts the exchange as not written.
    fn put(&mut self, status: &Status, write: impl FnOnce(&mut dyn Write) -> io::Result<()>) {
        if !self.failed {
            let mut out = BufWriter::with_capacity(1 << 16, &mut self.file);
            let written = write(&mut out).and_then(|()| out.flush());
            drop(out);
            if let Err(e) = written {
                self.failed = true;
                let message = format!("{}; it gets nothing more this run", self.error(&e));
                status
                    .warnings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message);
            }
        }
        if self.failed {
            status.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn error(&self, e: &io::Error) -> String {
        format!("couldn't write {}: {e}", self.path.display())
    }
}

/// A file read and judged, not yet changed: how much of it to keep.
struct Prepared {
    path: PathBuf,
    /// The file as it is, when adding to one that's there.
    file: Option<File>,
    kept: u64,
}

/// Works out how much of `path` to keep: none to start afresh or, with
/// `append`, the first `keep(file, length)` bytes of what's there.
/// Changes nothing, so that both files can be judged before either is
/// touched.
fn prepare(
    path: &Path,
    append: bool,
    keep: impl FnOnce(&mut File, u64) -> Result<u64, String>,
) -> Result<Prepared, String> {
    let nothing = |file| Prepared {
        path: path.to_path_buf(),
        file,
        kept: 0,
    };
    if !append {
        return Ok(nothing(None));
    }
    let cant = |e: io::Error| format!("can't write {}: {e}", path.display());
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(nothing(None)),
        Err(e) => return Err(cant(e)),
    };
    let len = file.metadata().map_err(cant)?.len();
    let kept = if len > 0 { keep(&mut file, len)? } else { 0 };
    Ok(Prepared {
        kept,
        ..nothing(Some(file))
    })
}

impl Prepared {
    /// Cuts the file back to what's kept (creating it, or emptying it, to
    /// start afresh), ready to add to.
    fn sink(self) -> Result<Sink, String> {
        let cant = |e: io::Error| format!("can't write {}: {e}", self.path.display());
        let mut file = match self.file {
            Some(file) => file,
            None => std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&self.path)
                .map_err(cant)?,
        };
        file.set_len(self.kept).map_err(cant)?;
        file.seek(SeekFrom::Start(self.kept)).map_err(cant)?;
        Ok(Sink {
            path: self.path,
            file,
            failed: false,
        })
    }
}

fn not_ours(path: &Path, what: &str) -> String {
    format!(
        "can't add to {}: it {what}, so it isn't a file netweir wrote",
        path.display()
    )
}

struct Warc {
    sink: Sink,
    gzip: bool,
}

/// One WARC record: its type, id and target, extra header lines, content
/// type, and the block in parts, the last of which is the payload when
/// there is one.
struct Record<'a> {
    kind: &'a str,
    id: &'a str,
    date: &'a str,
    uri: Option<&'a str>,
    extra: Vec<(&'a str, String)>,
    content_type: &'a str,
    block: [&'a [u8]; 2],
    payload: bool,
}

impl Warc {
    #[cfg(test)]
    fn open(path: &Path, append: bool) -> Result<Warc, String> {
        Warc::finish(Warc::prepare(path, append)?)
    }

    fn prepare(path: &Path, append: bool) -> Result<Prepared, String> {
        let gzip = is_gzip(path);
        prepare(path, append, |file, len| {
            let read = |e: io::Error| format!("can't read {}: {e}", path.display());
            let start = warc_start(path);
            let mut head = Vec::new();
            (&mut *file)
                .take(start.len() as u64)
                .read_to_end(&mut head)
                .map_err(read)?;
            file.seek(SeekFrom::Start(0)).map_err(read)?;
            if !start.starts_with(&head) {
                return Err(not_ours(path, "doesn't start as a WARC file does"));
            }
            if gzip {
                whole_members(file, len).map_err(read)?.ok_or_else(|| {
                    not_ours(path, "has a gzip member that isn't torn but won't read")
                })
            } else {
                whole_records(file, len)
                    .map_err(read)?
                    .ok_or_else(|| not_ours(path, "has a malformed record before its end"))
            }
        })
    }

    /// Cuts the file back and writes the run's warcinfo record.
    fn finish(prepared: Prepared) -> Result<Warc, String> {
        let path = prepared.path.clone();
        let gzip = is_gzip(&path);
        let mut warc = Warc {
            sink: prepared.sink()?,
            gzip,
        };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let fields = format!(
            "software: netweir/{}\r\nformat: WARC File Format 1.1\r\n\
             conformsTo: http://iipc.github.io/warc-specifications/specifications/warc-format/warc-1.1/\r\n",
            env!("CARGO_PKG_VERSION")
        );
        let info = Record {
            kind: "warcinfo",
            id: &record_id(),
            date: &iso8601(SystemTime::now(), 6),
            uri: None,
            extra: vec![("WARC-Filename", name.unwrap_or_default())],
            content_type: "application/warc-fields",
            block: [fields.as_bytes(), b""],
            payload: false,
        };
        let mut out = BufWriter::new(&mut warc.sink.file);
        write_record(&mut out, gzip, &info)
            .and_then(|()| out.flush())
            .map_err(|e| format!("can't write {}: {e}", path.display()))?;
        drop(out);
        Ok(warc)
    }

    /// Writes the request and response records of `e`.
    fn write(&mut self, e: &Exchange, status: &Status) {
        let date = iso8601(e.started, 6);
        let protocol = protocol(e.version);
        let (request_id, response_id) = (record_id(), record_id());

        let mut request = format!("{} {} HTTP/1.1\r\n", e.method, origin_form(&e.url)).into_bytes();
        for (name, value) in &e.request_headers {
            request.extend_from_slice(name.as_bytes());
            // An empty value is written as the client writes it.
            request.extend_from_slice(if value.is_empty() { b":" } else { b": " });
            request.extend_from_slice(value);
            request.extend_from_slice(b"\r\n");
        }
        request.extend_from_slice(b"\r\n");

        // HTTP/2 has no status line; its exchange is written in HTTP/1.1
        // form.
        let line_version = if e.version == "HTTP/1.0" {
            "HTTP/1.0"
        } else {
            "HTTP/1.1"
        };
        let mut response = format!("{line_version} {} {}\r\n", e.status, e.reason).into_bytes();
        for (name, value) in stored_headers(e) {
            response.extend_from_slice(name.as_bytes());
            response.extend_from_slice(b": ");
            response.extend_from_slice(&value);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"\r\n");

        // The request went out as HTTP/1.1 even to a server that answered
        // in 1.0.
        let request_protocol = if protocol == "h2" { "h2" } else { "http/1.1" };
        let request = Record {
            kind: "request",
            id: &request_id,
            date: &date,
            uri: Some(&e.url),
            extra: vec![
                ("WARC-Concurrent-To", response_id.clone()),
                ("WARC-Protocol", request_protocol.into()),
            ],
            content_type: "application/http; msgtype=request",
            block: [&request, e.request_body.as_deref().unwrap_or_default()],
            payload: true,
        };
        let mut extra = vec![("WARC-Protocol", protocol.to_string())];
        if e.truncated {
            extra.push(("WARC-Truncated", "length".into()));
        }
        let response = Record {
            kind: "response",
            id: &response_id,
            date: &date,
            uri: Some(&e.url),
            extra,
            content_type: "application/http; msgtype=response",
            block: [&response, &e.body],
            payload: true,
        };
        let gzip = self.gzip;
        self.sink.put(status, |out| {
            write_record(out, gzip, &request)?;
            write_record(out, gzip, &response)
        });
    }
}

/// How `WARC-Protocol` names a response's HTTP version.
fn protocol(version: &str) -> &'static str {
    match version {
        "HTTP/2" => "h2",
        "HTTP/3" => "h3",
        "HTTP/1.0" => "http/1.0",
        "HTTP/0.9" => "http/0.9",
        _ => "http/1.1",
    }
}

/// Writes `r` to `out`, gzipped as a member of its own for a .warc.gz.
/// The digests are worked out first, from the parts as they are, so the
/// body is never copied.
fn write_record(out: &mut dyn Write, gzip: bool, r: &Record) -> io::Result<()> {
    let mut block = Sha1::new();
    for part in r.block {
        block.update(part);
    }
    let length: usize = r.block.iter().map(|p| p.len()).sum();
    let mut head = format!(
        "WARC/1.1\r\nWARC-Type: {}\r\nWARC-Record-ID: {}\r\n",
        r.kind, r.id
    );
    if let Some(uri) = r.uri {
        head.push_str(&format!("WARC-Target-URI: {uri}\r\n"));
    }
    head.push_str(&format!("WARC-Date: {}\r\n", r.date));
    for (name, value) in &r.extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if r.payload {
        head.push_str(&format!("WARC-Payload-Digest: {}\r\n", digest(r.block[1])));
    }
    head.push_str(&format!(
        "WARC-Block-Digest: sha1:{}\r\nContent-Type: {}\r\nContent-Length: {length}\r\n\r\n",
        data_encoding::BASE32.encode(&block.finalize()),
        r.content_type,
    ));
    let put = |out: &mut dyn Write| -> io::Result<()> {
        out.write_all(head.as_bytes())?;
        for part in r.block {
            out.write_all(part)?;
        }
        out.write_all(b"\r\n\r\n")
    };
    if gzip {
        let mut gz = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        put(&mut gz)?;
        gz.finish()?;
        Ok(())
    } else {
        put(out)
    }
}

/// The response's headers as stored. The body is kept dechunked, so
/// Transfer-Encoding goes. Content-Length is the stored body's when it
/// can be: added if the server sent none (chunked, or decompressed by the
/// client, which takes the header away), rewritten if the body changed
/// length. Left as sent for a status that has no body, a HEAD, and a body
/// only partly read (whose record says it's truncated).
fn stored_headers(e: &Exchange) -> Vec<(&str, Cow<'_, [u8]>)> {
    let as_sent = matches!(e.status, 100..=199 | 204 | 304) || e.method == "HEAD" || e.truncated;
    let length = e.body.len().to_string().into_bytes();
    let mut out = Vec::with_capacity(e.response_headers.len() + 1);
    let mut has_length = false;
    for (name, value) in &e.response_headers {
        match name.as_str() {
            "transfer-encoding" => continue,
            "content-length" if !as_sent => {
                has_length = true;
                out.push((name.as_str(), Cow::Owned(length.clone())));
            }
            _ => out.push((name.as_str(), Cow::Borrowed(value.as_slice()))),
        }
    }
    if !as_sent && !has_length {
        out.push(("content-length", Cow::Owned(length)));
    }
    out
}

/// The path and query, as an HTTP/1.1 request line has them.
fn origin_form(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => match u.query() {
            Some(q) => format!("{}?{q}", u.path()),
            None => u.path().to_string(),
        },
        Err(_) => url.to_string(),
    }
}

/// `sha1:` and the digest in base 32, as warcio writes it.
fn digest(bytes: &[u8]) -> String {
    format!(
        "sha1:{}",
        data_encoding::BASE32.encode(&Sha1::digest(bytes))
    )
}

/// A random version 4 UUID as a WARC record id. The randomness is the
/// process's hash seed, which is fresh for every run.
fn record_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static SEED: OnceLock<std::collections::hash_map::RandomState> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seed = SEED.get_or_init(Default::default);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0u8; 16];
    for (half, salt) in bytes.chunks_mut(8).zip([1u64, 2]) {
        let mut h = seed.build_hasher();
        h.write_u64(n);
        h.write_u64(salt);
        half.copy_from_slice(&h.finish().to_be_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "<urn:uuid:{}-{}-{}-{}-{}>",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// `t` as an ISO 8601 UTC time with `digits` decimal places of seconds:
/// `2026-10-12T09:30:00.123456Z`.
fn iso8601(t: SystemTime, digits: u32) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's days-to-civil, for days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let fraction = since.subsec_nanos() / 10u32.pow(9 - digits);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{fraction:0width$}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60,
        width = digits as usize
    )
}

/// Wraps a reader, counting the bytes taken from it.
struct Counted<R> {
    inner: R,
    taken: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.taken += n as u64;
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Counted<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, n: usize) {
        self.taken += n as u64;
        self.inner.consume(n);
    }
}

/// Where the last whole gzip member of `file` ends, decompressing each in
/// turn. None if one fails for a reason other than being cut short.
fn whole_members(file: &mut File, len: u64) -> io::Result<Option<u64>> {
    let mut reader = Counted {
        inner: BufReader::with_capacity(1 << 16, &mut *file),
        taken: 0,
    };
    loop {
        let start = reader.taken;
        if reader.fill_buf()?.is_empty() {
            return Ok(Some(start));
        }
        let mut member = flate2::bufread::GzDecoder::new(&mut reader);
        match io::copy(&mut member, &mut io::sink()) {
            Ok(_) => {}
            // A member the writer was still writing ends at the end of the
            // file.
            Err(_) if reader.taken >= len || reader.fill_buf()?.is_empty() => {
                return Ok(Some(start));
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Some(start)),
            Err(_) => return Ok(None),
        }
    }
}

/// Where the last whole record of a plain WARC `file` ends, walking the
/// records by their Content-Length. None if one is malformed before the
/// end of the file, which a torn write can't do.
fn whole_records(file: &mut File, len: u64) -> io::Result<Option<u64>> {
    const MOST_HEAD: u64 = 1 << 16;
    let mut at = 0;
    while at < len {
        file.seek(SeekFrom::Start(at))?;
        let mut head = Vec::new();
        (&mut *file).take(MOST_HEAD).read_to_end(&mut head)?;
        let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") else {
            // No end to the head: torn if the file ends within it.
            return Ok(
                (at + head.len() as u64 >= len && head.len() < MOST_HEAD as usize).then_some(at),
            );
        };
        let length = String::from_utf8_lossy(&head[..end])
            .lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<u64>().ok());
        let Some(length) = length.filter(|_| head.starts_with(b"WARC/")) else {
            return Ok(None);
        };
        let next = at + end as u64 + 4 + length + 4;
        if next > len {
            return Ok(Some(at));
        }
        file.seek(SeekFrom::Start(next - 4))?;
        let mut tail = [0u8; 4];
        file.read_exact(&mut tail)?;
        if &tail != b"\r\n\r\n" {
            return Ok(None);
        }
        at = next;
    }
    Ok(Some(at))
}

/// What a HAR file this writer made starts with.
const HAR_START: &[u8] = br#"{"log":{"version":"1.2","creator":{"name":"netweir","#;
const FOOT: &[u8] = b"\n]}}\n";
/// What it ends with.
const HAR_HEAD_END: &[u8] = br#"},"entries":["#;

struct Har {
    sink: Sink,
    /// The file has an entry: the next needs a comma before it.
    any: bool,
}

impl Har {
    /// The file judged, and whether it has an entry.
    fn prepare(path: &Path, append: bool) -> Result<(Prepared, bool), String> {
        let mut any = false;
        let prepared = prepare(path, append, |file, len| {
            let read = |e: io::Error| format!("can't read {}: {e}", path.display());
            let mut start = vec![0u8; HAR_START.len().min(len as usize)];
            file.read_exact(&mut start).map_err(read)?;
            if start != HAR_START[..start.len()] {
                return Err(not_ours(path, "doesn't start as netweir's HAR files do"));
            }
            let (kept, has) = whole_entries(file, len).map_err(read)?;
            any = has;
            Ok(kept)
        })?;
        Ok((prepared, any))
    }

    /// Cuts the file back, starting it with the head if it's empty.
    fn finish((prepared, any): (Prepared, bool)) -> Result<Har, String> {
        let kept = prepared.kept;
        let mut sink = prepared.sink()?;
        if kept == 0 {
            let head = format!(
                r#"{{"log":{{"version":"1.2","creator":{{"name":"netweir","version":"{}"}},"entries":["#,
                env!("CARGO_PKG_VERSION")
            );
            sink.file
                .write_all(head.as_bytes())
                .map_err(|e| sink.error(&e))?;
        }
        Ok(Har { sink, any })
    }

    fn write(&mut self, e: &Exchange, status: &Status) {
        let first = !self.any;
        self.sink.put(status, |out| {
            out.write_all(if first { b"\n" } else { b",\n" })?;
            write_entry(out, e)
        });
        self.any |= !self.sink.failed;
    }
}

/// Where the file's last whole entry ends, and whether it has one: a
/// closing `]}}` is taken off, and so is a last line that isn't a whole
/// entry, with the comma before it.
fn whole_entries(file: &mut File, len: u64) -> io::Result<(u64, bool)> {
    let mut end = len;
    if len >= FOOT.len() as u64 {
        let mut tail = vec![0u8; FOOT.len()];
        file.seek(SeekFrom::Start(len - FOOT.len() as u64))?;
        file.read_exact(&mut tail)?;
        if tail == FOOT {
            end -= FOOT.len() as u64;
        }
    }
    // The head is the first line; each entry is a line after it. A head
    // cut off part way is written again.
    let Some(newline) = last_newline(file, end)? else {
        let whole = end >= HAR_HEAD_END.len() as u64 && {
            let mut tail = vec![0u8; HAR_HEAD_END.len()];
            file.seek(SeekFrom::Start(end - HAR_HEAD_END.len() as u64))?;
            file.read_exact(&mut tail)?;
            tail == HAR_HEAD_END
        };
        return Ok((if whole { end } else { 0 }, false));
    };
    file.seek(SeekFrom::Start(newline + 1))?;
    let mut line = Vec::new();
    (&mut *file)
        .take(end - newline - 1)
        .read_to_end(&mut line)?;
    if serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&line).is_ok() {
        return Ok((end, true));
    }
    let mut end = newline;
    if end > 0 {
        let mut comma = [0u8];
        file.seek(SeekFrom::Start(end - 1))?;
        file.read_exact(&mut comma)?;
        if comma == *b"," {
            end -= 1;
        }
    }
    Ok((end, last_newline(file, end)?.is_some()))
}

/// Where the last `\n` before `end` is, reading backwards.
fn last_newline(file: &mut File, end: u64) -> io::Result<Option<u64>> {
    let mut chunk = vec![0u8; 1 << 16];
    let mut to = end;
    while to > 0 {
        let from = to.saturating_sub(chunk.len() as u64);
        let part = &mut chunk[..(to - from) as usize];
        file.seek(SeekFrom::Start(from))?;
        file.read_exact(part)?;
        if let Some(i) = part.iter().rposition(|b| *b == b'\n') {
            return Ok(Some(from + i as u64));
        }
        to = from;
    }
    Ok(None)
}

/// Writes `e` as a HAR entry on one line. Everything but the response body
/// is small and goes through serde_json; the body is written straight from
/// its buffer, escaped or in base64, so it's never copied whole.
fn write_entry(out: &mut dyn Write, e: &Exchange) -> io::Result<()> {
    use serde_json::json;
    let json = |out: &mut dyn Write, value: &serde_json::Value| -> io::Result<()> {
        serde_json::to_writer(&mut *out, value).map_err(io::Error::other)
    };
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let pairs = |headers: &[(String, Vec<u8>)]| -> serde_json::Value {
        headers
            .iter()
            .map(|(name, value)| json!({"name": name, "value": String::from_utf8_lossy(value)}))
            .collect()
    };
    let query: Vec<serde_json::Value> = url::Url::parse(&e.url)
        .map(|u| {
            u.query_pairs()
                .map(|(k, v)| json!({"name": k, "value": v}))
                .collect()
        })
        .unwrap_or_default();
    let header = |headers: &[(String, Vec<u8>)], name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
    };
    let mut request = json!({
        "method": e.method,
        "url": e.url,
        // HTTP/1.1 even to a server that answered in 1.0, as the WARC
        // request record has it.
        "httpVersion": if e.version == "HTTP/2" { "HTTP/2" } else { "HTTP/1.1" },
        "cookies": [],
        "headers": pairs(&e.request_headers),
        "queryString": query,
        "headersSize": -1,
        "bodySize": e.request_body.as_ref().map_or(0, Vec::len),
    });
    if let Some(body) = &e.request_body {
        let mime = header(&e.request_headers, "content-type").unwrap_or_default();
        let mut post = json!({"mimeType": mime, "text": String::from_utf8_lossy(body)});
        if mime.split(';').next().is_some_and(|m| {
            m.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        }) {
            post["params"] = url::form_urlencoded::parse(body)
                .map(|(k, v)| json!({"name": k, "value": v}))
                .collect();
        }
        request["postData"] = post;
    }
    let (wait, receive) = (ms(e.wait), ms(e.receive));

    out.write_all(b"{\"startedDateTime\":")?;
    json(out, &json!(iso8601(e.started, 3)))?;
    out.write_all(b",\"time\":")?;
    json(out, &json!(wait + receive))?;
    out.write_all(b",\"request\":")?;
    json(out, &request)?;
    out.write_all(b",\"response\":{\"status\":")?;
    json(out, &json!(e.status))?;
    out.write_all(b",\"statusText\":")?;
    json(out, &json!(e.reason))?;
    out.write_all(b",\"httpVersion\":")?;
    json(out, &json!(e.version))?;
    out.write_all(b",\"cookies\":[],\"headers\":")?;
    json(out, &pairs(&e.response_headers))?;
    out.write_all(b",\"redirectURL\":")?;
    json(
        out,
        &json!(header(&e.response_headers, "location").unwrap_or_default()),
    )?;
    out.write_all(b",\"headersSize\":-1,\"bodySize\":-1,\"content\":{\"size\":")?;
    json(out, &json!(e.body.len()))?;
    out.write_all(b",\"mimeType\":")?;
    json(
        out,
        &json!(header(&e.response_headers, "content-type").unwrap_or_default()),
    )?;
    if e.truncated {
        // HAR's convention for a field of one's own: an underscore.
        out.write_all(b",\"_truncated\":true")?;
    }
    if std::str::from_utf8(&e.body).is_ok() {
        out.write_all(b",\"text\":\"")?;
        write_escaped(out, &e.body)?;
    } else {
        out.write_all(b",\"encoding\":\"base64\",\"text\":\"")?;
        // Whole groups of three bytes, so the pieces join into one string.
        for chunk in e.body.chunks(3 << 14) {
            out.write_all(data_encoding::BASE64.encode(chunk).as_bytes())?;
        }
    }
    out.write_all(b"\"}},\"cache\":{},\"timings\":")?;
    json(
        out,
        &json!({
            "blocked": -1,
            "dns": -1,
            "connect": -1,
            "ssl": -1,
            "send": 0,
            "wait": wait,
            "receive": receive,
        }),
    )?;
    out.write_all(b"}")
}

/// `text` (valid UTF-8) as the inside of a JSON string. Newlines are
/// escaped with the rest, so an entry stays on one line.
fn write_escaped(out: &mut dyn Write, text: &[u8]) -> io::Result<()> {
    let mut from = 0;
    for (i, &b) in text.iter().enumerate() {
        let escaped: Cow<'_, [u8]> = match b {
            b'"' => Cow::Borrowed(b"\\\""),
            b'\\' => Cow::Borrowed(b"\\\\"),
            b'\n' => Cow::Borrowed(b"\\n"),
            b'\r' => Cow::Borrowed(b"\\r"),
            b'\t' => Cow::Borrowed(b"\\t"),
            0..0x20 => Cow::Owned(format!("\\u{b:04x}").into_bytes()),
            _ => continue,
        };
        out.write_all(&text[from..i])?;
        out.write_all(&escaped)?;
        from = i + 1;
    }
    out.write_all(&text[from..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_written_in_utc_to_the_digit_asked_for() {
        let t = UNIX_EPOCH + Duration::new(1_791_797_400, 123_456_789);
        assert_eq!(iso8601(t, 6), "2026-10-12T09:30:00.123456Z");
        assert_eq!(iso8601(t, 3), "2026-10-12T09:30:00.123Z");
        assert_eq!(iso8601(UNIX_EPOCH, 3), "1970-01-01T00:00:00.000Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(iso8601(leap, 3), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn record_ids_are_version_4_uuids_and_differ() {
        let (a, b) = (record_id(), record_id());
        assert_ne!(a, b);
        assert_eq!(a.len(), "<urn:uuid:>".len() + 36);
        assert_eq!(&a[24..25], "4", "{a}");
        assert!("89ab".contains(&a[29..30]), "{a}");
    }

    #[test]
    fn digests_are_sha1_in_base_32() {
        assert_eq!(digest(b""), "sha1:3I42H3S6NNFQ2MSVX7XZKYAYSCX5QBYJ");
        // As warcio wrote it for this body.
        assert_eq!(digest(b"hello"), "sha1:VL2MMHO4YXUKFWV63YHTWSBM3GXKSQ2N");
    }

    fn exchange() -> Exchange {
        Exchange {
            started: SystemTime::now(),
            url: "http://example.com/".into(),
            method: "GET".into(),
            request_headers: vec![("Host".into(), b"example.com".to_vec())],
            request_body: None,
            version: "HTTP/1.1",
            status: 200,
            reason: "OK".into(),
            response_headers: Vec::new(),
            body: Bytes::from_static(b"hi"),
            truncated: false,
            wait: Duration::ZERO,
            receive: Duration::ZERO,
        }
    }

    fn entry(e: &Exchange) -> serde_json::Value {
        let mut line = Vec::new();
        write_entry(&mut line, e).unwrap();
        assert!(!line.contains(&b'\n'), "one line");
        serde_json::from_slice(&line).unwrap()
    }

    #[test]
    fn a_failed_write_is_told_once_and_the_file_gets_nothing_more() {
        let dir = std::env::temp_dir().join(format!("netweir-archive-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.warc");
        let mut files = Files {
            warc: Some(Warc::open(&path, false).unwrap()),
            har: None,
            status: Arc::default(),
            per_exchange: 1,
        };
        // A handle that can't be written to stands in for a full disk.
        let warc = files.warc.as_mut().unwrap();
        warc.sink.file = File::open(&path).unwrap();
        let (queue, inbox) = mpsc::unbounded_channel();
        let status = files.status.clone();
        let room = Arc::new(Semaphore::new(10));
        let writer = std::thread::spawn(move || files.write_loop(inbox));
        for _ in 0..3 {
            let permits = [
                room.clone().try_acquire_owned().unwrap(),
                room.clone().try_acquire_owned().unwrap(),
            ];
            queue
                .send(Message::Record(Box::new(exchange()), permits))
                .unwrap();
        }
        drop(queue);
        let _ = writer.join().unwrap();
        assert_eq!(status.errors.load(Ordering::Relaxed), 3);
        let warnings = std::mem::take(&mut *status.warnings.lock().unwrap());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("a.warc"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory of its own for a test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "netweir-archive-unit-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn closing_while_fetches_wait_for_room_returns_on_their_own_thread() {
        // Everything on one thread, as a current-thread runtime runs it:
        // the fetches waiting for a place can't be polled while closing
        // blocks, so closing mustn't wait for any of them.
        let dir = temp_dir("close");
        let path = dir.join("busy.warc");
        let (done, outcome) = std::sync::mpsc::channel();
        let archive = Archive::new(Some(&path), None, false).unwrap();
        let waiting = archive.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let fetches: Vec<_> = (0..2 * QUEUE + 20)
                    .map(|_| {
                        let archive = waiting.clone();
                        tokio::spawn(async move { archive.record(exchange()).await })
                    })
                    .collect();
                // Until the queue is full and the rest are waiting.
                while waiting.inner.slots.available_permits() > 0 {
                    tokio::task::yield_now().await;
                }
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                waiting.start().unwrap();
                waiting.close().unwrap();
                for fetch in fetches {
                    fetch.await.unwrap();
                }
            });
            let _ = done.send(());
        });
        outcome
            .recv_timeout(Duration::from_secs(30))
            .expect("closing hung");
        // The queue was written; those that were still waiting weren't.
        assert_eq!(archive.errors(), QUEUE as u64 + 20);
        let written = std::fs::read(&path).unwrap();
        let responses = written
            .windows(b"WARC-Type: response".len())
            .filter(|w| *w == b"WARC-Type: response")
            .count();
        assert_eq!(responses, QUEUE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_har_file_whose_head_was_cut_off_starts_again() {
        let dir = temp_dir("head");
        let path = dir.join("torn.har");
        std::fs::write(&path, &HAR_START[..HAR_START.len() - 3]).unwrap();
        let (prepared, any) = Har::prepare(&path, true).unwrap();
        assert_eq!((prepared.kept, any), (0, false));
        let har = Har::finish((prepared, any)).unwrap();
        drop(har);
        let whole = std::fs::read(&path).unwrap();
        assert!(whole.starts_with(HAR_START) && whole.ends_with(HAR_HEAD_END));
        // A whole head with no entry yet is kept as it is.
        let (prepared, any) = Har::prepare(&path, true).unwrap();
        assert_eq!((prepared.kept, any), (whole.len() as u64, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_har_file_that_cant_be_added_to_leaves_the_warc_file_alone() {
        let dir = temp_dir("both");
        let (warc, har) = (dir.join("a.warc"), dir.join("a.har"));
        let first = Archive::new(Some(&warc), Some(&har), false).unwrap();
        first.start().unwrap();
        first.close().unwrap();
        let before = std::fs::read(&warc).unwrap();
        let again = Archive::new(Some(&warc), Some(&har), true).unwrap();
        // Changed between the check and the start.
        std::fs::write(&har, "not a HAR file").unwrap();
        let refused = again.start().unwrap_err();
        assert!(refused.contains("a.har"), "{refused}");
        assert_eq!(std::fs::read(&warc).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_warc_and_har_files_cant_be_one_file() {
        let dir = temp_dir("same");
        let warc = dir.join("crawl.warc");
        let same = |har: &Path| Archive::new(Some(&warc), Some(har), false).is_err();
        assert!(same(&warc));
        assert!(same(&dir.join(".").join("crawl.warc")));
        // Letter case aside, as a case-insensitive disk sees it.
        assert!(same(&dir.join("Crawl.WARC")));
        assert!(!same(&dir.join("crawl.har")));
        #[cfg(unix)]
        {
            std::fs::write(&warc, "").unwrap();
            std::os::unix::fs::symlink(&warc, dir.join("link.har")).unwrap();
            assert!(same(&dir.join("link.har")));
            std::fs::hard_link(&warc, dir.join("hard.har")).unwrap();
            assert!(same(&dir.join("hard.har")));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_har_request_has_the_version_it_went_out_in() {
        let mut e = exchange();
        e.version = "HTTP/1.0";
        assert_eq!(entry(&e)["request"]["httpVersion"], "HTTP/1.1");
        assert_eq!(entry(&e)["response"]["httpVersion"], "HTTP/1.0");
        e.version = "HTTP/2";
        assert_eq!(entry(&e)["request"]["httpVersion"], "HTTP/2");
        assert!(entry(&e)["response"]["content"].get("_truncated").is_none());
        e.truncated = true;
        assert_eq!(entry(&e)["response"]["content"]["_truncated"], true);
    }

    #[test]
    fn a_body_that_isnt_utf8_is_base64_in_the_har_file() {
        let mut e = exchange();
        e.body = Bytes::from_static(&[0x89, b'P', b'N', b'G', 0xff]);
        let first = entry(&e);
        assert_eq!(first["response"]["content"]["encoding"], "base64");
        assert_eq!(first["response"]["content"]["text"], "iVBOR/8=");
        assert_eq!(first["response"]["content"]["size"], 5);
        // Long enough to be written in several pieces.
        let long: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8 | 0x80).collect();
        e.body = Bytes::from(long.clone());
        let text = entry(&e)["response"]["content"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(data_encoding::BASE64.decode(text.as_bytes()).unwrap(), long);
    }

    #[test]
    fn text_is_escaped_as_json_strings_are() {
        let mut e = exchange();
        let text = "line one\nquote \" back \\ tab\t bell\u{7} caf\u{e9} \u{2028}";
        e.body = Bytes::from(text);
        assert_eq!(entry(&e)["response"]["content"]["text"], text);
    }

    #[test]
    fn a_form_body_has_its_params() {
        let mut e = exchange();
        e.method = "POST".into();
        e.request_headers.push((
            "Content-Type".into(),
            b"application/x-www-form-urlencoded".to_vec(),
        ));
        e.request_body = Some(b"q=rust+lang&page=2&x=%C3%A9".to_vec());
        let post = entry(&e)["request"]["postData"].clone();
        assert_eq!(post["text"], "q=rust+lang&page=2&x=%C3%A9");
        assert_eq!(
            post["params"],
            serde_json::json!([
                {"name": "q", "value": "rust lang"},
                {"name": "page", "value": "2"},
                {"name": "x", "value": "\u{e9}"},
            ])
        );
        e.request_headers[1].1 = b"application/json".to_vec();
        e.request_body = Some(b"{}".to_vec());
        assert!(entry(&e)["request"]["postData"].get("params").is_none());
    }

    fn stored(
        status: u16,
        method: &str,
        headers: &[(&str, &str)],
        body: &'static [u8],
    ) -> Vec<(String, String)> {
        let mut e = exchange();
        e.status = status;
        e.method = method.into();
        e.body = Bytes::from_static(body);
        e.response_headers = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
            .collect();
        stored_headers(&e)
            .into_iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8(v.into_owned()).unwrap()))
            .collect()
    }

    #[test]
    fn content_length_is_the_stored_bodys_only_where_it_can_be() {
        let pairs = |p: &[(&str, &str)]| -> Vec<(String, String)> {
            p.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        // As sent, when it's right.
        assert_eq!(
            stored(200, "GET", &[("content-length", "2"), ("x", "1")], b"hi"),
            pairs(&[("content-length", "2"), ("x", "1")])
        );
        // Chunked, or decompressed by the client: the stored length added.
        assert_eq!(
            stored(
                200,
                "GET",
                &[("transfer-encoding", "chunked"), ("x", "1")],
                b"hi"
            ),
            pairs(&[("x", "1"), ("content-length", "2")])
        );
        // No body to measure: left alone.
        for status in [204, 304, 101] {
            assert_eq!(
                stored(status, "GET", &[("x", "1")], b""),
                pairs(&[("x", "1")])
            );
        }
        assert_eq!(
            stored(304, "GET", &[("content-length", "900")], b""),
            pairs(&[("content-length", "900")])
        );
        assert_eq!(
            stored(200, "HEAD", &[("content-length", "1234")], b""),
            pairs(&[("content-length", "1234")])
        );
    }
}
