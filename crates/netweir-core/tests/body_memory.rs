//! How much memory reading a large body takes, counted by an allocator of
//! this test's own. A test binary of its own, so nothing else allocates
//! while it counts.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use netweir_core::{FetchOptions, Fetcher, Profile};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The bytes allocated now, and the most there have been.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::SeqCst) + by;
    PEAK.fetch_max(live, Ordering::SeqCst);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.dealloc(ptr, layout) }
    }

    /// Counted as the new block made beside the old, which is what the
    /// system allocator does when it can't grow a block where it is.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        grew(new_size);
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[tokio::test]
async fn a_large_body_is_read_into_one_buffer_of_its_size() {
    const SIZE: usize = 32 << 20;
    let body: Vec<u8> = (0..SIZE).map(|i| i as u8).collect();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        let head =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\nConnection: close\r\n\r\n");
        let _ = s.write_all(head.as_bytes()).await;
        // A piece at a time, as a slow network delivers it.
        for piece in body.chunks(16 << 10) {
            if s.write_all(piece).await.is_err() {
                return;
            }
            tokio::task::yield_now().await;
        }
        let _ = s.shutdown().await;
    });
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.timeout = Duration::from_secs(60);
    options.max_body = Some(64 << 20);
    let fetcher = Fetcher::new(options).unwrap();

    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let response = fetcher
        .get(&format!("http://127.0.0.1:{port}/"), &[])
        .await
        .unwrap();
    let peak = PEAK.load(Ordering::SeqCst) - before;
    assert_eq!(response.body.len(), SIZE);
    // The body, and the network's buffers beside it: never the body's
    // last buffer and the one it was copied into.
    assert!(peak < SIZE + SIZE / 4, "{} MiB at the peak", peak >> 20);
}

#[tokio::test]
async fn a_length_no_one_could_hold_doesnt_end_the_process() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        // 4 EiB promised, a few bytes sent.
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\nshort",
            1u64 << 62
        );
        let _ = s.write_all(head.as_bytes()).await;
        let _ = s.shutdown().await;
    });
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.timeout = Duration::from_secs(10);
    // A limit set as high as it goes: the declared length passes it.
    options.max_body = Some(u64::MAX);
    let fetcher = Fetcher::new(options).unwrap();
    // The body ends short of what was declared: an error, not an abort.
    assert!(
        fetcher
            .get(&format!("http://127.0.0.1:{port}/"), &[])
            .await
            .is_err()
    );
}
