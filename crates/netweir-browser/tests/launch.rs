mod common;

use common::chrome;
use netweir_browser::{Browser, LaunchOptions};

#[tokio::test]
async fn closing_the_browser_ends_chrome_and_removes_its_profile() {
    let Some(executable) = chrome() else { return };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        ..LaunchOptions::default()
    })
    .await;
    let profile = browser.profile_dir().unwrap().to_path_buf();
    assert!(profile.exists());
    assert!(
        browser.version().starts_with("Chrome/"),
        "{}",
        browser.version()
    );
    browser.close().await.unwrap();
    assert!(!profile.exists(), "{} left behind", profile.display());
    assert!(browser.is_closed());
}

#[tokio::test]
async fn a_missing_executable_says_where_it_looked() {
    let err = Browser::launch(LaunchOptions {
        executable: Some("/nonexistent/chrome".into()),
        ..LaunchOptions::default()
    })
    .await
    .err()
    .unwrap();
    assert!(err.to_string().contains("/nonexistent/chrome"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_browser_that_dies_reads_as_closed() {
    let Some(executable) = chrome() else { return };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        ..LaunchOptions::default()
    })
    .await;
    let pid = browser.pid().unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !browser.is_closed() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(browser.is_closed());
    assert!(browser.new_page().await.is_err());
    browser.close().await.unwrap();
    assert!(!browser.profile_dir().unwrap().exists());
}

/// Ends a process outright.
fn kill(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output()
        .unwrap();
}

/// Each of several Chromes launched at once is seen to die when it does,
/// while the others still run: no process one launch starts may come by
/// the end of the pipe another's Chrome writes to.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chromes_launched_at_once_are_each_seen_to_die() {
    let Some(executable) = chrome() else { return };
    // Every turn there is: these Chromes are this test's alone.
    let _turns = common::all_turns();
    for _round in 0..3 {
        let launches: Vec<_> = (0..4)
            .map(|_| {
                let executable = executable.clone();
                tokio::spawn(Browser::launch(LaunchOptions {
                    executable: Some(executable),
                    ..LaunchOptions::default()
                }))
            })
            .collect();
        let mut browsers = Vec::new();
        for launch in launches {
            browsers.push(launch.await.unwrap().unwrap());
        }
        for (n, browser) in browsers.iter().enumerate() {
            kill(browser.pid().unwrap());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !browser.is_closed() && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(browser.is_closed(), "browser {n}'s Chrome died unseen");
        }
        for browser in browsers {
            browser.close().await.unwrap();
        }
    }
}

/// A Chrome is seen to die even when the program started other processes
/// of its own, outside netweir, while Chrome was launching: on Windows any
/// of them could inherit Chrome's end of its pipe and hold it open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chrome_is_seen_to_die_while_other_processes_hold_its_pipe() {
    let Some(executable) = chrome() else { return };
    let _turns = common::all_turns();
    // A process lasting well past the five seconds below, started over and
    // over by another thread while each Chrome launches.
    fn lasting() -> std::process::Command {
        let mut command = if cfg!(windows) {
            let mut c = std::process::Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]);
            c
        } else {
            let mut c = std::process::Command::new("sleep");
            c.arg("30");
            c
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }
    for _round in 0..3 {
        let launching = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let starter = std::thread::spawn({
            let launching = launching.clone();
            move || {
                let mut started = Vec::new();
                while launching.load(std::sync::atomic::Ordering::Relaxed) && started.len() < 300 {
                    started.push(lasting().spawn().unwrap());
                }
                started
            }
        });
        let browser = Browser::launch(LaunchOptions {
            executable: Some(executable.clone()),
            ..LaunchOptions::default()
        })
        .await;
        launching.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut started = starter.join().unwrap();
        let browser = browser.unwrap();
        kill(browser.pid().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !browser.is_closed() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let seen = browser.is_closed();
        for child in &mut started {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(
            seen,
            "Chrome died unseen ({} other processes)",
            started.len()
        );
        browser.close().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_chrome_that_wont_start_says_why() {
    use std::os::unix::fs::PermissionsExt;
    // Stands in for a Chrome that can't start, and says so on stderr, as a
    // Chrome without a usable sandbox does.
    let dir = std::env::temp_dir().join(format!("netweir-nochrome-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("chrome");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho 'starting up' >&2\necho 'FATAL: No usable sandbox!' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = Browser::launch(LaunchOptions {
        executable: Some(fake),
        ..LaunchOptions::default()
    })
    .await
    .err()
    .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(err.to_string().contains("No usable sandbox"), "{err}");
}

/// The SIDs an ACL grants access to, as PowerShell reads them: the names
/// icacls prints are translated. Asked for as SIDs, not names turned into
/// SIDs: "ALL APPLICATION PACKAGES" doesn't translate back, and the rule
/// would quietly drop out.
#[cfg(windows)]
fn granted_sids(dir: &std::path::Path) -> String {
    let script = format!(
        "(Get-Acl -LiteralPath '{}').GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier]) | ForEach-Object {{ $_.IdentityReference.Value }}",
        dir.display()
    );
    // Started from PowerShell 7 (as CI's steps are), Windows PowerShell
    // inherits 7's module path and can't load its own Get-Acl.
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", script.as_str()])
        .env_remove("PSModulePath")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg(windows)]
#[tokio::test]
async fn chrome_netweir_installed_lets_its_sandbox_in() {
    let Some(executable) = chrome() else { return };
    let home = netweir_browser::netweir_home().join("chrome");
    let Ok(rest) = executable.strip_prefix(&home) else {
        assert!(
            std::env::var_os("NETWEIR_REQUIRE_CHROME").is_none(),
            "CI runs `netweir install chrome` first, but {} isn't one",
            executable.display()
        );
        eprintln!(
            "skipped: {} wasn't installed by netweir",
            executable.display()
        );
        return;
    };
    let dir = home.join(rest.components().next().unwrap());
    // Launching grants it, if installing hadn't.
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        ..LaunchOptions::default()
    })
    .await;
    browser.close().await.unwrap();
    assert!(dir.join(".netweir-sandbox-access").is_file());
    let sids = granted_sids(&dir);
    for sid in ["S-1-15-2-1", "S-1-15-2-2"] {
        assert!(sids.lines().any(|l| l.trim() == sid), "{sid} not in {sids}");
    }
}

/// A chrome-headless-shell that `netweir install chrome --headless-shell`
/// put in place, if there is one: the tests that need it skip without.
fn headless_shell() -> Option<std::path::PathBuf> {
    let platform = netweir_browser::cft_platform()?;
    let mut found: Vec<_> = std::fs::read_dir(netweir_browser::netweir_home().join("chrome"))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| {
            e.path()
                .join(netweir_browser::cft_executable(platform, true))
        })
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    let shell = found.pop();
    if shell.is_none() {
        eprintln!("skipped: no chrome-headless-shell installed");
    }
    shell
}

#[tokio::test]
async fn chrome_headless_shell_launches_and_says_what_it_is() {
    let Some(executable) = headless_shell() else {
        return;
    };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        timeout: std::time::Duration::from_secs(30),
        ..LaunchOptions::default()
    })
    .await;
    let page = browser.new_page().await.unwrap();
    let agent = page.evaluate("navigator.userAgent").await.unwrap();
    assert!(
        agent.as_str().unwrap().contains(" Chrome/")
            && !agent.as_str().unwrap().contains("Headless"),
        "{agent}"
    );
    browser.close().await.unwrap();
}

/// An empty profile folder is no folder: Chrome reads `--user-data-dir=`
/// as unset and opens its own default profile, which netweir never
/// prepared. Refused, in every form, before anything is written.
#[tokio::test]
async fn an_empty_profile_folder_is_refused() {
    let cases = [
        (vec!["--user-data-dir=".to_string()], None),
        (vec!["-user-data-dir=  ".to_string()], None),
        (vec![], Some(std::path::PathBuf::from(""))),
        (vec![], Some(std::path::PathBuf::from(" "))),
    ];
    for (args, user_data_dir) in cases {
        let err = Browser::launch(LaunchOptions {
            executable: Some("/nonexistent/chrome".into()),
            args: args.clone(),
            user_data_dir: user_data_dir.clone(),
            ..LaunchOptions::default()
        })
        .await
        .err()
        .unwrap();
        assert!(
            err.to_string().contains("empty"),
            "{args:?} {user_data_dir:?}: {err}"
        );
        assert!(
            !std::path::Path::new("Default").exists(),
            "written to the working directory"
        );
    }
}

/// Whether anything of the Chrome started as `pid` with `profile` still
/// runs: a process with the profile on its command line (Chrome or a
/// helper; the path is unique to this test), or one left in its process
/// group (the watcher too).
#[cfg(unix)]
fn running_with(profile: &std::path::Path, pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-axo", "command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&*profile.to_string_lossy())
        || unsafe { libc::kill(-(pid as libc::pid_t), 0) } == 0
}

/// Whether Chrome, or any of its helpers, still runs: a process other
/// than the one asking with `profile` on its command line.
#[cfg(windows)]
fn running_with(profile: &std::path::Path, _pid: u32) -> bool {
    let query = format!(
        "@(Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -ne $PID -and \
         $_.CommandLine -and $_.CommandLine.Contains('{}') }}).Count",
        profile.display()
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &query])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim() != "0"
}

/// A program for the tests below, run as a process of its own: this test
/// binary, with `env`.
struct Controller(std::process::Child);

impl Controller {
    fn start(
        temp: &std::path::Path,
        env: &[(&str, &str)],
    ) -> (Controller, std::sync::mpsc::Receiver<String>) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "launches_chrome_for_the_orphan_test",
                "--nocapture",
            ])
            .stdout(std::process::Stdio::piped());
        // A temporary folder of its own, which no other launch sweeps.
        for var in ["TMPDIR", "TMP", "TEMP"] {
            command.env(var, temp);
        }
        command.envs(env.iter().copied());
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(|l| l.ok())
            {
                if let Some(said) = line.strip_prefix("controller: ") {
                    let _ = tx.send(said.to_string());
                }
            }
        });
        (Controller(child), rx)
    }
}

/// Killed whatever happens, so a failed test doesn't leave it running.
impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Not a test of its own: run by the tests below as a process of its own.
/// Launches Chrome (with `NETWEIR_ORPHAN_PROFILE` as its profile, if
/// set), opens `NETWEIR_ORPHAN_URL` or a blank page, says which Chrome,
/// and waits to be killed. With `NETWEIR_ORPHAN_CONTROLLER=close`, it
/// closes it instead, and says once `NETWEIR_ORPHAN_GONE` is gone.
#[tokio::test]
async fn launches_chrome_for_the_orphan_test() {
    let Some(mode) = std::env::var_os("NETWEIR_ORPHAN_CONTROLLER") else {
        return;
    };
    if mode == "fail" {
        // Fails after Chrome has been asked what it is, and before.
        let failed = Browser::launch(LaunchOptions {
            executable: chrome(),
            proxy: Some("not a proxy".into()),
            ..LaunchOptions::default()
        })
        .await;
        assert!(failed.is_err());
        #[cfg(unix)]
        {
            let failed = Browser::launch(LaunchOptions {
                executable: Some("/usr/bin/false".into()),
                ..LaunchOptions::default()
            })
            .await;
            assert!(failed.is_err());
        }
        println!("controller: failed");
        return;
    }
    let browser = Browser::launch(LaunchOptions {
        executable: chrome(),
        user_data_dir: std::env::var_os("NETWEIR_ORPHAN_PROFILE").map(Into::into),
        ..LaunchOptions::default()
    })
    .await
    .unwrap();
    if mode == "close" {
        browser.close().await.unwrap();
        let gone = std::path::PathBuf::from(std::env::var_os("NETWEIR_ORPHAN_GONE").unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while gone.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        println!(
            "controller: {}",
            if gone.exists() { "kept" } else { "gone" }
        );
        return;
    }
    // Google Chrome 155 outlived its pipe with a page open, not without.
    let page = browser.new_page().await.unwrap();
    let url = std::env::var("NETWEIR_ORPHAN_URL").unwrap_or_else(|_| "about:blank".into());
    page.goto(&url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    println!(
        "controller: {} {}",
        browser.pid().unwrap(),
        browser.profile_dir().unwrap().display()
    );
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(600));
}

/// A folder of the test's own, emptied first.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("netweir-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Kills `controller` outright, with no chance to close its browser, and
/// waits for everything of its Chrome to go. Returns Chrome's profile.
fn kill_and_wait(
    mut controller: Controller,
    said: &std::sync::mpsc::Receiver<String>,
) -> std::path::PathBuf {
    let said = said
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the controller never launched Chrome");
    let (pid, profile) = said.split_once(' ').unwrap();
    let (pid, profile): (u32, std::path::PathBuf) = (pid.parse().unwrap(), profile.into());
    assert!(running_with(&profile, pid));
    controller.0.kill().unwrap();
    controller.0.wait().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while running_with(&profile, pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "Chrome outlived the program that launched it"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    profile
}

/// A program killed outright takes Chrome with it: Chrome, and its
/// helpers, are gone within seconds, and the profile it left is removed
/// at the next launch.
#[tokio::test]
async fn chrome_goes_when_the_program_that_launched_it_is_killed() {
    if chrome().is_none() {
        return;
    }
    let _turn = common::turn();
    let temp = scratch("orphan-temp");
    let (controller, said) = Controller::start(&temp, &[("NETWEIR_ORPHAN_CONTROLLER", "1")]);
    let profile = kill_and_wait(controller, &said);
    // Nothing was left to remove the profile; the next launch does.
    assert!(profile.exists());
    let gone = profile.to_string_lossy().into_owned();
    let (next, said) = Controller::start(
        &temp,
        &[
            ("NETWEIR_ORPHAN_CONTROLLER", "close"),
            ("NETWEIR_ORPHAN_GONE", &gone),
        ],
    );
    assert_eq!(
        said.recv_timeout(std::time::Duration::from_secs(60))
            .as_deref(),
        Ok("gone"),
        "{} was left behind",
        profile.display()
    );
    drop(next);
    std::fs::remove_dir_all(&temp).unwrap();
}

/// A launch that fails leaves no profile behind.
#[tokio::test]
async fn a_launch_that_fails_leaves_no_profile() {
    if chrome().is_none() {
        return;
    }
    let _turn = common::turn();
    let temp = scratch("failed-launch");
    let (mut controller, said) = Controller::start(&temp, &[("NETWEIR_ORPHAN_CONTROLLER", "fail")]);
    assert_eq!(
        said.recv_timeout(std::time::Duration::from_secs(60))
            .as_deref(),
        Ok("failed")
    );
    controller.0.wait().unwrap();
    let left: Vec<_> = std::fs::read_dir(&temp)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().starts_with("netweir-chrome-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    std::fs::remove_dir_all(&temp).unwrap();
}

/// A server that sets a cookie that lasts an hour.
fn cookie_server() -> common::Server {
    common::serve(vec![(
        "/",
        common::Reply {
            status: 200,
            headers: vec![
                ("Content-Type", "text/html".into()),
                ("Set-Cookie", "kept=yes; Path=/; Max-Age=3600".into()),
            ],
            body: "<p>hi</p>".into(),
        },
    )])
}

/// Launches Chrome with the profile `dir` and checks it has the cookie
/// [`cookie_server`] sets; then removes `temp`.
async fn has_the_cookie(dir: std::path::PathBuf, temp: std::path::PathBuf) {
    let browser = common::launch(LaunchOptions {
        executable: chrome(),
        user_data_dir: Some(dir),
        ..LaunchOptions::default()
    })
    .await;
    let page = browser.new_page().await.unwrap();
    let cookies = page.cookies().await.unwrap();
    assert!(
        cookies.iter().any(|c| c.name == "kept" && c.value == "yes"),
        "{cookies:?}"
    );
    browser.close().await.unwrap();
    drop(browser);
    // Chrome's helpers can hold files a moment after it exits.
    for _ in 0..100 {
        if std::fs::remove_dir_all(&temp).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Dropping a browser with a profile to keep, without closing it, asks
/// Chrome to quit, so what it hadn't written out, such as a cookie just
/// set, is kept; and a launch with the profile straight after waits for
/// it to have quit, rather than finding it there.
#[tokio::test]
async fn a_kept_profile_keeps_its_cookies_when_the_browser_is_dropped() {
    if chrome().is_none() {
        return;
    }
    let server = cookie_server();
    let temp = scratch("dropped-profile");
    let first = common::launch(LaunchOptions {
        executable: chrome(),
        user_data_dir: Some(temp.join("profile")),
        ..LaunchOptions::default()
    })
    .await;
    let pid = first.pid().unwrap();
    let page = first.new_page().await.unwrap();
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    drop(page);
    drop(first);
    // Holds the old Chrome up for a moment, as a busy machine can, so the
    // launch below starts while it's still quitting.
    #[cfg(unix)]
    {
        let pid = pid as libc::pid_t;
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            unsafe { libc::kill(pid, libc::SIGCONT) };
        });
    }
    let _ = pid;
    has_the_cookie(temp.join("profile"), temp).await;
}

/// The same when the browser outlives the runtime it was launched on, as
/// one dropped as a program ends can.
#[test]
fn a_kept_profile_keeps_its_cookies_when_dropped_after_its_runtime() {
    if chrome().is_none() {
        return;
    }
    let server = cookie_server();
    let temp = scratch("dropped-late-profile");
    let runtime = || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let first = runtime().block_on(async {
        let first = common::launch(LaunchOptions {
            executable: chrome(),
            user_data_dir: Some(temp.join("profile")),
            ..LaunchOptions::default()
        })
        .await;
        let page = first.new_page().await.unwrap();
        page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
            .await
            .unwrap();
        first
    });
    // The runtime is gone by now.
    drop(first);
    runtime().block_on(has_the_cookie(temp.join("profile"), temp));
}

/// Chrome is asked to end before it's made to, so a profile to keep has
/// what Chrome hadn't yet written out, such as a cookie just set.
#[tokio::test]
async fn a_kept_profile_keeps_its_cookies_when_the_program_is_killed() {
    let Some(executable) = chrome() else { return };
    let server = common::serve(vec![(
        "/",
        common::Reply {
            status: 200,
            headers: vec![
                ("Content-Type", "text/html".into()),
                ("Set-Cookie", "kept=yes; Path=/; Max-Age=3600".into()),
            ],
            body: "<p>hi</p>".into(),
        },
    )]);
    let temp = scratch("killed-profile");
    let dir = temp.join("profile");
    let turn = common::turn();
    let (controller, said) = Controller::start(
        &temp,
        &[
            ("NETWEIR_ORPHAN_CONTROLLER", "1"),
            ("NETWEIR_ORPHAN_PROFILE", &dir.to_string_lossy()),
            ("NETWEIR_ORPHAN_URL", &server.url),
        ],
    );
    kill_and_wait(controller, &said);
    drop(turn);
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        user_data_dir: Some(dir.clone()),
        ..LaunchOptions::default()
    })
    .await;
    let page = browser.new_page().await.unwrap();
    let cookies = page.cookies().await.unwrap();
    assert!(
        cookies.iter().any(|c| c.name == "kept" && c.value == "yes"),
        "{cookies:?}"
    );
    browser.close().await.unwrap();
    drop(browser);
    // Chrome's helpers can hold files a moment after it exits.
    for _ in 0..100 {
        if std::fs::remove_dir_all(&temp).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A profile whose lock nothing holds is removed at the next launch, and
/// one from before there were locks, once its program has gone. One in
/// use is left alone, and so is anything not named as netweir names its
/// own, and anything reached through a link.
#[tokio::test]
async fn profiles_left_behind_are_swept_at_the_next_launch() {
    let Some(executable) = chrome() else { return };
    // A program that has certainly gone.
    let mut gone = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    gone.wait().unwrap();
    let temp = std::env::temp_dir();
    let me = std::process::id();
    let name = |n: &str| temp.join(format!("netweir-chrome-{n}"));
    // Left with its lock, by a program that may still run elsewhere.
    let unlocked = name(&format!("{me}-90-1"));
    // One from before locks: its program has gone, or hasn't.
    let old = name(&format!("{}-0-1", gone.id()));
    let old_live = name(&format!("{me}-91-1"));
    let other = name(&format!("{}-mine", gone.id()));
    let mut dirs = vec![&unlocked, &old, &old_live, &other];
    for dir in &dirs {
        std::fs::create_dir_all(dir.join("Default")).unwrap();
        std::fs::write(dir.join("Default").join("Preferences"), "{}").unwrap();
    }
    std::fs::write(name(&format!("{me}-90-1.lock")), "").unwrap();
    // A lock its program never named, and one being named now.
    let unnamed = name(&format!("{}-93-1.lock-new", gone.id()));
    let naming = name(&format!("{me}-94-1.lock-new"));
    std::fs::write(&unnamed, "").unwrap();
    std::fs::write(&naming, "").unwrap();
    // A link with a free lock, to a folder that isn't netweir's.
    #[cfg(unix)]
    let (target, link) = {
        let target = scratch("sweep-target");
        let link = name(&format!("{me}-92-1"));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        std::fs::write(name(&format!("{me}-92-1.lock")), "").unwrap();
        (target, link)
    };
    let browser = common::launch(LaunchOptions {
        executable: Some(executable),
        ..LaunchOptions::default()
    })
    .await;
    // The browser's own profile, in use, has a lock held.
    let own = browser.profile_dir().unwrap().to_path_buf();
    // The sweep runs on the side.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while unlocked.exists() || old.exists() || unnamed.exists() {
        assert!(std::time::Instant::now() < deadline, "not swept");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(own.exists() && old_live.exists() && other.exists() && naming.exists());
    std::fs::remove_file(&naming).unwrap();
    #[cfg(unix)]
    {
        assert!(target.exists());
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
    }
    browser.close().await.unwrap();
    assert!(!own.exists());
    dirs.retain(|d| d.exists());
    for dir in dirs {
        std::fs::remove_dir_all(dir).unwrap();
    }
}
