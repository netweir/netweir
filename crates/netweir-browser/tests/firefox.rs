//! Launching Firefox: the stealth start of a Firefox `netweir install
//! firefox` installed, automation mode when asked for, and the failures in
//! between.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use netweir_browser::{Browser, Engine, Error, LaunchOptions};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("netweir-fft-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn firefox_options() -> LaunchOptions {
    LaunchOptions {
        engine: Engine::Firefox,
        timeout: Duration::from_secs(60),
        ..LaunchOptions::default()
    }
}

/// Whether anything of the browser started as `pid` still runs: its
/// process group on macOS and Linux (helpers and watcher too), the
/// process itself on Windows, whose job ends the helpers with it.
fn running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(-(pid as libc::pid_t), 0) == 0 }
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }
}

fn gone_within(pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while running(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

#[tokio::test]
async fn firefox_launches_and_says_what_it_is() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let browser = common::launch_firefox(executable, LaunchOptions::default()).await;
    let info = browser.engine_info();
    assert_eq!(info.engine, Engine::Firefox);
    assert!(info.stealth);
    let major: u32 = info.version.split('.').next().unwrap().parse().unwrap();
    assert!(major >= 159, "{info:?}");
    assert_eq!(browser.version(), format!("Firefox/{}", info.version));
    let pid = browser.pid().unwrap();
    let profile = browser.profile_dir().unwrap().to_path_buf();
    browser.close().await.unwrap();
    assert!(gone_within(pid, Duration::from_secs(15)));
    assert!(!profile.exists(), "a profile Netweir made goes with it");
}

/// A Firefox that exits at once fails the launch at once, saying what it
/// wrote, rather than at the timeout.
#[cfg(unix)]
#[tokio::test]
async fn a_firefox_that_exits_fails_the_launch_at_once() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("exits");
    let fake = dir.join("firefox");
    std::fs::write(&fake, "#!/bin/sh\necho boom >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let started = Instant::now();
    let err = Browser::launch(LaunchOptions {
        executable: Some(fake),
        stealth: false,
        timeout: Duration::from_secs(30),
        ..firefox_options()
    })
    .await
    .err()
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let Error::Launch(message) = &err else {
        panic!("{err:?}")
    };
    assert!(message.contains("exited"), "{message}");
    assert!(message.contains("boom"), "{message}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_firefox_netweir_didnt_install_needs_stealth_false() {
    let dir = scratch("theirs-exe");
    let theirs = dir.join(if cfg!(windows) {
        "firefox.exe"
    } else {
        "firefox"
    });
    std::fs::write(&theirs, "").unwrap();
    let err = Browser::launch(LaunchOptions {
        executable: Some(theirs.clone()),
        ..firefox_options()
    })
    .await
    .err()
    .unwrap();
    let message = err.to_string();
    assert!(message.contains("netweir install firefox"), "{message}");
    assert!(message.contains("stealth: false"), "{message}");
    // Nothing was written beside it.
    assert!(!dir.join("mozilla.cfg").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn automation_mode_is_only_by_asking() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let browser = common::launch_firefox(
        executable,
        LaunchOptions {
            stealth: false,
            ..LaunchOptions::default()
        },
    )
    .await;
    assert!(!browser.engine_info().stealth);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_profile_netweir_didnt_make_is_refused() {
    let dir = scratch("theirs-profile");
    std::fs::write(dir.join("prefs.js"), "// a person's\n").unwrap();
    for stealth in [true, false] {
        let err = Browser::launch(LaunchOptions {
            user_data_dir: Some(dir.clone()),
            stealth,
            executable: common::firefox(),
            ..firefox_options()
        })
        .await
        .err()
        .unwrap();
        assert!(err.to_string().contains("didn't make"), "{err}");
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("prefs.js")).unwrap(),
        "// a person's\n"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_stale_port_file_is_not_read() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let dir = scratch("stale");
    let first = common::launch_firefox(
        executable.clone(),
        LaunchOptions {
            user_data_dir: Some(dir.clone()),
            ..LaunchOptions::default()
        },
    )
    .await;
    first.close().await.unwrap();
    drop(first);
    // A port nothing listens on, as a crashed launch would leave it.
    std::fs::write(dir.join("netweir-port"), "1").unwrap();
    std::fs::write(
        dir.join("netweir-port.failed"),
        "NETWEIR-FIREFOX failed: old",
    )
    .unwrap();
    let second = common::launch_firefox(
        executable,
        LaunchOptions {
            user_data_dir: Some(dir.clone()),
            ..LaunchOptions::default()
        },
    )
    .await;
    assert!(second.engine_info().stealth);
    second.close().await.unwrap();
    assert!(dir.exists(), "a kept profile stays");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_firefoxes_run_at_once() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let _turn = common::turns_for(2);
    let launch = || {
        Browser::launch(LaunchOptions {
            executable: Some(executable.clone()),
            ..firefox_options()
        })
    };
    let (a, b) = tokio::join!(launch(), launch());
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.pid(), b.pid());
    for browser in [&a, &b] {
        assert!(browser.engine_info().stealth);
        browser.close().await.unwrap();
    }
}

#[test]
fn a_dropped_firefox_ends_its_processes() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let _turn = common::turn();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let browser = runtime
        .block_on(Browser::launch(LaunchOptions {
            executable: Some(executable),
            ..firefox_options()
        }))
        .unwrap();
    let pid = browser.pid().unwrap();
    drop(browser);
    drop(runtime);
    // The watcher asks Firefox to quit and gives it ten seconds.
    assert!(
        gone_within(pid, Duration::from_secs(20)),
        "Firefox {pid} still runs"
    );
}
