//! Any string as script text: every literal in it, the value of a name
//! (the first line) assigned in the rest, and Nuxt's state in it.
#![no_main]

use libfuzzer_sys::fuzz_target;
use netweir_dom::js;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let _ = js::all(&text);
    let (name, script) = text.split_once('\n').unwrap_or(("data", &text));
    let _ = js::assigned(script, name);
    let _ = js::nuxt(&text);
});
