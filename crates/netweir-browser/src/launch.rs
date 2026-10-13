//! Finding Chrome and starting it with the DevTools pipe.

use std::io::{PipeReader, PipeWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

use crate::{Error, Result};

/// netweir's own folder: `$NETWEIR_HOME`, or `~/.netweir`.
pub fn netweir_home() -> PathBuf {
    if let Some(home) = std::env::var_os("NETWEIR_HOME") {
        return PathBuf::from(home);
    }
    let user = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    user.map(PathBuf::from).unwrap_or_default().join(".netweir")
}

/// This machine as Chrome for Testing names platforms, if it has builds
/// for it.
pub fn cft_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("mac-arm64"),
        ("macos", "x86_64") => Some("mac-x64"),
        ("linux", "x86_64") => Some("linux64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("windows", "x86_64") => Some("win64"),
        ("windows", "x86") => Some("win32"),
        _ => None,
    }
}

/// Where the executable is in an unpacked Chrome for Testing download for
/// `platform`: Chrome itself, or `chrome-headless-shell`.
pub fn cft_executable(platform: &str, headless_shell: bool) -> PathBuf {
    let exe = if platform.starts_with("win") {
        ".exe"
    } else {
        ""
    };
    if headless_shell {
        return PathBuf::from(format!(
            "chrome-headless-shell-{platform}/chrome-headless-shell{exe}"
        ));
    }
    if platform.starts_with("mac") {
        return PathBuf::from(format!(
            "chrome-{platform}/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
        ));
    }
    PathBuf::from(format!("chrome-{platform}/chrome{exe}"))
}

/// The newest Chrome `netweir install chrome` put under `home`.
pub(crate) fn installed_chrome(home: &Path) -> Option<PathBuf> {
    let platform = cft_platform()?;
    let mut versions: Vec<(Vec<u32>, PathBuf)> = std::fs::read_dir(home.join("chrome"))
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let version: Option<Vec<u32>> = name.split('.').map(|n| n.parse().ok()).collect();
            Some((version?, e.path()))
        })
        .collect();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|(_, dir)| dir.join(cft_executable(platform, false)))
        .find(|p| p.is_file())
}

/// More Chrome switches, from `$NETWEIR_CHROME_ARGS`, for every Chrome
/// netweir launches: what a machine needs whatever the code says, such as
/// `--disable-gpu` on a virtual Mac whose GPU hangs.
pub(crate) fn env_switches() -> Vec<String> {
    switches_from(std::env::var_os("NETWEIR_CHROME_ARGS"))
}

fn switches_from(value: Option<std::ffi::OsString>) -> Vec<String> {
    value
        .map(|v| {
            v.to_string_lossy()
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The Chrome to launch: `$NETWEIR_CHROME`, then the newest one
/// `netweir install chrome` installed, then the usual install paths
/// (Chrome, Chrome for Testing, Chromium). The error lists where it
/// looked.
pub fn find_chrome() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NETWEIR_CHROME") {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(Error::Launch(format!(
                "$NETWEIR_CHROME is {}, which doesn't exist",
                path.display()
            )))
        };
    }
    let home = netweir_home();
    if let Some(installed) = installed_chrome(&home) {
        return Ok(installed);
    }
    let candidates = candidates();
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            let looked: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            Error::Launch(format!(
                "no Chrome found; run `netweir install chrome`, set $NETWEIR_CHROME or pass \
                 executable=. Looked in: {}, {}",
                home.join("chrome").display(),
                looked.join(", ")
            ))
        })
}

#[cfg(target_os = "macos")]
fn candidates() -> Vec<PathBuf> {
    let apps = [
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        "Chromium.app/Contents/MacOS/Chromium",
    ];
    let mut dirs = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join("Applications"));
    }
    dirs.iter()
        .flat_map(|d| apps.iter().map(move |a| d.join(a)))
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn candidates() -> Vec<PathBuf> {
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "chromium",
        "chromium-browser",
    ];
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut found: Vec<PathBuf> = std::env::split_paths(&path)
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .collect();
    found.push(PathBuf::from("/opt/google/chrome/chrome"));
    found
}

#[cfg(windows)]
fn candidates() -> Vec<PathBuf> {
    let suffixes = [
        r"Google\Chrome\Application\chrome.exe",
        r"Google\Chrome for Testing\Application\chrome.exe",
        r"Chromium\Application\chrome.exe",
    ];
    ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"]
        .iter()
        .filter_map(std::env::var_os)
        .flat_map(|d| suffixes.iter().map(move |s| Path::new(&d).join(s)))
        .collect()
}

/// A started Chrome: the process, and the pipe's two ends on our side.
pub(crate) struct Started {
    pub child: Child,
    pub commands: PipeWriter,
    pub replies: PipeReader,
    pub said: Said,
}

/// The last lines Chrome wrote to stderr, so an error can say why it
/// wouldn't start. Read on a thread of its own until Chrome closes it,
/// which also keeps a chatty Chrome from filling the pipe and stalling.
#[derive(Clone, Default)]
pub(crate) struct Said(Arc<(Mutex<Lines>, Condvar)>);

/// The lines kept, and whether Chrome has closed stderr.
type Lines = (std::collections::VecDeque<String>, bool);

impl Said {
    fn read(stderr: ChildStderr) -> Said {
        let said = Said::default();
        let writing = said.clone();
        let _ = std::thread::Builder::new()
            .name("netweir-chrome-stderr".into())
            .spawn(move || {
                use std::io::BufRead;
                let (lock, done) = &*writing.0;
                for line in std::io::BufReader::new(stderr)
                    .lines()
                    .map_while(|l| l.ok())
                {
                    let mut lines = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if lines.0.len() == 20 {
                        lines.0.pop_front();
                    }
                    lines.0.push_back(line);
                }
                lock.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
                done.notify_all();
            });
        said
    }

    /// `message`, with what Chrome has said lately, if anything, after
    /// it; for a Chrome still running.
    pub(crate) fn lately(&self, message: String) -> String {
        match self.recent() {
            Some(said) => format!("{message}; Chrome said lately:\n{said}"),
            None => message,
        }
    }

    /// What Chrome has lately written to stderr, if anything.
    pub(crate) fn recent(&self) -> Option<String> {
        let lines = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
        if lines.0.is_empty() {
            return None;
        }
        let said: Vec<&str> = lines.0.iter().map(String::as_str).collect();
        Some(said.join("\n"))
    }

    /// `message`, with what Chrome said, if anything, after it. Waits up
    /// to a second for a Chrome that has just exited to finish saying it;
    /// only ever on the way to an error.
    pub(crate) fn explain(&self, message: String) -> String {
        let (lock, done) = &*self.0;
        let lines = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (lines, _) = done
            .wait_timeout_while(lines, std::time::Duration::from_secs(1), |l| !l.1)
            .unwrap_or_else(|e| e.into_inner());
        if lines.0.is_empty() {
            return message;
        }
        let said: Vec<&str> = lines.0.iter().map(String::as_str).collect();
        format!("{message}; Chrome said:\n{}", said.join("\n"))
    }
}

pub(crate) fn start(executable: &Path, args: &[String]) -> Result<Started> {
    if !executable.is_file() {
        return Err(Error::Launch(format!(
            "no Chrome at {}",
            executable.display()
        )));
    }
    let failed =
        |e: std::io::Error| Error::Launch(format!("can't start {}: {e}", executable.display()));
    // Chrome reads commands from one pipe and writes replies to the other.
    let (chrome_reads, commands) = std::io::pipe().map_err(failed)?;
    let (replies, chrome_writes) = std::io::pipe().map_err(failed)?;
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // On Windows, Chrome's ends are inheritable from hand_over until they're
    // dropped, and any child started meanwhile would get them too; then a
    // browser's reader would never see its Chrome exit. Launches here take
    // turns through that window.
    static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let child = {
        let _turn = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        hand_over(&mut command, &chrome_reads, &chrome_writes).map_err(failed)?;
        let child = command.spawn().map_err(failed)?;
        // Our copies of Chrome's ends must close, or we'd never see it exit.
        drop((chrome_reads, chrome_writes));
        child
    };
    let mut child = child;
    let said = child.stderr.take().map(Said::read).unwrap_or_default();
    Ok(Started {
        child,
        commands,
        replies,
        said,
    })
}

/// Gives Chrome its ends of the pipe as file descriptors 3 and 4, where
/// `--remote-debugging-pipe` expects them.
#[cfg(unix)]
fn hand_over(
    command: &mut Command,
    reads: &PipeReader,
    writes: &PipeWriter,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    command.arg("--remote-debugging-pipe");
    let (reads, writes) = (reads.as_raw_fd(), writes.as_raw_fd());
    // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and
    // exec.
    unsafe {
        command.pre_exec(move || {
            // Either end may already be 3 or 4; move both out of the way
            // first so the dup2s can't clobber each other. dup2 clears
            // close-on-exec on 3 and 4; the moved copies keep it.
            let reads = libc::fcntl(reads, libc::F_DUPFD_CLOEXEC, 5);
            let writes = libc::fcntl(writes, libc::F_DUPFD_CLOEXEC, 5);
            if reads < 0 || writes < 0 || libc::dup2(reads, 3) < 0 || libc::dup2(writes, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Windows has no numbered descriptors to hand over: the pipe handles are
/// made inheritable and their values passed on the command line.
#[cfg(windows)]
fn hand_over(
    command: &mut Command,
    reads: &PipeReader,
    writes: &PipeWriter,
) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    // Known limit: start() keeps netweir's own launches apart, but a process
    // started elsewhere in the program during the launch would still
    // inherit these. PROC_THREAD_ATTRIBUTE_HANDLE_LIST would make it exact.
    for handle in [reads.as_raw_handle(), writes.as_raw_handle()] {
        if unsafe { SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    command.arg("--remote-debugging-pipe").arg(format!(
        "--remote-debugging-io-pipes={},{}",
        reads.as_raw_handle() as usize,
        writes.as_raw_handle() as usize
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_from_the_environment_are_split_on_whitespace() {
        assert_eq!(
            switches_from(Some("--disable-gpu  --no-sandbox\n".into())),
            ["--disable-gpu", "--no-sandbox"]
        );
        assert!(switches_from(Some("   ".into())).is_empty());
        assert!(switches_from(None).is_empty());
    }

    #[test]
    fn the_newest_installed_chrome_is_found() {
        let Some(platform) = cft_platform() else {
            return;
        };
        let home = std::env::temp_dir().join(format!("netweir-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(installed_chrome(&home), None);
        for version in [
            "154.0.8037.98",
            "154.0.8037.110",
            "153.0.1.1",
            "not-a-version",
        ] {
            let exe = home
                .join("chrome")
                .join(version)
                .join(cft_executable(platform, false));
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, "").unwrap();
        }
        let found = installed_chrome(&home).unwrap();
        assert!(
            found.starts_with(home.join("chrome").join("154.0.8037.110")),
            "{}",
            found.display()
        );
        std::fs::remove_dir_all(&home).unwrap();
    }
}
