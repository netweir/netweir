//! `netweir install chrome`: the Chrome for Testing build that matches
//! netweir's newest Chrome profile, unpacked where `find_chrome` looks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use netweir_browser::{cft_executable, cft_platform};
use serde_json::Value;
use sha2::Digest;

use crate::fetch::{FetchOptions, Fetcher, Response};
use crate::profile::{DEFAULT, Profile};

/// Google's list of the newest build of each Chrome milestone.
pub const CHROME_FOR_TESTING: &str = "https://googlechromelabs.github.io/chrome-for-testing/latest-versions-per-milestone-with-downloads.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub executable: PathBuf,
    /// It was there already; nothing was downloaded.
    pub already: bool,
}

/// Installs the Chrome for Testing build (or, with `headless_shell`,
/// `chrome-headless-shell`) of netweir's newest Chrome profile's
/// milestone, as `index` lists it, into `home/chrome/<version>/`. A build
/// already there isn't downloaded again.
pub async fn install_chrome(
    index: &str,
    home: &Path,
    headless_shell: bool,
) -> Result<Installed, String> {
    let platform = cft_platform().ok_or_else(|| {
        format!(
            "no Chrome for Testing builds for {} on {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let milestone = DEFAULT
        .split('-')
        .nth(1)
        .ok_or("netweir's default profile has no version in its name")?;
    let mut options = FetchOptions::new(Profile::named("chrome").map_err(|e| e.to_string())?);
    // A download is a few hundred megabytes.
    options.timeout = Duration::from_secs(600);
    let fetcher = Fetcher::new(options).map_err(|e| e.message)?;

    let listing = download(&fetcher, index).await?;
    let listing: Value =
        serde_json::from_slice(&listing.body).map_err(|e| format!("{index}: {e}"))?;
    let build = &listing["milestones"][milestone];
    let version = build["version"]
        .as_str()
        .ok_or_else(|| format!("{index} has no Chrome {milestone}"))?
        .to_string();
    // It becomes a folder name, and find_chrome reads only dotted numbers.
    if version.is_empty()
        || !version
            .split('.')
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!("{index} lists {version:?} as a version"));
    }
    let kind = if headless_shell {
        "chrome-headless-shell"
    } else {
        "chrome"
    };
    let downloads = build["downloads"][kind]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let url = downloads
        .iter()
        .find(|d| d["platform"] == platform)
        .and_then(|d| d["url"].as_str())
        .ok_or_else(|| {
            let there: Vec<&str> = downloads
                .iter()
                .filter_map(|d| d["platform"].as_str())
                .collect();
            format!(
                "no Chrome for Testing {kind} {version} for {platform}; there is for: {}",
                if there.is_empty() {
                    "nothing".to_string()
                } else {
                    there.join(", ")
                }
            )
        })?
        .to_string();

    let dir = home.join("chrome").join(&version);
    let executable = dir.join(cft_executable(platform, headless_shell));
    if executable.is_file() {
        // One installed before netweir granted the sandbox access gets it.
        netweir_browser::grant_sandbox_access(&dir)?;
        return Ok(Installed {
            version,
            executable,
            already: true,
        });
    }
    let zip = download(&fetcher, &url).await?;
    // Unpacked beside its final place and moved in whole, so a download
    // cut short never looks installed.
    let prefix = format!(".{version}-{kind}-");
    sweep_partials(&home.join("chrome"), &prefix);
    let partial = home
        .join("chrome")
        .join(format!("{prefix}{}", std::process::id()));
    let unpacked = tokio::task::spawn_blocking({
        let partial = partial.clone();
        move || -> Result<(), String> {
            std::fs::create_dir_all(&partial).map_err(|e| e.to_string())?;
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip.body))
                .map_err(|e| format!("not a zip: {e}"))?;
            archive.extract(&partial).map_err(|e| e.to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    let moved = unpacked.and_then(|()| move_into(&partial, &dir));
    let _ = std::fs::remove_dir_all(&partial);
    moved.map_err(|e| format!("unpacking {url} into {}: {e}", dir.display()))?;
    if !executable.is_file() {
        return Err(format!("{url} didn't hold {}", executable.display()));
    }
    netweir_browser::grant_sandbox_access(&dir)?;
    Ok(Installed {
        version,
        executable,
        already: false,
    })
}

/// Removes what an install that was killed left behind in `root`: folders
/// named with `prefix`. Not one under way now, so only those an hour old.
fn sweep_partials(root: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(3600));
        if old && entry.file_name().to_string_lossy().starts_with(prefix) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Moves what was unpacked into `partial` into `dir`, entry by entry.
fn move_into(partial: &Path, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(partial).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let to = dir.join(entry.file_name());
        // Ours is complete; one already there isn't, or the executable
        // check before the download would have found it.
        if to.exists() {
            std::fs::remove_dir_all(&to).map_err(|e| e.to_string())?;
        }
        std::fs::rename(entry.path(), &to).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Where Mozilla keeps every Firefox release, by version.
pub const FIREFOX_RELEASES: &str = "https://archive.mozilla.org/pub/firefox/releases";

/// A Firefox release Netweir installs, and the SHA-256 of its download
/// for each platform, as Mozilla's signed SHA256SUMS lists them.
#[derive(Debug, Clone, Copy)]
pub struct FirefoxRelease<'a> {
    pub version: &'a str,
    /// (platform as Mozilla names it, lowercase hex SHA-256).
    pub sums: &'a [(&'a str, &'a str)],
}

/// The Firefox Netweir installs. Moved on with each release, once the
/// Firefox tests pass on it; the sums come from that release's
/// SHA256SUMS, after checking its signature.
pub const FIREFOX: FirefoxRelease<'static> = FirefoxRelease {
    version: "161.0",
    sums: &[
        (
            "mac",
            "18f398ada9ba9faa5458dc01c2a68fc7df35517283be2e7a421a0867c5f47f83",
        ),
        (
            "linux-x86_64",
            "3e9b1e5132d7b53b1ad968c7300281e90e6a0ef57d0656b1516202d686d25733",
        ),
        (
            "linux-aarch64",
            "b3c2992292efc4a2f7ab6abe2de0ba6e15a482d41813e469c255a2528cacf10a",
        ),
        (
            "win64",
            "8368757b8e39ffe0441b2489e051d0d5ffc016c246ed5634495104da0e66fa83",
        ),
        (
            "win64-aarch64",
            "ea3faa9eee6961e26d7151d59cee15dc1b827e7076920180be37622b510f0a26",
        ),
    ],
};

/// Installs `release`'s US English build from `base` (laid out as
/// [`FIREFOX_RELEASES`]) into `home/firefox/<version>/`, then writes the
/// startup files that let Netweir drive it without the automation flag.
/// The download must match its pinned SHA-256 before anything is
/// unpacked. A build already there isn't downloaded again.
pub async fn install_firefox(
    base: &str,
    home: &Path,
    release: &FirefoxRelease<'_>,
) -> Result<Installed, String> {
    let version = release.version;
    let number = |n: &str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    // A release such as 159.0.1, or a beta such as 160.0b5, which the
    // weekly Firefox Beta check installs.
    let mut parts: Vec<&str> = version.split('.').collect();
    let last = parts.pop().unwrap_or_default();
    let last_ok = number(last)
        || last
            .split_once('b')
            .is_some_and(|(minor, beta)| number(minor) && number(beta));
    if !last_ok || !parts.iter().all(|n| number(n)) {
        return Err(format!("{version:?} isn't a Firefox release version"));
    }
    let platform = netweir_browser::firefox_platform().ok_or_else(|| {
        format!(
            "no Firefox builds for {} on {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let sum = release
        .sums
        .iter()
        .find(|(p, _)| *p == platform)
        .map(|(_, sum)| *sum)
        .ok_or_else(|| format!("Netweir has no checksum for Firefox {version} on {platform}"))?;
    let root = home.join("firefox");
    let dir = root.join(version);
    let executable = netweir_browser::firefox_executable(&dir);
    if executable.is_file() {
        // One from an older Netweir gets this one's startup files.
        netweir_browser::prepare_firefox(&executable)?;
        return Ok(Installed {
            version: version.to_string(),
            executable,
            already: true,
        });
    }
    let file = if platform == "mac" {
        format!("Firefox {version}.dmg")
    } else if platform.starts_with("win") {
        format!("Firefox Setup {version}.exe")
    } else {
        format!("firefox-{version}.tar.xz")
    };
    let url = format!(
        "{base}/{version}/{platform}/en-US/{}",
        file.replace(' ', "%20")
    );
    let mut options = FetchOptions::new(Profile::named("firefox").map_err(|e| e.to_string())?);
    // A download is over a hundred megabytes.
    options.timeout = Duration::from_secs(600);
    let fetcher = Fetcher::new(options).map_err(|e| e.message)?;
    let download = download(&fetcher, &url).await?;
    let got = hex(&sha2::Sha256::digest(&download.body));
    if got != sum {
        return Err(format!(
            "{url} has SHA-256 {got}, not the {sum} Mozilla published for it; nothing was installed"
        ));
    }

    let prefix = format!(".{version}-firefox-");
    sweep_partials(&root, &prefix);
    let partial = root.join(format!("{prefix}{}", std::process::id()));
    let unpacked = tokio::task::spawn_blocking({
        let partial = partial.clone();
        move || unpack_firefox(&download.body, &file, &partial)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    let moved = unpacked.and_then(|()| move_into(&partial, &dir));
    let _ = std::fs::remove_dir_all(&partial);
    moved.map_err(|e| format!("unpacking {url} into {}: {e}", dir.display()))?;
    if !executable.is_file() {
        return Err(format!("{url} didn't hold {}", executable.display()));
    }
    netweir_browser::prepare_firefox(&executable)?;
    Ok(Installed {
        version: version.to_string(),
        executable,
        already: false,
    })
}

/// How many times a download is tried before its error is given.
const ATTEMPTS: u32 = 4;

/// `url`'s 200 response, tried again after a connection that failed,
/// closed early, broke or timed out, or a 429 or 5xx answer, which a
/// mirror or CDN gives now and then. Waits 1, 2 and then 4 s between tries. Any other
/// failure, or the last one, is the error.
async fn download(fetcher: &Fetcher, url: &str) -> Result<Response, String> {
    download_waiting(fetcher, url, Duration::from_secs(1)).await
}

async fn download_waiting(
    fetcher: &Fetcher,
    url: &str,
    first_wait: Duration,
) -> Result<Response, String> {
    let mut wait = first_wait;
    for attempt in 1..=ATTEMPTS {
        let last = attempt == ATTEMPTS;
        match fetcher.get(url, &[]).await {
            Ok(r) if r.status == 200 => return Ok(r),
            Ok(r) if !last && (r.status == 429 || r.status >= 500) => {}
            Ok(r) => return Err(format!("{url}: HTTP {}", r.status)),
            // The errors a crawl tries again after.
            Err(e) if !last && crate::crawl::transient(&e) => {}
            Err(e) => return Err(format!("{url}: {}", e.message)),
        }
        tokio::time::sleep(wait).await;
        wait *= 2;
    }
    unreachable!("the last attempt returns")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Unpacks Mozilla's download, named `file`, into `partial`, with the
/// system's own tools: the disk image is mounted and its app copied out;
/// the tarball is untarred; the Windows installer, already checked, is
/// run only to extract itself, so nothing is installed into Program Files.
fn unpack_firefox(archive: &[u8], file: &str, partial: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};

    std::fs::create_dir_all(partial).map_err(|e| e.to_string())?;
    // Beside the folder, not in it, so only what's unpacked is moved in.
    // The folder's name has dots in it, so not with_extension.
    let beside = |suffix: &str| {
        let mut name = partial.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let downloaded = beside(match Path::new(file).extension().and_then(|e| e.to_str()) {
        Some("dmg") => ".dmg",
        Some("exe") => ".exe",
        _ => ".tar.xz",
    });
    std::fs::write(&downloaded, archive).map_err(|e| e.to_string())?;
    let run = |command: &mut Command| -> Result<(), String> {
        let out = command
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("can't run {:?}: {e}", command.get_program()))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{:?} failed ({}): {}",
                command.get_program(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    };
    let done = if file.ends_with(".dmg") {
        let mount = beside(".mount");
        run(Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet"])
            .arg("-mountpoint")
            .arg(&mount)
            .arg(&downloaded))
        .and_then(|()| {
            // ditto keeps the app's links and its signature's attributes.
            let copied = run(Command::new("ditto")
                .arg(mount.join("Firefox.app"))
                .arg(partial.join("Firefox.app")));
            let detached = run(Command::new("hdiutil")
                .args(["detach", "-quiet", "-force"])
                .arg(&mount));
            copied.and(detached)
        })
    } else if file.ends_with(".exe") {
        let mut extract = std::ffi::OsString::from("/ExtractDir=");
        extract.push(partial);
        run(Command::new(&downloaded).arg(extract))
    } else {
        run(Command::new("tar")
            .arg("-xJf")
            .arg(&downloaded)
            .arg("-C")
            .arg(partial))
    };
    let _ = std::fs::remove_file(&downloaded);
    done
}

#[cfg(test)]
mod download_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::download_waiting;
    use crate::fetch::{FetchOptions, Fetcher};
    use crate::profile::Profile;

    /// A server that answers its requests in turn from `answers`: a status
    /// to send, or 0 to close the connection without answering. The last
    /// answer repeats. Returns its URL and how many requests came.
    fn serve(answers: &'static [u16]) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/f",
            listener.local_addr().unwrap().port()
        );
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let n = seen.fetch_add(1, Ordering::SeqCst);
                let status = answers[n.min(answers.len() - 1)];
                if status == 0 {
                    continue;
                }
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                );
                let _ = stream.write_all(head.as_bytes());
            }
        });
        (url, count)
    }

    fn fetcher() -> Fetcher {
        Fetcher::new(FetchOptions::new(Profile::named("firefox").unwrap())).unwrap()
    }

    const NO_WAIT: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn a_busy_server_is_asked_again() {
        let (url, count) = serve(&[503, 429, 200]);
        let got = download_waiting(&fetcher(), &url, NO_WAIT).await.unwrap();
        assert_eq!(&got.body[..], b"ok");
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_dropped_connection_is_tried_again() {
        let (url, count) = serve(&[0, 200]);
        let got = download_waiting(&fetcher(), &url, NO_WAIT).await.unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_missing_file_is_asked_for_once() {
        let (url, count) = serve(&[404]);
        let err = download_waiting(&fetcher(), &url, NO_WAIT)
            .await
            .unwrap_err();
        assert!(err.ends_with("HTTP 404"), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_server_that_stays_down_is_given_up_on() {
        let (url, count) = serve(&[503]);
        let err = download_waiting(&fetcher(), &url, NO_WAIT)
            .await
            .unwrap_err();
        assert!(err.ends_with("HTTP 503"), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), super::ATTEMPTS as usize);
    }
}
