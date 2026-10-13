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

/// Written into an install once Chrome's sandbox has been given access to
/// it.
const SANDBOX_GRANTED: &str = ".netweir-sandbox-access";

/// What `icacls` is given to let Chrome's sandboxed processes read and run
/// what's in `dir`: read and execute, inherited by everything in it, for
/// ALL APPLICATION PACKAGES (S-1-15-2-1) and ALL RESTRICTED APPLICATION
/// PACKAGES (S-1-15-2-2), by SID, since the names are translated.
pub fn sandbox_grant_args(dir: &Path) -> Vec<std::ffi::OsString> {
    vec![
        dir.as_os_str().to_owned(),
        "/grant".into(),
        "*S-1-15-2-1:(OI)(CI)(RX)".into(),
        "/grant".into(),
        "*S-1-15-2-2:(OI)(CI)(RX)".into(),
        "/Q".into(),
    ]
}

/// The version folder of a Chrome `netweir install chrome` put under
/// `home`, if `executable` is one.
pub(crate) fn install_dir_of(executable: &Path, home: &Path) -> Option<PathBuf> {
    let root = home.join("chrome");
    let rest = executable.strip_prefix(&root).ok()?;
    let mut parts = rest.components();
    let version = parts.next()?;
    // A version folder holds the build; the executable is inside it.
    parts.next()?;
    Some(root.join(version))
}

/// Lets Chrome's sandbox read the Chrome installed in `dir`, once. On
/// Windows, Chrome runs its network service and other helpers in an
/// AppContainer, which can read only what grants app packages access;
/// Chrome's own installer grants it, and an unpacked download has none,
/// so the network service can't start, and every navigation under way
/// when it fails is aborted. Elsewhere there's nothing to do.
pub fn grant_sandbox_access(dir: &Path) -> std::result::Result<(), String> {
    if !cfg!(windows) || dir.join(SANDBOX_GRANTED).is_file() {
        return Ok(());
    }
    let out = Command::new("icacls")
        .args(sandbox_grant_args(dir))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            format!(
                "can't run icacls to give Chrome's sandbox access to {}: {e}",
                dir.display()
            )
        })?;
    if !out.status.success() {
        return Err(format!(
            "icacls couldn't give Chrome's sandbox access to {} ({}): {}",
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::fs::write(dir.join(SANDBOX_GRANTED), "")
        .map_err(|e| format!("can't mark {} as done: {e}", dir.display()))
}

/// [`grant_sandbox_access`] for `executable`, if it's a Chrome netweir
/// installed; one installed elsewhere is never touched.
pub(crate) fn grant_if_ours(executable: &Path) -> Result<()> {
    match install_dir_of(executable, &netweir_home()) {
        Some(dir) => grant_sandbox_access(&dir).map_err(Error::Launch),
        None => Ok(()),
    }
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

/// The name and value (empty without `=`) of `arg`, if Chrome reads it as
/// a switch: after `--` or `-`, or `/` on Windows, with the name in
/// lower case on Windows, where Chrome lowers it, and as given elsewhere.
/// Surrounding whitespace is trimmed first, as Chrome trims it.
fn switch_in(arg: &str) -> Option<(String, &str)> {
    let arg = arg.trim();
    let prefixes: &[&str] = if cfg!(windows) {
        &["--", "-", "/"]
    } else {
        &["--", "-"]
    };
    let rest = prefixes
        .iter()
        .find_map(|p| arg.strip_prefix(p))
        .filter(|rest| !rest.is_empty())?;
    let (name, value) = rest.split_once('=').unwrap_or((rest, ""));
    let name = if cfg!(windows) {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    };
    Some((name, value))
}

/// How many of `args` Chrome reads for switches: all of them, up to a
/// bare `--`, after which nothing is a switch.
fn switch_end(args: &[String]) -> usize {
    args.iter()
        .position(|a| a.trim() == "--")
        .unwrap_or(args.len())
}

/// The value Chrome takes for the switch `name` (given without its
/// prefix) in `args`: the last one's.
pub(crate) fn last_switch(args: &[String], name: &str) -> Option<String> {
    args[..switch_end(args)]
        .iter()
        .rev()
        .filter_map(|a| switch_in(a))
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.to_string())
}

/// Takes every switch `name` out of `args`, returning their values in
/// order.
pub(crate) fn take_switches(args: &mut Vec<String>, name: &str) -> Vec<String> {
    let end = switch_end(args);
    let mut values = Vec::new();
    let mut kept = Vec::with_capacity(args.len());
    for (i, arg) in args.drain(..).enumerate() {
        match switch_in(&arg) {
            Some((n, v)) if i < end && n == name => values.push(v.to_string()),
            _ => kept.push(arg),
        }
    }
    *args = kept;
    values
}

/// The profile Chrome opens in the user data directory `dir`, as Chrome
/// picks it: `--profile-directory`, then the one Local State says was
/// used last, then `Default`. A name that isn't a single folder inside
/// `dir` (`..`, a path, an absolute one) is refused rather than followed
/// outside it, as Local State from a profile someone else made could
/// ask; Chrome itself wouldn't start with it either, and falling back to
/// `Default` would prepare a profile Chrome doesn't open.
pub(crate) fn profile_in(dir: &Path, args: &[String]) -> Result<String> {
    let name = match last_switch(args, "profile-directory") {
        Some(named) => named,
        None => std::fs::read(dir.join("Local State"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|state| state["profile"]["last_used"].as_str().map(str::to_string))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Default".into()),
    };
    let mut parts = Path::new(&name).components();
    let single = matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    if !single || name.contains(['/', '\\']) {
        return Err(Error::Launch(format!(
            "the profile {name:?} isn't a folder in {}: a profile is named by one folder \
             inside the user data directory",
            dir.display()
        )));
    }
    Ok(name)
}

/// Turns off Chrome's own "Preload pages" setting (Settings > Performance)
/// in the profile Chrome will open in `dir` (see [`profile_in`]). With it
/// on, Chrome fetches the pages a page's speculation rules name, and can
/// show one when a link to it is followed, outside every session netweir
/// has: no route, block or guard would see the request. Off, a page can
/// tell nothing from its own side: speculation rules are still supported,
/// and a real person can have it off too. The rest of the profile's
/// preferences are kept as they are; a file already set this way isn't
/// written.
pub(crate) fn turn_off_preloading(dir: &Path, args: &[String]) -> Result<()> {
    let profile = dir.join(profile_in(dir, args)?);
    let file = profile.join("Preferences");
    let failed = |why: String| {
        Error::Launch(format!(
            "can't turn off preloading in {}: {why}",
            file.display()
        ))
    };
    let mut prefs = match std::fs::read(&file) {
        // Chrome starts afresh from an empty file, as one cut short by a
        // crash can be; anything else that isn't JSON is someone's to
        // look at, not netweir's to replace.
        Ok(bytes) if bytes.trim_ascii().is_empty() => serde_json::json!({}),
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| failed(format!("it isn't valid JSON ({e})")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(failed(e.to_string())),
    };
    let Some(net) = prefs
        .as_object_mut()
        .map(|p| p.entry("net").or_insert_with(|| serde_json::json!({})))
        .and_then(|net| net.as_object_mut())
    else {
        return Err(failed("it isn't the preferences Chrome writes".into()));
    };
    // 2 is "never": what the setting saves when it's switched off.
    let off = serde_json::json!(2);
    if net.get("network_prediction_options") == Some(&off) {
        return Ok(());
    }
    net.insert("network_prediction_options".into(), off);
    std::fs::create_dir_all(&profile).map_err(|e| failed(e.to_string()))?;
    replace(&file, prefs.to_string().as_bytes()).map_err(|e| failed(e.to_string()))
}

/// Puts `contents` in `file` whole, so Chrome never reads half of it: in a
/// file beside it first, then renamed into place. A linked file is
/// changed where it really is, and the link kept; the file keeps its
/// permissions, and a new one is private, as Chrome makes it.
fn replace(file: &Path, contents: &[u8]) -> std::io::Result<()> {
    let (target, permissions) = match std::fs::metadata(file) {
        Ok(meta) => (std::fs::canonicalize(file)?, Some(meta.permissions())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (file.to_path_buf(), None),
        Err(e) => return Err(e),
    };
    let partial = target.with_extension("netweir");
    let written = (|| {
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        std::io::Write::write_all(&mut open.open(&partial)?, contents)?;
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&partial, permissions)?;
        }
        std::fs::rename(&partial, &target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written
}

/// `args` with `features` added to Chrome's `--enable-features`: those
/// already there, in whatever form Chrome reads, are gathered into one
/// switch with them, since Chrome reads only the last. It goes before a
/// bare `--`, after which Chrome reads no switches.
pub(crate) fn enable_features(mut args: Vec<String>, features: &[&str]) -> Vec<String> {
    let mut enabled: Vec<String> = features.iter().map(|f| f.to_string()).collect();
    for list in take_switches(&mut args, "enable-features") {
        enabled.extend(
            list.split(',')
                .filter(|f| !f.is_empty())
                .map(str::to_string),
        );
    }
    let end = switch_end(&args);
    args.insert(end, format!("--enable-features={}", enabled.join(",")));
    args
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
    pub lifeline: Lifeline,
}

/// Chrome runs while netweir holds this, and is ended, helpers and all,
/// once it's let go: when the browser is dropped, and when the program
/// ends, however it ends. Chrome is meant to quit when its end of the
/// DevTools pipe closes, but doesn't always: Google Chrome 155, headless,
/// closes its pages and then runs on with nothing to drive it, for good.
/// See [`tie`].
pub(crate) struct Lifeline {
    #[cfg(unix)]
    end: Option<PipeWriter>,
    #[cfg(unix)]
    watcher: Option<Child>,
    #[cfg(windows)]
    _job: Option<std::os::windows::io::OwnedHandle>,
}

#[cfg(unix)]
impl Drop for Lifeline {
    fn drop(&mut self) {
        drop(self.end.take());
        // The watcher ends Chrome and then itself, which can take a few
        // seconds; it's reaped when it does, without anyone waiting here.
        if let Some(mut watcher) = self.watcher.take()
            && !matches!(watcher.try_wait(), Ok(Some(_)))
        {
            let _ = std::thread::Builder::new()
                .name("netweir-chrome-watcher".into())
                .spawn(move || watcher.wait());
        }
    }
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
    // In a group of its own, which its helpers join: what the watcher
    // ends (see tie).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // Started suspended, so it's in its job before it can start a helper.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(
        &mut command,
        windows_sys::Win32::System::Threading::CREATE_SUSPENDED,
    );
    let mut child = {
        let _turn = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        hand_over(&mut command, &chrome_reads, &chrome_writes).map_err(failed)?;
        let child = command.spawn().map_err(failed)?;
        // Our copies of Chrome's ends must close, or we'd never see it exit.
        drop((chrome_reads, chrome_writes));
        child
    };
    let lifeline = tie(&child);
    #[cfg(windows)]
    if let Err(e) = resume(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(failed(e));
    }
    let said = child.stderr.take().map(Said::read).unwrap_or_default();
    Ok(Started {
        child,
        commands,
        replies,
        said,
        lifeline,
    })
}

/// What Chrome's watcher runs, with Chrome's process id as `$1`. It waits
/// for the lifeline to close, asks Chrome to quit as Ctrl+C would, gives
/// it up to ten seconds to write out its profile and go, then kills
/// whatever is left in the group, itself included. SIGINT, not SIGTERM:
/// Chrome writes out cookies it hasn't yet on SIGINT, and on macOS
/// doesn't on SIGTERM. Chrome's id can't have been given to anything
/// else meanwhile: it names the group, which the watcher keeps alive.
#[cfg(unix)]
const WATCH: &str = "exec >/dev/null 2>&1; read _; kill -INT \"$1\"; n=0; \
                     while [ $n -lt 10 ] && kill -0 \"$1\"; do sleep 1; n=$((n+1)); done; kill -9 0";

/// Ties Chrome to a [`Lifeline`]: a watcher, started into Chrome's
/// process group, a shell reading the far end of a pipe whose only
/// writer is netweir. Nothing is ever written, so the read ends only when
/// the writer closes, on drop or with the program, and then the shell
/// ends the group (see [`WATCH`]). Without a shell, Chrome runs untied,
/// with a warning.
#[cfg(unix)]
fn tie(chrome: &Child) -> Lifeline {
    use std::os::unix::process::CommandExt;

    let watcher = std::io::pipe().and_then(|(held, end)| {
        let watcher = Command::new("/bin/sh")
            .args(["-c", WATCH, "netweir-watch", &chrome.id().to_string()])
            .stdin(held)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(chrome.id() as i32)
            .spawn()?;
        Ok((end, watcher))
    });
    match watcher {
        Ok((end, watcher)) => Lifeline {
            end: Some(end),
            watcher: Some(watcher),
        },
        Err(e) => {
            untied(&format!("/bin/sh, to watch it, can't be started: {e}"));
            Lifeline {
                end: None,
                watcher: None,
            }
        }
    }
}

/// Says, once, that Chrome runs untied, and why.
fn untied(why: &str) {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        crate::warn(&format!(
            "Chrome isn't tied to the program that launched it ({why}); a program \
             that ends without closing the browser may leave Chrome running"
        ))
    });
}

/// Ties Chrome to a [`Lifeline`]: a job that ends every process in it
/// when its last handle closes, and netweir holds the only one. Chrome is
/// still suspended, so every helper it starts joins the job too. A Chrome
/// that can't be put in one runs untied, with a warning.
#[cfg(windows)]
fn tie(child: &Child) -> Lifeline {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    // SAFETY: the handle is checked, owned from here on, and the limits
    // passed are the size given.
    let job = unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            Err(std::io::Error::last_os_error())
        } else {
            let job = OwnedHandle::from_raw_handle(job as _);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.as_raw_handle() as _,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(job.as_raw_handle() as _, child.as_raw_handle() as _)
                    == 0
            {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(job)
            }
        }
    };
    let job = job
        .map_err(|e| untied(&format!("it couldn't be put in a job: {e}")))
        .ok();
    Lifeline { _job: job }
}

/// Lets a Chrome started suspended run.
#[cfg(windows)]
fn resume(child: &Child) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtResumeProcess(process: *mut std::ffi::c_void) -> i32;
    }
    // SAFETY: the handle is Chrome's, open while `child` is.
    match unsafe { NtResumeProcess(child.as_raw_handle()) } {
        0 => Ok(()),
        status => Err(std::io::Error::other(format!(
            "it couldn't be resumed (NTSTATUS {status:#x})"
        ))),
    }
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

/// A fresh profile folder netweir made in the temporary directory,
/// removed when this is dropped, and the lock that says it's in use: a
/// file beside the folder, named after it with `.lock`. On macOS and
/// Linux it's an `flock`, which the system lets go of with the process,
/// however it ends; on Windows, the file open sharing only its deletion,
/// which nothing else can open until the process lets go of it. A
/// filesystem with no `flock` gets no lock, and the profile is swept as
/// one from before there were locks.
pub(crate) struct TempProfile {
    dir: PathBuf,
    lock: Option<(PathBuf, std::fs::File)>,
}

impl TempProfile {
    /// Makes one, its lock taken first, so the folder never exists with
    /// it untaken.
    pub(crate) fn make() -> std::io::Result<TempProfile> {
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "netweir-chrome-{}-{}-{nanos}",
            std::process::id(),
            COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let lock = take_lock(&dir)?;
        let profile = TempProfile { dir, lock };
        std::fs::create_dir_all(&profile.dir)?;
        Ok(profile)
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for TempProfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        // The lock goes last, and its file before it closes, so no sweep
        // takes it meanwhile; Windows deletes it once it closes.
        if let Some((path, _file)) = &self.lock {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Takes the lock for the profile `dir`, which doesn't exist yet. It's
/// made under another name and held before it's given its own, so a
/// sweep never finds it untaken.
#[cfg(unix)]
fn take_lock(dir: &Path) -> std::io::Result<Option<(PathBuf, std::fs::File)>> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = lock_of(dir);
    let making = dir.with_extension("lock-new");
    let made = (|| {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&making)?;
        if !try_lock(&file) {
            let e = std::io::Error::last_os_error();
            // Some network and FUSE filesystems have no flock.
            return match e.raw_os_error() {
                Some(libc::ENOLCK | libc::EOPNOTSUPP | libc::ENOSYS) => Ok(None),
                _ => Err(e),
            };
        }
        std::fs::rename(&making, &path)?;
        Ok(Some((path, file)))
    })();
    if !matches!(made, Ok(Some(_))) {
        let _ = std::fs::remove_file(&making);
    }
    made
}

#[cfg(windows)]
fn take_lock(dir: &Path) -> std::io::Result<Option<(PathBuf, std::fs::File)>> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    let path = lock_of(dir);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_DELETE)
        .open(&path)?;
    Ok(Some((path, file)))
}

/// The lock file for the profile `dir`.
fn lock_of(dir: &Path) -> PathBuf {
    let mut name = dir.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Takes `file`'s lock if nothing holds it.
#[cfg(unix)]
fn try_lock(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a descriptor `file` owns.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// The lock file `path`, if nothing holds it: opened (never through a
/// link) and locked.
fn untaken(path: &Path) -> Option<std::fs::File> {
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()?;
        try_lock(&file).then_some(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Fails while the profile's program holds it: it shares nothing
        // but deletion.
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .ok()
    }
}

/// Runs [`sweep_profiles`] on `std::env::temp_dir()` on a thread of its
/// own, which doesn't hold up the launch, nor the program's end. One
/// sweep at a time: a launch while one runs has it go round once more,
/// for what it may have read too soon to see.
pub(crate) fn sweep_soon() {
    // Whether one runs, and whether it's to go round again.
    static SWEEPING: std::sync::Mutex<(bool, bool)> = std::sync::Mutex::new((false, false));
    let state = || SWEEPING.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut sweeping = state();
        if sweeping.0 {
            sweeping.1 = true;
            return;
        }
        *sweeping = (true, false);
    }
    let started = std::thread::Builder::new()
        .name("netweir-profile-sweep".into())
        .spawn(move || {
            loop {
                sweep_profiles(&std::env::temp_dir());
                let mut sweeping = state();
                if !std::mem::take(&mut sweeping.1) {
                    sweeping.0 = false;
                    return;
                }
            }
        });
    if started.is_err() {
        state().0 = false;
    }
}

/// Removes the profiles in `temp` that programs ended without removing,
/// as one killed or crashed can't. Only folders named as netweir names
/// its own (`netweir-chrome-<pid>-<count>-<nanos>`) are looked at, and
/// links are never followed, so a profile someone gave is never touched.
/// One whose lock can be taken is no longer in use. One with no lock is
/// from before there were any, and goes once the process that made it
/// has: the only test there is, which is wrong where processes from more
/// than one container share the folder, so a profile made since always
/// has a lock.
pub(crate) fn sweep_profiles(temp: &Path) {
    let Ok(entries) = std::fs::read_dir(temp) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = entry.path();
        if let Some(made) = name.strip_suffix(".lock").filter(|n| left_by(n).is_some()) {
            let dir = temp.join(made);
            let Some(lock) = untaken(&path) else { continue };
            if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::remove_dir_all(&dir);
            }
            if cfg!(windows) {
                drop(lock);
            }
            let _ = std::fs::remove_file(&path);
        } else if let Some(pid) = name.strip_suffix(".lock-new").and_then(left_by) {
            // A lock a program ended before naming. One being named right
            // now is in the moment between being made and being taken, so
            // its program must have gone too.
            if pid != std::process::id() && !running(pid) && untaken(&path).is_some() {
                let _ = std::fs::remove_file(&path);
            }
        } else if let Some(pid) = left_by(name) {
            let old = std::fs::symlink_metadata(lock_of(&path)).is_err()
                && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
            if old && pid != std::process::id() && !running(pid) {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

/// The process that made a profile named `name`, if netweir named it:
/// `netweir-chrome-<pid>-<count>-<nanos>`, all three numbers.
fn left_by(name: &str) -> Option<u32> {
    let parts: Vec<&str> = name.strip_prefix("netweir-chrome-")?.split('-').collect();
    let [pid, count, nanos] = parts[..] else {
        return None;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !(digits(count) && digits(nanos) && digits(pid)) {
        return None;
    }
    pid.parse().ok().filter(|&pid| pid > 0)
}

/// Whether process `pid` runs. One netweir isn't allowed to look at
/// counts as running.
#[cfg(unix)]
pub(crate) fn running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks; pid is positive, so it names one
    // process, never a group.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Whether process `pid` runs. One netweir isn't allowed to look at
/// counts as running.
#[cfg(windows)]
pub(crate) fn running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: the handle is checked and closed.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut code = 0;
        let ok = GetExitCodeProcess(process, &mut code) != 0;
        CloseHandle(process);
        !ok || code == STILL_ACTIVE as u32
    }
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
    fn features_join_those_already_asked_for() {
        let args = enable_features(
            vec![
                "--enable-features=A,B".into(),
                "--headless".into(),
                "--enable-features=C".into(),
            ],
            &["Ours"],
        );
        assert_eq!(args, ["--headless", "--enable-features=Ours,A,B,C"]);
        assert_eq!(
            enable_features(vec![], &["Ours"]),
            ["--enable-features=Ours"]
        );
        // A single dash is the same switch; after "--" nothing is one.
        assert_eq!(
            enable_features(strings(&["-enable-features=A", "--", "page"]), &["Ours"]),
            ["--enable-features=Ours,A", "--", "page"]
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("netweir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn prediction(file: &Path) -> serde_json::Value {
        let prefs: serde_json::Value =
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        prefs["net"]["network_prediction_options"].clone()
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn switches_are_read_as_chrome_reads_them() {
        let args = strings(&[
            "--profile-directory=A",
            "-profile-directory=B",
            " --other ",
            "--",
            "--profile-directory=after-the-end",
        ]);
        assert_eq!(
            last_switch(&args, "profile-directory").as_deref(),
            Some("B")
        );
        assert_eq!(last_switch(&args, "other").as_deref(), Some(""));
        assert_eq!(last_switch(&args, "missing"), None);
        let mut args = strings(&[
            "-user-data-dir=a",
            "x",
            "--user-data-dir=b=c",
            "--",
            "-user-data-dir=d",
        ]);
        assert_eq!(take_switches(&mut args, "user-data-dir"), ["a", "b=c"]);
        assert_eq!(args, ["x", "--", "-user-data-dir=d"]);
        // A slash, and capitals, count on Windows only, as in Chrome.
        let args = strings(&["/User-Data-Dir=w"]);
        assert_eq!(last_switch(&args, "user-data-dir").is_some(), cfg!(windows));
        let args = strings(&["--User-Data-Dir=w"]);
        assert_eq!(last_switch(&args, "user-data-dir").is_some(), cfg!(windows));
    }

    #[test]
    fn a_profile_name_must_be_a_folder_in_the_user_data_dir() {
        let dir = scratch("bad-profile");
        let mut bad = vec!["", ".", "..", "../outside", "a/b", "a\\b", "/abs"];
        if cfg!(windows) {
            bad.extend(["C:\\abs", "C:rel"]);
        }
        for name in bad {
            let args = [format!("--profile-directory={name}")];
            let err = profile_in(&dir, &args).unwrap_err().to_string();
            assert!(err.contains("profile"), "{name}: {err}");
            assert!(turn_off_preloading(&dir, &args).is_err(), "{name}");
            std::fs::write(
                dir.join("Local State"),
                serde_json::json!({"profile": {"last_used": name}}).to_string(),
            )
            .unwrap();
            // An empty name in Local State is Chrome's default.
            assert_eq!(profile_in(&dir, &[]).is_err(), !name.is_empty(), "{name}");
        }
        std::fs::remove_file(dir.join("Local State")).unwrap();
        // Nothing was written anywhere.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert!(!dir.parent().unwrap().join("outside").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_profile_is_the_one_chrome_opens() {
        let dir = scratch("which-profile");
        assert_eq!(profile_in(&dir, &[]).unwrap(), "Default");
        std::fs::write(
            dir.join("Local State"),
            r#"{"profile":{"last_used":"Profile 1"}}"#,
        )
        .unwrap();
        assert_eq!(profile_in(&dir, &[]).unwrap(), "Profile 1");
        let args = ["--profile-directory=Work".to_string()];
        assert_eq!(profile_in(&dir, &args).unwrap(), "Work");
        std::fs::write(dir.join("Local State"), "{not json").unwrap();
        assert_eq!(profile_in(&dir, &[]).unwrap(), "Default");

        std::fs::write(
            dir.join("Local State"),
            r#"{"profile":{"last_used":"Profile 1"}}"#,
        )
        .unwrap();
        turn_off_preloading(&dir, &[]).unwrap();
        assert_eq!(prediction(&dir.join("Profile 1/Preferences")), 2);
        assert!(!dir.join("Default").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_preferences_file_is_chromes_to_recover() {
        let dir = scratch("empty-prefs");
        std::fs::create_dir_all(dir.join("Default")).unwrap();
        let file = dir.join("Default/Preferences");
        for empty in ["", " \n"] {
            std::fs::write(&file, empty).unwrap();
            turn_off_preloading(&dir, &[]).unwrap();
            assert_eq!(prediction(&file), 2);
        }
        std::fs::write(&file, "{\"net\":").unwrap();
        let err = turn_off_preloading(&dir, &[]).unwrap_err().to_string();
        assert!(err.contains("isn't valid JSON"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_file_keeps_its_permissions_and_its_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("prefs-mode");
        let file = dir.join("Default/Preferences");
        // A new one is private, as Chrome makes it.
        turn_off_preloading(&dir, &[]).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        turn_off_preloading(&dir, &[]).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
        // A linked file is changed where it is, and stays linked.
        let real = dir.join("elsewhere.json");
        std::fs::write(&real, "{}").unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&real, &file).unwrap();
        turn_off_preloading(&dir, &[]).unwrap();
        assert!(
            std::fs::symlink_metadata(&file)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(prediction(&real), 2);
        assert!(!dir.join("Default/Preferences.netweir").exists());
        assert!(!dir.join("elsewhere.netweir").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn preloading_is_turned_off_and_the_rest_kept() {
        let dir = std::env::temp_dir().join(format!("netweir-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let read = |p: &str| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(dir.join(p).join("Preferences")).unwrap())
                .unwrap()
        };
        // A new profile.
        turn_off_preloading(&dir, &[]).unwrap();
        assert_eq!(read("Default")["net"]["network_prediction_options"], 2);
        // Someone's own, with the setting on and more besides.
        let own = dir.join("Work");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(
            own.join("Preferences"),
            r#"{"net":{"network_prediction_options":0,"other":1},"homepage":"x"}"#,
        )
        .unwrap();
        let args = ["--profile-directory=Work".to_string()];
        turn_off_preloading(&dir, &args).unwrap();
        let prefs = read("Work");
        assert_eq!(prefs["net"]["network_prediction_options"], 2);
        assert_eq!(
            (&prefs["net"]["other"], &prefs["homepage"]),
            (&1.into(), &"x".into())
        );
        // One that isn't Chrome's is left alone.
        std::fs::write(own.join("Preferences"), "[1]").unwrap();
        assert!(turn_off_preloading(&dir, &args).is_err());
        assert_eq!(std::fs::read(own.join("Preferences")).unwrap(), b"[1]");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_sandbox_grant_names_app_packages_by_sid() {
        let args: Vec<String> = sandbox_grant_args(Path::new("C:/n/chrome/154.0.1.2"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "C:/n/chrome/154.0.1.2",
                "/grant",
                "*S-1-15-2-1:(OI)(CI)(RX)",
                "/grant",
                "*S-1-15-2-2:(OI)(CI)(RX)",
                "/Q",
            ]
        );
    }

    #[test]
    fn only_chrome_netweir_installed_counts_as_its_own() {
        let home = Path::new("/h/.netweir");
        let exe = home.join("chrome/154.0.1.2/chrome-win64/chrome.exe");
        assert_eq!(
            install_dir_of(&exe, home),
            Some(home.join("chrome").join("154.0.1.2"))
        );
        assert_eq!(
            install_dir_of(Path::new("/Program Files/Google/Chrome/chrome.exe"), home),
            None
        );
        // Not one of netweir's versions, though under its folder.
        assert_eq!(install_dir_of(&home.join("chrome/chrome.exe"), home), None);
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
