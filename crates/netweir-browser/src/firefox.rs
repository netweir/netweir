//! Finding a Firefox `netweir install firefox` installed, and preparing it
//! to start without the automation flag (see firefox/mozilla.cfg).

use std::path::{Path, PathBuf};

use crate::launch::netweir_home;
use crate::{Error, Result};

/// Firefox reads it from its `defaults/pref` folder and is told by it to
/// read `mozilla.cfg`, unsandboxed.
const AUTOCONFIG: &str = include_str!("firefox/autoconfig.js");

/// What a Netweir Firefox runs at start. Its first line, which Firefox
/// skips, marks the install as Netweir's.
const MOZILLA_CFG: &str = include_str!("firefox/mozilla.cfg");

const MARK: &str = "// netweir";

/// This machine as Mozilla's release archive names platforms, if it has
/// builds for it.
pub fn firefox_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => Some("mac"),
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("windows", "x86_64") => Some("win64"),
        ("windows", "aarch64") => Some("win64-aarch64"),
        _ => None,
    }
}

/// Where the executable is in an install folder, as Mozilla's archive
/// unpacks: the app from the disk image on macOS, the tarball's `firefox`
/// folder on Linux, and what the installer's `/ExtractDir=` leaves on
/// Windows.
pub fn firefox_executable(dir: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        dir.join("Firefox.app/Contents/MacOS/firefox")
    } else if cfg!(windows) {
        dir.join("core/firefox.exe")
    } else {
        dir.join("firefox/firefox")
    }
}

/// The folder Firefox reads `mozilla.cfg` and `defaults/pref` from: the
/// executable's own, or on macOS the app's Resources.
fn resources_of(executable: &Path) -> Option<PathBuf> {
    let dir = executable.parent()?;
    Some(if cfg!(target_os = "macos") {
        dir.parent()?.join("Resources")
    } else {
        dir.to_path_buf()
    })
}

/// Writes the startup files into the Firefox at `executable`, where they
/// differ from Netweir's own: an install from an older Netweir is brought
/// up to date. Only ever called for a Firefox Netweir installed.
pub fn prepare_firefox(executable: &Path) -> std::result::Result<(), String> {
    let resources = resources_of(executable)
        .ok_or_else(|| format!("{} isn't in a Firefox install", executable.display()))?;
    for (path, contents) in [
        (resources.join("defaults/pref/autoconfig.js"), AUTOCONFIG),
        (resources.join("mozilla.cfg"), MOZILLA_CFG),
    ] {
        if std::fs::read(&path).is_ok_and(|now| now == contents.as_bytes()) {
            continue;
        }
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, contents));
        written.map_err(|e| format!("can't write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Whether `executable` is a Firefox Netweir installed under `home`: the
/// only kind it starts without the automation flag, and the only kind it
/// writes into.
pub(crate) fn is_ours_in(executable: &Path, home: &Path) -> bool {
    let root = home.join("firefox");
    // Both sides as the file system has them, so a path through a link or
    // with `..` in it is judged by where it leads.
    let (Ok(executable), Ok(root)) = (executable.canonicalize(), root.canonicalize()) else {
        return false;
    };
    executable.starts_with(&root)
        && resources_of(&executable)
            .and_then(|r| std::fs::read_to_string(r.join("mozilla.cfg")).ok())
            .is_some_and(|cfg| cfg.lines().next() == Some(MARK))
}

#[expect(dead_code, reason = "the Firefox launch, still to come, is its caller")]
pub(crate) fn is_ours(executable: &Path) -> bool {
    is_ours_in(executable, &netweir_home())
}

/// The newest Firefox `netweir install firefox` put under `home`.
pub(crate) fn installed_firefox(home: &Path) -> Option<PathBuf> {
    let mut versions: Vec<(Vec<u32>, PathBuf)> = std::fs::read_dir(home.join("firefox"))
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
        .map(|(_, dir)| firefox_executable(&dir))
        .find(|p| p.is_file())
}

/// The Firefox to launch: `$NETWEIR_FIREFOX`, then the newest one
/// `netweir install firefox` installed. A Firefox installed some other way
/// is never picked: Netweir can drive one only in automation mode, which
/// pages can see, so that takes asking for by name.
pub fn find_firefox() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NETWEIR_FIREFOX") {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(Error::Launch(format!(
                "$NETWEIR_FIREFOX is {}, which doesn't exist",
                path.display()
            )))
        };
    }
    let home = netweir_home();
    installed_firefox(&home).ok_or_else(|| {
        Error::Launch(format!(
            "no Firefox in {}: run `netweir install firefox`",
            home.join("firefox").display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("netweir-ff-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An install folder holding an empty executable where Firefox's is.
    fn install(dir: &Path) -> PathBuf {
        let executable = firefox_executable(dir);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "").unwrap();
        executable
    }

    #[test]
    fn prepare_writes_both_files_where_firefox_reads_them() {
        let dir = temp("prepare");
        let executable = install(&dir);
        prepare_firefox(&executable).unwrap();
        let resources = if cfg!(target_os = "macos") {
            dir.join("Firefox.app/Contents/Resources")
        } else if cfg!(windows) {
            dir.join("core")
        } else {
            dir.join("firefox")
        };
        assert_eq!(
            std::fs::read_to_string(resources.join("defaults/pref/autoconfig.js")).unwrap(),
            AUTOCONFIG
        );
        assert_eq!(
            std::fs::read_to_string(resources.join("mozilla.cfg")).unwrap(),
            MOZILLA_CFG
        );
        // Firefox skips the first line; ours is the mark.
        assert_eq!(MOZILLA_CFG.lines().next(), Some(MARK));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prepare_leaves_unchanged_files_alone_and_mends_changed_ones() {
        let dir = temp("unchanged");
        let executable = install(&dir);
        prepare_firefox(&executable).unwrap();
        let cfg = resources_of(&executable).unwrap().join("mozilla.cfg");
        let autoconfig = resources_of(&executable)
            .unwrap()
            .join("defaults/pref/autoconfig.js");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for f in [&cfg, &autoconfig] {
            std::fs::File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        std::fs::write(&cfg, "// netweir\n// an older one\n").unwrap();
        prepare_firefox(&executable).unwrap();
        assert_eq!(
            std::fs::metadata(&autoconfig).unwrap().modified().unwrap(),
            old
        );
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), MOZILLA_CFG);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn installed_firefox_picks_the_newest_version() {
        let home = temp("newest");
        for version in ["159.0", "159.0.1", "160.0b3", ".159.0-firefox-12", "158.0"] {
            install(&home.join("firefox").join(version));
        }
        // A version folder without an executable doesn't count.
        std::fs::create_dir_all(home.join("firefox/161.0")).unwrap();
        assert_eq!(
            installed_firefox(&home),
            Some(firefox_executable(&home.join("firefox/159.0.1")))
        );
        assert_eq!(installed_firefox(&home.join("nothing")), None);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn only_a_netweir_install_is_ours() {
        let home = temp("ours");
        let ours = install(&home.join("firefox/159.0"));
        assert!(!is_ours_in(&ours, &home), "no cfg yet");
        prepare_firefox(&ours).unwrap();
        assert!(is_ours_in(&ours, &home));
        // The same files in a Firefox somewhere else don't make it ours.
        let elsewhere = install(&home.join("elsewhere"));
        prepare_firefox(&elsewhere).unwrap();
        assert!(!is_ours_in(&elsewhere, &home));
        // Nor does a path that only looks as if it's inside.
        let sneaky = home.join("firefox/159.0/../../elsewhere");
        assert!(!is_ours_in(&firefox_executable(&sneaky), &home));
        // A cfg someone else wrote isn't Netweir's.
        let theirs = install(&home.join("firefox/158.0"));
        let resources = resources_of(&theirs).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("mozilla.cfg"), "// company policy\n").unwrap();
        assert!(!is_ours_in(&theirs, &home));
        std::fs::remove_dir_all(home).unwrap();
    }
}
