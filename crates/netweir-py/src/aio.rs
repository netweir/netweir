//! Rust futures as Python awaitables, handed over so that no Rust thread
//! is still in Python when Python exits.
//!
//! A thread that isn't Python's own and needs the GIL while the interpreter
//! is shutting down is ended there by CPython before 3.14 (with
//! `pthread_exit`). On Linux that unwinds through Rust frames holding
//! Python objects, which then touch Python without the GIL: a crash at
//! exit. pyo3-async-runtimes, which this replaces, hit it whenever the last
//! thing a program awaited was one of ours: its Rust thread wakes the event
//! loop from inside Python, so the program could run to its end before
//! that thread had let go of the GIL.
//!
//! Every Rust thread in Python is counted ([`attach`]), and an `atexit`
//! hook waits, without the GIL, for the count to reach zero, so the thread
//! that delivered a program's last result is out of Python before shutdown
//! begins. atexit runs hooks last-registered first, so hooks registered
//! before `import netweir` run after ours; a result awaited there is set
//! only once the thread that delivered it has left Python (see
//! [`_settle`]). Before the hook has run, results are set at once: waiting
//! for the delivering thread on every result would cost two GIL hand-offs
//! each, for nothing, as the hook covers them.
//!
//! Known limit: both waits are capped at a second, so a delivering thread
//! held in Python longer than that (by a `__del__` that waits for the
//! loop's thread, say), and Rust work nobody awaits that enters Python
//! after the hook, can still meet the crash on Linux before Python 3.14.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use pyo3::IntoPyObjectExt;
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use tokio::task::AbortHandle;

/// Threads inside [`attach`] right now.
static ATTACHED: AtomicUsize = AtomicUsize::new(0);
/// Set once the `atexit` hook has run: from then on, results wait for the
/// thread that delivered them.
static EXITING: AtomicBool = AtomicBool::new(false);
/// Bumped in a forked child, whose parent's threads don't exist there.
static GENERATION: AtomicUsize = AtomicUsize::new(0);

/// How long Python's exit waits for a Rust thread to leave it.
const EXIT_WAIT: Duration = Duration::from_secs(1);

/// Runs `f` attached to Python, counted in [`ATTACHED`] for the `atexit`
/// hook.
pub(crate) fn attach<R>(f: impl FnOnce(Python<'_>) -> R) -> R {
    let _in = In::new();
    Python::attach(f)
}

/// Counts this thread in [`ATTACHED`] while it lives.
struct In;

impl In {
    fn new() -> In {
        ATTACHED.fetch_add(1, Ordering::SeqCst);
        In
    }
}

impl Drop for In {
    fn drop(&mut self) {
        ATTACHED.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Registered with `atexit` when the module loads: waits, without the GIL,
/// for Rust threads that are in Python.
#[pyfunction]
pub(crate) fn _wait_for_rust_threads(py: Python<'_>) {
    EXITING.store(true, Ordering::SeqCst);
    wait_without_gil(py, || ATTACHED.load(Ordering::SeqCst) == 0);
}

/// Waits up to [`EXIT_WAIT`] for `done`, letting other threads have the GIL.
fn wait_without_gil(py: Python<'_>, done: impl Fn() -> bool + Send + Sync) {
    if done() {
        return;
    }
    let until = Instant::now() + EXIT_WAIT;
    py.detach(|| {
        while !done() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
}

/// Registered with `os.register_at_fork`: a child has only the thread that
/// forked, so no thread of its parent's is in Python there.
#[pyfunction]
pub(crate) fn _forked() {
    ATTACHED.store(0, Ordering::SeqCst);
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// `fut` as an asyncio future on the running loop. Cancelling the asyncio
/// future stops `fut`; a panic in it raises `PanicException`.
pub(crate) fn future_into_py<F, T>(py: Python<'_>, fut: F) -> PyResult<Bound<'_, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    let event_loop = pyo3_async_runtimes::get_running_loop(py)?;
    let future = event_loop.call_method0("create_future")?;
    let (to_loop, to_future) = (event_loop.unbind(), future.clone().unbind());
    let runtime = pyo3_async_runtimes::tokio::get_runtime();
    // Cancelled, the work is dropped on a runtime thread, and may hold
    // Python objects (a `self` to return, say).
    let work = runtime.spawn(Release(Some(Box::pin(fut))));
    let abort = work.abort_handle();
    runtime.spawn(async move {
        let result = match work.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => Err(PanicException::new_err(panic_message(e.into_panic()))),
            // Cancelled from Python: the future is already done.
            Err(_) => return release((to_loop, to_future)),
        };
        // Waiting for the GIL would hold up a runtime thread.
        tokio::task::spawn_blocking(move || hand_over(to_loop, to_future, result));
    });
    future.call_method1("add_done_callback", (Abort(abort),))?;
    Ok(future)
}

/// Lets go of Python objects from a thread that isn't in Python.
///
/// The extension is built without PyO3's pool of references dropped
/// outside Python: with it, every call from Python into Rust took one
/// process-wide lock, the whole of what kept threads from scaling on
/// free-threaded Python (docs/design/threads.md). So nothing may drop a
/// Python object on a thread that isn't attached. A runtime thread hands
/// them to a blocking thread, as waiting for the GIL would hold it up.
/// Once Python is shutting down they are leaked, as the pool did.
pub(crate) fn release<T: Send + 'static>(objects: T) {
    fn now<T>(objects: T) {
        let mut objects = Some(objects);
        // Counted like attach(), so Python's exit waits for this thread
        // to leave (a task cancelled as asyncio.run() ends lands here).
        let _in = In::new();
        if Python::try_attach(|_| drop(objects.take())).is_none() {
            std::mem::forget(objects);
        }
    }
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn_blocking(move || now(objects))),
        Err(_) => now(objects),
    }
}

/// A future that, dropped before it finishes, drops what it holds through
/// [`release`].
struct Release<F: Send + 'static>(Option<Pin<Box<F>>>);

impl<F: Future + Send + 'static> Future for Release<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let fut = self.0.as_mut().expect("polled after it finished");
        let out = fut.as_mut().poll(cx);
        if out.is_ready() {
            self.0 = None;
        }
        out
    }
}

impl<F: Send + 'static> Drop for Release<F> {
    fn drop(&mut self) {
        if let Some(fut) = self.0.take() {
            release(fut);
        }
    }
}

fn panic_message(panic: Box<dyn Any + Send>) -> String {
    let what = panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_string());
    format!("Rust code panicked: {what}")
}

/// Has the loop's thread set the result, once this thread is out of Python.
fn hand_over<T>(to_loop: Py<PyAny>, to_future: Py<PyAny>, result: PyResult<T>)
where
    T: for<'py> IntoPyObject<'py>,
{
    let out = Arc::new(AtomicBool::new(false));
    // Dropped last, after `attach` has let go of the GIL; dropped on a
    // panic too, so `_settle` never waits for nothing.
    let _out = Out(out.clone());
    attach(move |py| {
        let (ok, value) = match result.and_then(|v| v.into_py_any(py)) {
            Ok(v) => (true, v),
            Err(e) => (false, e.into_value(py).into_any()),
        };
        // An error here means the loop is closed: whatever awaited the
        // future is gone, and there is no one to tell.
        let _ = to_loop.call_method1(
            py,
            "call_soon_threadsafe",
            (
                settle(py),
                to_future,
                ok,
                value,
                Handed {
                    out,
                    generation: GENERATION.load(Ordering::SeqCst),
                },
            ),
        );
    });
}

struct Out(Arc<AtomicBool>);

impl Drop for Out {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Whether the thread that handed a result over has left Python, and in
/// which process (by fork generation).
#[pyclass(frozen)]
struct Handed {
    out: Arc<AtomicBool>,
    generation: usize,
}

/// On the loop's thread: sets the future's result or exception, unless
/// it's already done (cancelled while the result was on its way). Once
/// Python is exiting, first waits for the thread that handed it over to
/// leave Python (it needs at most the GIL, released meanwhile).
#[pyfunction]
fn _settle(
    py: Python<'_>,
    future: &Bound<'_, PyAny>,
    ok: bool,
    value: &Bound<'_, PyAny>,
    handed: &Bound<'_, Handed>,
) -> PyResult<()> {
    let handed = handed.get();
    if EXITING.load(Ordering::SeqCst) && handed.generation == GENERATION.load(Ordering::SeqCst) {
        let out = &handed.out;
        wait_without_gil(py, || out.load(Ordering::SeqCst));
    }
    if future.call_method0("done")?.is_truthy()? {
        return Ok(());
    }
    let method = if ok { "set_result" } else { "set_exception" };
    future.call_method1(method, (value,))?;
    Ok(())
}

fn settle(py: Python<'_>) -> &Py<PyAny> {
    static SETTLE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    SETTLE.get_or_init(py, || {
        wrap_pyfunction!(_settle, py)
            .expect("a pyfunction can always be wrapped")
            .into_any()
            .unbind()
    })
}

/// Stops the Rust work behind a future that was cancelled.
#[pyclass(frozen)]
struct Abort(AbortHandle);

#[pymethods]
impl Abort {
    fn __call__(&self, future: &Bound<'_, PyAny>) -> PyResult<()> {
        if future.call_method0("cancelled")?.is_truthy()? {
            self.0.abort();
        }
        Ok(())
    }
}

/// For tests: a future that resolves to None after `seconds`.
#[pyfunction]
pub(crate) fn _test_sleep(py: Python<'_>, seconds: f64) -> PyResult<Bound<'_, PyAny>> {
    future_into_py(py, async move {
        tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
        Ok(())
    })
}

/// For tests: a future whose Rust work panics.
#[pyfunction]
pub(crate) fn _test_panic(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    future_into_py(py, async move {
        if std::hint::black_box(true) {
            panic!("the test asked for a panic");
        }
        Ok(())
    })
}
