// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 James Conway

//! The end of input closed one open template at a time and handled the
//! end again by calling itself, so a page of a few thousand nested
//! `<template>`s overflowed the stack, which ends the process.

use netweir_html::parse;

#[test]
fn nested_templates_at_the_end_of_input_dont_overflow_the_stack() {
    let shapes = [
        "<template>",
        "<div><template>",
        "<caption><template>",
        "<template><tr>",
        "<template><col>",
        "<template><td>",
    ];
    for shape in shapes {
        let page = shape.repeat(100_000);
        let count = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || parse(&page, false).len())
            .unwrap()
            .join()
            .unwrap();
        assert!(count > 100_000, "{shape}: {count} nodes");
    }
}
