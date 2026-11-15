//! A conformance suite's counts as JSON, written to `$CONFORMANCE_DIR/<suite>.json`
//! when that's set, so CI can publish them.

use std::path::Path;

pub struct Counts<'a> {
    pub suite: &'a str,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// Why the skipped tests can't run here.
    pub skipped_because: &'a str,
    /// The test data's directory: its `COMMIT` file, or else the git commit
    /// checked out there, says which version of the suite ran.
    pub data: &'a Path,
}

pub fn write(c: &Counts) {
    let Some(dir) = std::env::var_os("CONFORMANCE_DIR") else {
        return;
    };
    let commit = std::fs::read_to_string(c.data.join("COMMIT"))
        .ok()
        .or_else(|| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(c.data)
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        })
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());
    let json = serde_json::json!({
        "suite": c.suite,
        "passed": c.passed,
        "failed": c.failed,
        "skipped": c.skipped,
        "skipped_because": c.skipped_because,
        "test_data_commit": commit,
    });
    std::fs::create_dir_all(&dir).unwrap();
    let path = Path::new(&dir).join(format!("{}.json", c.suite));
    std::fs::write(path, format!("{json:#}\n")).unwrap();
}
