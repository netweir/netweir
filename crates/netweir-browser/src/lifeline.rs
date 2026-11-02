//! The lifeline: what ends a browser `launch` started, helpers and all,
//! once netweir lets go of it, however the program ends.

use std::io::PipeWriter;
use std::process::{Child, Command, Stdio};

use crate::launch::pipe;
#[cfg(windows)]
use crate::launch::{resume, system_dir};

/// Chrome runs while netweir holds this, and is ended, helpers and all,
/// once it's let go: when the browser is dropped, and when the program
/// ends, however it ends. Chrome is meant to quit when its end of the
/// DevTools pipe closes, but doesn't always: Google Chrome 155, headless,
/// closes its pages and then runs on with nothing to drive it, for good.
/// See [`tie`].
pub(crate) struct Lifeline {
    /// The pipe the watcher reads, written by nothing.
    end: Option<PipeWriter>,
    watcher: Option<Child>,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

impl Lifeline {
    /// Lets go once Chrome has exited, as after close(): the watcher has
    /// nothing to give time to, so on Windows, where it would wait ten
    /// seconds regardless, it's ended at once, before its read returns.
    /// Elsewhere it finds Chrome gone and ends at once by itself.
    ///
    /// On Windows, Chrome's helpers are ended with it, and waited for:
    /// one can outlive Chrome by half a minute on a busy machine, holding
    /// files in the profile that close() is about to remove, and Windows
    /// won't delete an open file.
    pub(crate) fn process_gone(self) {
        #[cfg(windows)]
        {
            let mut lifeline = self;
            if let Some(mut watcher) = lifeline.watcher.take() {
                let _ = watcher.kill();
                let _ = watcher.wait();
            }
            if let Some(job) = &lifeline.job {
                end_job(job);
            }
        }
    }
}

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
pub(crate) fn tie(chrome: &Child) -> Lifeline {
    use std::os::unix::process::CommandExt;

    let watcher = pipe().and_then(|(held, end)| {
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
/// when its last handle closes. Chrome is still suspended, so every helper
/// it starts joins the job too. A Chrome that can't be put in one runs
/// untied, with a warning. See [`watch`] for who holds the handles.
#[cfg(windows)]
pub(crate) fn tie(child: &Child) -> Lifeline {
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
    let job = match job {
        Ok(job) => job,
        Err(e) => {
            untied(&format!("it couldn't be put in a job: {e}"));
            return Lifeline {
                end: None,
                watcher: None,
                job: None,
            };
        }
    };
    // Without a watcher, netweir's is the only handle, and Chrome is
    // ended at once rather than given its time.
    let (end, watcher) = match watch(&job) {
        Ok((end, watcher)) => (Some(end), Some(watcher)),
        Err(_) => (None, None),
    };
    Lifeline {
        end,
        watcher,
        job: Some(job),
    }
}

/// Ends every process in `job` and waits for each to be gone, as a
/// process lets go of its files only once it has. Ending is quick to ask
/// for but not to finish: Chrome runs some helpers at idle priority, and
/// on a busy machine one told to end can wait over thirty seconds for the
/// time to do it. Each is put back at normal priority first, which brings
/// that to a few seconds, eleven at the slowest on a four-core runner
/// with four or eight busy loops; thirty is the bound. The helpers are in
/// a job of Chrome's own inside this one, which this job's own counts and
/// lists leave out, so they're found among all the system's processes.
/// Chrome itself has exited by now, so no new helper can start meanwhile.
#[cfg(windows)]
fn end_job(job: &std::os::windows::io::OwnedHandle) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    use windows_sys::Win32::System::Threading::{
        NORMAL_PRIORITY_CLASS, SetPriorityClass, WaitForSingleObject,
    };

    let processes = in_job(job);
    // SAFETY: each handle is open while `processes` is, and the job's
    // while `job` is borrowed.
    unsafe {
        for process in &processes {
            SetPriorityClass(process.as_raw_handle() as _, NORMAL_PRIORITY_CLASS);
        }
        if TerminateJobObject(job.as_raw_handle() as _, 1) == 0 {
            return;
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    for process in &processes {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        // SAFETY: as above.
        unsafe { WaitForSingleObject(process.as_raw_handle() as _, left.as_millis() as u32) };
    }
}

/// Every process in `job`, or in a job inside it, opened to wait on and
/// to set the priority of.
#[cfg(windows)]
fn in_job(job: &std::os::windows::io::OwnedHandle) -> Vec<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
        PROCESS_SYNCHRONIZE,
    };

    let mut found = Vec::new();
    // SAFETY: each handle is checked, and owned from here on; the entry
    // is the size it says.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return found;
        }
        let snapshot = OwnedHandle::from_raw_handle(snapshot as _);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot.as_raw_handle() as _, &mut entry) != 0;
        while more {
            let process = OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                entry.th32ProcessID,
            );
            if !process.is_null() {
                let process = OwnedHandle::from_raw_handle(process as _);
                let mut inside = 0;
                if IsProcessInJob(
                    process.as_raw_handle() as _,
                    job.as_raw_handle() as _,
                    &mut inside,
                ) != 0
                    && inside != 0
                {
                    found.push(process);
                }
            }
            more = Process32NextW(snapshot.as_raw_handle() as _, &mut entry) != 0;
        }
    }
    found
}

/// Starts Chrome's watcher, which holds a handle to its job too, so the
/// job outlives netweir by the time Chrome is given to quit: `cmd`,
/// reading a pipe whose only writer is netweir. Nothing is written, so
/// the read returns only when the writer closes, on drop or on the
/// program's death, which also closes Chrome's DevTools pipe, and Chrome
/// quits on that, writing out its profile as it goes. Ten seconds later
/// (`ping`'s wait: `timeout` wants a console), the watcher exits, the
/// job's last handle closes, and whatever is left is ended. `cmd` and
/// `ping` are on every Windows; the watcher runs with a console of its
/// own, hidden, so a Ctrl+C meant for the program doesn't end it early.
#[cfg(windows)]
fn watch(job: &std::os::windows::io::OwnedHandle) -> std::io::Result<(PipeWriter, Child)> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, GetCurrentProcess,
    };

    // Both from the system folder, by full path: neither a search from
    // the program's own folder nor an environment that may be empty.
    let system = system_dir()?;
    let ping = system.join("PING.EXE");
    let (held, end) = pipe()?;
    let mut command = Command::new(system.join("cmd.exe"));
    command
        .raw_arg(format!(
            r#"/d /q /s /c "set /p _= & "{}" -n 11 127.0.0.1 >nul""#,
            ping.display()
        ))
        .stdin(held)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut watcher = command.spawn()?;
    // The job handle is copied into the watcher, never made inheritable,
    // so no other process started meanwhile can come by one. Holding it
    // is all the watcher does with it.
    let mut copy = std::ptr::null_mut();
    // SAFETY: both process handles and the job's are open; the copy
    // belongs to the watcher and closes with it.
    let copied = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            job.as_raw_handle() as _,
            watcher.as_raw_handle() as _,
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } != 0;
    let started = if copied {
        resume(&watcher)
    } else {
        Err(std::io::Error::last_os_error())
    };
    if let Err(e) = started {
        let _ = watcher.kill();
        let _ = watcher.wait();
        return Err(e);
    }
    Ok((end, watcher))
}
