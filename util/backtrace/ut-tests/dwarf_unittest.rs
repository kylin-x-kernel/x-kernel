// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String};
use core::fmt;

use unittest::def_test;

use super::*;

struct DisplayOneFrame(addr2line::Frame<'static, DwarfReader>);

impl fmt::Display for DisplayOneFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_frame(f, &self.0)
    }
}

struct DisplayFrames<'a>(&'a [crate::Frame]);

impl fmt::Display for DisplayFrames<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_frames(f, self.0)
    }
}

fn render_symbolized_frame(frame: addr2line::Frame<'static, DwarfReader>) -> String {
    format!("{}", DisplayOneFrame(frame))
}

fn plain_function_frame() -> addr2line::Frame<'static, DwarfReader> {
    addr2line::Frame {
        dw_die_offset: None,
        function: Some(addr2line::FunctionName {
            name: DwarfReader::new(b"plain_function", gimli::RunTimeEndian::default()),
            language: None,
        }),
        location: None,
    }
}

#[def_test]
fn dwarf_context_storage_starts_empty() {
    let storage = DwarfContextStorage::new();
    assert!(storage.get().is_none());
}

#[def_test]
fn frame_iter_returns_none_without_context() {
    let frames = [crate::Frame::new(0x1000, 0x2000)];
    let mut iter = FrameIter::new(&frames);

    assert!(iter.next().is_none());
    assert!(iter.next().is_none());
}

#[def_test]
fn fmt_frame_falls_back_to_unknown_function_without_location() {
    let rendered = render_symbolized_frame(addr2line::Frame {
        dw_die_offset: None,
        function: None,
        location: None,
    });

    assert_eq!(rendered, ": <unknown>\n");
}

#[def_test]
fn fmt_frame_stops_at_unknown_file_boundary() {
    let mut frame = plain_function_frame();
    frame.location = Some(addr2line::Location {
        file: None,
        line: Some(42),
        column: Some(7),
    });

    let rendered = render_symbolized_frame(frame);

    assert_eq!(rendered, ": plain_function\n            at ??");
}

#[def_test]
fn fmt_frame_includes_full_source_location_when_available() {
    let mut frame = plain_function_frame();
    frame.location = Some(addr2line::Location {
        file: Some("src/dwarf.rs"),
        line: Some(42),
        column: Some(7),
    });

    let rendered = render_symbolized_frame(frame);

    assert_eq!(
        rendered,
        ": plain_function\n            at src/dwarf.rs:42:7"
    );
}

#[def_test]
fn fmt_frames_reports_empty_capture() {
    assert_eq!(
        format!("{}", DisplayFrames(&[])),
        "Symbolication disabled in test mode.\nRaw frames:\n"
    );
}

#[def_test]
fn fmt_frames_without_context_reports_raw_frames() {
    let frames = [crate::Frame::new(0x10, 0x20), crate::Frame::new(0x30, 0x40)];

    let rendered = format!("{}", DisplayFrames(&frames));

    assert!(rendered.starts_with("Symbolication disabled in test mode.\nRaw frames:\n"));
    assert!(rendered.contains("     0: fp=0x0000000000000010, ip=0x0000000000000020"));
    assert!(rendered.contains("     1: fp=0x0000000000000030, ip=0x0000000000000040"));
}
