//! What a headless Chrome said it is, kept on disk between programs.
//! Asking means starting a Chrome of its own: 0.9 seconds and 1.6 seconds
//! of CPU on an M2 Max, at the start of every program that launched one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

/// Answers kept, newest first: one per Chrome and switches in use.
const KEPT: usize = 16;

/// What an answer is kept under: the executable as it is now, so a Chrome
/// updated in place is asked again; the switches it's started with, which
/// can change what it says; and the system under it, whose version its
/// client hints carry.
pub(crate) fn key(executable: &Path, extra: &[String]) -> Value {
    let meta = std::fs::metadata(executable).ok();
    json!({
        "executable": executable.to_string_lossy(),
        "modified": meta.as_ref().and_then(|m| nanos(m.modified().ok()?)),
        "len": meta.map_or(0, |m| m.len()),
        "switches": extra,
        "system": system(),
    })
}

fn nanos(time: std::time::SystemTime) -> Option<String> {
    Some(time.duration_since(UNIX_EPOCH).ok()?.as_nanos().to_string())
}

/// Something that changes when the system is updated.
fn system() -> String {
    let stamp = if cfg!(target_os = "macos") {
        std::fs::read("/System/Library/CoreServices/SystemVersion.plist").ok()
    } else if cfg!(target_os = "linux") {
        std::fs::read("/proc/sys/kernel/osrelease").ok()
    } else if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        std::fs::metadata(Path::new(&root).join("System32").join("ntoskrnl.exe"))
            .ok()
            .and_then(|m| Some(format!("{} {}", nanos(m.modified().ok()?)?, m.len())))
            .map(String::into_bytes)
    } else {
        None
    };
    // FNV-1a: the file's contents would do, but they're half a kilobyte.
    let hash = stamp
        .unwrap_or_default()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// Where answers are kept; none in this crate's own tests unless one
/// names a file.
pub(crate) fn file() -> Option<PathBuf> {
    #[cfg(test)]
    return FILE.with(|f| f.borrow().clone());
    #[cfg(not(test))]
    Some(crate::launch::netweir_home().join("chrome-identity.json"))
}

#[cfg(test)]
thread_local! {
    pub(crate) static FILE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Every answer kept; none from a file that's missing or doesn't parse.
fn read(path: &Path) -> Vec<Value> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|kept| kept["known"].as_array().cloned())
        .unwrap_or_default()
}

/// The user agent and client hints kept under `key`.
pub(crate) fn recall(path: &Path, key: &Value) -> Option<(String, Value)> {
    let entry = read(path).into_iter().find(|e| e["key"] == *key)?;
    let agent = entry["user_agent"].as_str().filter(|a| !a.is_empty())?;
    let hints = &entry["metadata"];
    hints
        .is_object()
        .then(|| (agent.to_string(), hints.clone()))
}

pub(crate) fn remember(path: &Path, key: &Value, agent: &str, hints: &Value) {
    let mut known = others(path, key);
    known.insert(
        0,
        json!({"key": key, "user_agent": agent, "metadata": hints}),
    );
    known.truncate(KEPT);
    write(path, &json!({"known": known}));
}

pub(crate) fn forget(path: &Path, key: &Value) {
    let known = others(path, key);
    write(path, &json!({"known": known}));
}

fn others(path: &Path, key: &Value) -> Vec<Value> {
    read(path)
        .into_iter()
        .filter(|e| e["key"] != *key)
        .collect()
}

/// Written aside and renamed into place, so a program reading at the same
/// moment sees the old file or the new one, never half of one. A file
/// that can't be written costs only the next program a probe.
fn write(path: &Path, kept: &Value) {
    static N: AtomicU64 = AtomicU64::new(0);
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".chrome-identity-{}-{n}", std::process::id()));
    let written =
        std::fs::write(&temp, kept.to_string()).and_then(|()| std::fs::rename(&temp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("netweir-identity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("chrome-identity.json")
    }

    #[test]
    fn an_answer_is_kept_under_its_key_alone() {
        let file = temp_file("kept");
        let exe = std::env::current_exe().unwrap();
        let key = key(&exe, &["--a".into()]);
        let hints = json!({"platform": "macOS"});
        remember(&file, &key, "Mozilla/5.0 HeadlessChrome/154.0.1.2", &hints);
        assert_eq!(
            recall(&file, &key),
            Some(("Mozilla/5.0 HeadlessChrome/154.0.1.2".into(), hints))
        );
        assert_eq!(recall(&file, &super::key(&exe, &["--b".into()])), None);
        forget(&file, &key);
        assert_eq!(recall(&file, &key), None);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_file_that_doesnt_parse_is_none_and_is_replaced() {
        let file = temp_file("broken");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{\"known\": [").unwrap();
        let key = json!("k");
        assert_eq!(recall(&file, &key), None);
        remember(&file, &key, "agent", &json!({}));
        assert_eq!(recall(&file, &key), Some(("agent".into(), json!({}))));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn only_the_newest_answers_are_kept() {
        let file = temp_file("newest");
        for n in 0..KEPT + 3 {
            remember(&file, &json!(n), "agent", &json!({}));
        }
        assert_eq!(read(&file).len(), KEPT);
        assert!(recall(&file, &json!(KEPT + 2)).is_some());
        assert!(recall(&file, &json!(0)).is_none());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}
