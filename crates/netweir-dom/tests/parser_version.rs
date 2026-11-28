//! `PARSER` names the parsec-web-core that Cargo.lock builds.

#[test]
fn parser_is_the_locked_parsec_web_core() {
    let lock =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock")).unwrap();
    let version = lock
        .split("[[package]]")
        .find(|p| p.contains("name = \"parsec-web-core\""))
        .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = ")))
        .expect("parsec-web-core in Cargo.lock")
        .trim_matches('"');
    assert_eq!(netweir_dom::PARSER, format!("parsec-web-core {version}"));
}
