// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Basic integration tests for backtrace functionality.
//!
//! Stack unwinding is always available; DWARF/symtab symbolication is not
//! exercised here because it requires kernel-specific linker sections.

// Stub linker symbols for the embedded DWARF sections: the `dwarf`
// feature compiles `dwarf::init` against the `__start_debug_*` /
// `__stop_debug_*` symbols that the kernel linker script provides, and
// host-side integration tests link the library without `cfg(test)`.
#[cfg(feature = "dwarf")]
mod test_stubs {
    #![allow(dead_code)]
    #![allow(non_upper_case_globals)]
    #![allow(clippy::used_underscore_binding)]

    macro_rules! stub {
        ($name:ident) => {
            // `no_mangle`: `dwarf.rs` references these through an `extern`
            // block, which resolves the unmangled C symbol name.
            #[used]
            #[unsafe(no_mangle)]
            pub static $name: [u8; 1] = [0];
        };
    }

    stub!(__start_debug_abbrev);
    stub!(__stop_debug_abbrev);
    stub!(__start_debug_addr);
    stub!(__stop_debug_addr);
    stub!(__start_debug_aranges);
    stub!(__stop_debug_aranges);
    stub!(__start_debug_info);
    stub!(__stop_debug_info);
    stub!(__start_debug_line);
    stub!(__stop_debug_line);
    stub!(__start_debug_line_str);
    stub!(__stop_debug_line_str);
    stub!(__start_debug_ranges);
    stub!(__stop_debug_ranges);
    stub!(__start_debug_rnglists);
    stub!(__stop_debug_rnglists);
    stub!(__start_debug_str);
    stub!(__stop_debug_str);
    stub!(__start_debug_str_offsets);
    stub!(__stop_debug_str_offsets);
}

use backtrace::{Backtrace, Frame, init, max_depth, set_max_depth};

#[test]
fn test_initialization() {
    // Initialize with wide ranges for testing
    init(0..usize::MAX, 0..usize::MAX);

    // Should not panic
    let bt = Backtrace::capture();

    // Unwinding must produce at least the current frame
    assert!(bt.frame_count() > 0);
}

#[test]
fn test_frame_creation() {
    let frame = Frame::new(0x7fff_0000, 0x8000_1234);

    assert_eq!(frame.fp, 0x7fff_0000);
    assert_eq!(frame.ip, 0x8000_1234);
    assert!(frame.is_valid());
}

#[test]
fn test_frame_display() {
    let frame = Frame::new(0x7fff_0000, 0x8000_1234);
    let display = format!("{}", frame);

    // Should contain hex addresses
    assert!(display.contains("fp="));
    assert!(display.contains("ip="));
}

#[test]
fn test_frame_adjusted_ip() {
    let frame = Frame::new(0x1000, 0x2000);
    assert_eq!(frame.adjust_ip(), 0x1fff);

    // Edge case: IP = 0
    let frame_zero = Frame::new(0x1000, 0);
    assert_eq!(frame_zero.adjust_ip(), 0);
}

#[test]
fn test_invalid_frame() {
    let frame = Frame::new(0, 0);
    assert!(!frame.is_valid());
}

#[test]
fn test_max_depth_configuration() {
    let original = max_depth();

    set_max_depth(10);
    assert_eq!(max_depth(), 10);

    set_max_depth(100);
    assert_eq!(max_depth(), 100);

    // Restore original
    set_max_depth(original);
}

#[test]
fn test_recursive_capture() {
    init(0..usize::MAX, 0..usize::MAX);

    fn recursive(depth: usize) -> Backtrace {
        if depth == 0 {
            Backtrace::capture()
        } else {
            recursive(depth - 1)
        }
    }

    let bt = recursive(5);

    // Raw unwinding always captures frames, independent of any
    // symbolication feature.
    assert!(bt.frame_count() > 0);
}

#[test]
fn test_backtrace_display() {
    init(0..usize::MAX, 0..usize::MAX);

    let bt = Backtrace::capture();
    let display = format!("{}", bt);

    // Raw output starts with a stable header followed by indexed addresses.
    assert!(display.starts_with("Backtrace:"));
    assert!(display.contains("0: 0x"));
}

#[test]
fn test_capture_trap() {
    init(0..usize::MAX, 0..usize::MAX);

    // An invalid frame pointer aborts unwinding immediately, leaving only
    // the synthetic trap frame.
    let fp = 0usize;
    let ip = 0x8000_5000usize;
    let ra = 0x8000_6000usize;

    let bt = Backtrace::capture_trap(fp, ip, ra);
    assert_eq!(bt.frame_count(), 1);
    let first = bt.raw_frames().expect("captured frames").first().unwrap();
    assert_eq!(first.fp, fp);
    assert_eq!(first.ip, ip.wrapping_add(1));
}

#[test]
fn test_raw_frames_access() {
    init(0..usize::MAX, 0..usize::MAX);

    let bt = Backtrace::capture();

    // Raw frames are always available.
    let frames = bt.raw_frames().expect("raw frames must be available");
    assert!(!frames.is_empty());
    for frame in frames {
        // Each frame should be displayable
        let _ = format!("{}", frame);
    }
}

#[test]
fn test_frame_count() {
    init(0..usize::MAX, 0..usize::MAX);

    let bt = Backtrace::capture();
    let count = bt.frame_count();

    // Unwinding is always available, so at least the current frame is
    // captured.
    assert!(count > 0);
}
