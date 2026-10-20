//! Files a step uploads from a folder it's held to: checked, and copied
//! where nothing else can change them, before Chrome is handed them.
//!
//! Chrome reads an upload's file when the form is sent, which can be well
//! after the step that set it. A path checked against the folder could by
//! then be a link to somewhere else. So each file is opened without
//! following links, checked to be the file that was resolved, and copied
//! into a folder of netweir's own (0700 on Unix), under its own name, as
//! forms see it. Chrome gets the copy, which goes when the page closes.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, Result};

/// The folders a page's uploads were copied into, removed when it goes.
#[derive(Default)]
pub(crate) struct UploadDirs {
    dirs: Mutex<Vec<PathBuf>>,
}

impl UploadDirs {
    pub(crate) fn keep(&self, dir: PathBuf) {
        self.dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(dir);
    }

    /// Removes them now (the page has closed).
    pub(crate) fn clear(&self) {
        let dirs = std::mem::take(&mut *self.dirs.lock().unwrap_or_else(|e| e.into_inner()));
        for dir in dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl Drop for UploadDirs {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Copies of `files`, each of which must be inside `root`, into a new
/// private folder; the folder is returned too, for whoever removes it.
pub(crate) fn held_copies(root: &Path, files: &[PathBuf]) -> Result<(PathBuf, Vec<PathBuf>)> {
    let invalid = |what: String| Error::Invalid(what);
    let root = std::fs::canonicalize(root)
        .map_err(|e| invalid(format!("the upload folder {}: {e}", root.display())))?;
    let dir =
        private_dir().map_err(|e| invalid(format!("can't make a folder for uploads: {e}")))?;
    let copied = (|| {
        let mut out = Vec::new();
        for (i, file) in files.iter().enumerate() {
            let real = std::fs::canonicalize(file)
                .map_err(|e| invalid(format!("{}: {e}", file.display())))?;
            if !real.starts_with(&root) {
                return Err(invalid(format!(
                    "{} is {}, outside the upload folder {}",
                    file.display(),
                    real.display(),
                    root.display()
                )));
            }
            let mut source = open_held(&real)?;
            let name = real
                .file_name()
                .ok_or_else(|| invalid(format!("{} has no file name", file.display())))?;
            let slot = dir.join(i.to_string());
            std::fs::create_dir(&slot)
                .map_err(|e| invalid(format!("can't copy {}: {e}", file.display())))?;
            let copy = slot.join(name);
            let mut target = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&copy)
                .map_err(|e| invalid(format!("can't copy {}: {e}", file.display())))?;
            std::io::copy(&mut source, &mut target)
                .map_err(|e| invalid(format!("can't copy {}: {e}", file.display())))?;
            out.push(copy);
        }
        Ok(out)
    })();
    match copied {
        Ok(out) => Ok((dir, out)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// `real` (resolved, inside the folder) opened without following a link
/// at its end, and refused unless it's still the regular file it was
/// resolved to, with no link or swapped folder on the way to it.
fn open_held(real: &Path) -> Result<File> {
    let swapped = || {
        Error::Invalid(format!(
            "{} changed while it was being checked (a link swapped in?)",
            real.display()
        ))
    };
    let file = open_no_follow(real).map_err(|_| swapped())?;
    let meta = file.metadata().map_err(|_| swapped())?;
    if !meta.is_file() {
        return Err(Error::Invalid(format!("{} isn't a file", real.display())));
    }
    // A folder on the way swapped for a link since it was resolved would
    // resolve elsewhere now.
    if std::fs::canonicalize(real).ok().as_deref() != Some(real) {
        return Err(swapped());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let now = std::fs::symlink_metadata(real).map_err(|_| swapped())?;
        if (now.dev(), now.ino()) != (meta.dev(), meta.ino()) {
            return Err(swapped());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(swapped());
        }
    }
    Ok(file)
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    // The link itself, not what it points to: open_held then refuses it.
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// A new folder only this user can open.
fn private_dir() -> std::io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "netweir-upload-{}-{stamp}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("netweir-held-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("up")).unwrap();
        dir
    }

    #[test]
    fn a_held_upload_is_a_private_copy_with_its_own_name() {
        let dir = scratch("copy");
        let cv = dir.join("up").join("cv.txt");
        std::fs::write(&cv, "MY-CV").unwrap();
        let (copies, files) = held_copies(&dir.join("up"), std::slice::from_ref(&cv)).unwrap();
        assert_eq!(files[0].file_name().unwrap(), "cv.txt");
        assert!(files[0].starts_with(&copies));
        assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), "MY-CV");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&copies).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // Changing the original now changes nothing Chrome would read.
        std::fs::write(&cv, "SWAPPED").unwrap();
        assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), "MY-CV");
        let dirs = UploadDirs::default();
        dirs.keep(copies.clone());
        drop(dirs);
        assert!(!copies.exists(), "gone with the page");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_held_upload_refuses_links_and_dots_out_of_its_folder() {
        let dir = scratch("refuse");
        let outside = dir.join("secret.txt");
        std::fs::write(&outside, "SECRET").unwrap();
        let up = dir.join("up");
        let err = held_copies(&up, &[up.join("..").join("secret.txt")]).unwrap_err();
        assert!(
            err.to_string().contains("outside the upload folder"),
            "{err}"
        );
        #[cfg(unix)]
        {
            let link = up.join("link.txt");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let err = held_copies(&up, &[link]).unwrap_err();
            assert!(
                err.to_string().contains("outside the upload folder"),
                "{err}"
            );
            // A link opened as if it were the file it was resolved to.
            let real = std::fs::canonicalize(up.join(".."))
                .unwrap()
                .join("up")
                .join("late.txt");
            std::os::unix::fs::symlink(&outside, &real).unwrap();
            assert!(
                open_held(&real).is_err(),
                "a link at the end isn't followed"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
