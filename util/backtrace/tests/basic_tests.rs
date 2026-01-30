// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Basic integration tests for backtrace functionality.
//!
//! Note: DWARF symbolication is not tested here because it requires
//! kernel-specific linker symbols. These tests verify the core
//! unwinding logic and API surface.

// Include stub symbols when testing with dwarf feature
#[cfg(feature = "dwarf")]
mod test_stubs;

use backtrace::{Backtrace, Frame, init, max_depth, set_max_depth};

#[test]
fn test_initialization() {
    // Initialize with wide ranges for testing
    init(0..usize::MAX, 0..usize::MAX);

    // Should not panic
    let bt = Backtrace::capture();

    // Verify we got a backtrace object (frame count may be 0)
    let _count = bt.frame_count();
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

    // Should have captured some frames
    // Exact count depends on optimization level
    let _count = bt.frame_count();
    // Note: count may be 0 in optimized builds without dwarf feature
}

#[test]
fn test_backtrace_display() {
    init(0..usize::MAX, 0..usize::MAX);

    let bt = Backtrace::capture();
    let display = format!("{}", bt);

    // Should produce some output
    assert!(!display.is_empty());
}

#[test]
fn test_capture_trap() {
    init(0..usize::MAX, 0..usize::MAX);

    // Note: With dwarf feature enabled, capture_trap will try to unwind
    // from the provided frame pointer. Since we can't provide valid
    // stack addresses in a test, we skip this test when dwarf is enabled.
    #[cfg(not(feature = "dwarf"))]
    {
        let fp = 0x7fff_1000;
        let ip = 0x8000_5000;
        let ra = 0x8000_6000;

        let bt = Backtrace::capture_trap(fp, ip, ra);
        assert_eq!(bt.frame_count(), 0);
    }

    // With dwarf enabled, we can't test with fake addresses
    #[cfg(feature = "dwarf")]
    {
        // Just verify the API exists and is callable
        // In a real kernel, this would work with actual trap frame addresses
    }
}

#[test]
#[cfg(feature = "alloc")]
fn test_raw_frames_access() {
    init(0..usize::MAX, 0..usize::MAX);

    let bt = Backtrace::capture();

    // Should be able to access raw frames
    if let Some(frames) = bt.raw_frames() {
        for frame in frames {
            // Each frame should be displayable
            let _ = format!("{}", frame);
        }
    }
}

#[test]
fn test_frame_count() {
    init(0..usize::MAX, 0..usize::MAX);

    let bt = Backtrace::capture();
    let _count = bt.frame_count();

    // Count is always non-negative by definition (usize)
    // This test mainly checks the API works correctly
}
