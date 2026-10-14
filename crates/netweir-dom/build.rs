//! Compiles lexbor from the submodule with `cc`, so building netweir needs a
//! C compiler and nothing else (no cmake).

use std::path::Path;

/// The lexbor modules netweir uses, plus what they depend on.
const MODULES: &[&str] = &[
    "core",
    "dom",
    "html",
    "ns",
    "tag",
    "css",
    "selectors",
    "encoding",
    "punycode",
    "unicode",
    "url",
    "style",
];

fn main() {
    let source = Path::new("lexbor/source");
    if !source.exists() {
        panic!("lexbor sources missing: run `git submodule update --init`");
    }

    let mut build = cc::Build::new();
    build
        .include(source)
        .define("LEXBOR_STATIC", None)
        .warnings(false)
        .opt_level(2);
    for module in MODULES {
        add_c_files(&mut build, &source.join("lexbor").join(module));
    }
    let port = if cfg!(target_os = "windows") {
        "windows_nt"
    } else {
        "posix"
    };
    add_c_files(&mut build, &source.join("lexbor/ports").join(port));
    build.file("csrc/shim.c");
    println!("cargo:rustc-env=LEXBOR_VERSION={}", lexbor_version(source));
    build.compile("lexbor");

    println!("cargo:rerun-if-changed=csrc/shim.c");
    println!("cargo:rerun-if-changed=lexbor/source");
}

/// lexbor's version, as its `core/base.h` defines it.
fn lexbor_version(source: &Path) -> String {
    let header = std::fs::read_to_string(source.join("lexbor/core/base.h"))
        .expect("reading lexbor/core/base.h");
    let part = |name: &str| {
        let define = format!("#define LEXBOR_VERSION_{name} ");
        header
            .lines()
            .find_map(|line| line.strip_prefix(&define))
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|| panic!("lexbor/core/base.h has no LEXBOR_VERSION_{name}"))
    };
    format!("{}.{}.{}", part("MAJOR"), part("MINOR"), part("PATCH"))
}

fn add_c_files(build: &mut cc::Build, dir: &Path) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            add_c_files(build, &path);
        } else if path.extension().is_some_and(|e| e == "c") {
            build.file(&path);
        }
    }
}
