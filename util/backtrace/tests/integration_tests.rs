#![cfg(test)]
#![cfg(feature = "alloc")]

use backtrace::{init, set_max_depth, Frame};

#[test]
fn test_initialization() {
    init(0..usize::MAX, 0..usize::MAX);
    assert!(backtrace::is_initialized());
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
    assert!(display.contains("7fff0000"));
    assert!(display.contains("80001234"));
}

#[test]
fn test_frame_adjust_ip() {
    let frame = Frame::new(0x7fff_0000, 0x8000_1234);
    assert_eq!(frame.adjust_ip(), 0x8000_1233);

    // Test saturating_sub at 0
    let frame_zero = Frame::new(0, 0);
    assert_eq!(frame_zero.adjust_ip(), 0);
}

#[test]
fn test_set_max_depth() {
    set_max_depth(10);
    assert_eq!(backtrace::max_depth(), 10);

    set_max_depth(20);
    assert_eq!(backtrace::max_depth(), 20);

    // Setting 0 should not change the value
    set_max_depth(0);
    assert_eq!(backtrace::max_depth(), 20);
}

#[test]
fn test_backtrace_feature_enabled() {
    #[cfg(feature = "dwarf")]
    assert!(backtrace::is_enabled());

    #[cfg(not(feature = "dwarf"))]
    assert!(!backtrace::is_enabled());
}

#[test]
fn test_invalid_frame() {
    let frame = Frame::new(0, 0);
    assert!(!frame.is_valid());

    let frame = Frame::new(0x1000, 0);
    assert!(!frame.is_valid());

    let frame = Frame::new(0, 0x1000);
    assert!(!frame.is_valid());
}

#[test]
fn test_frame_ordering() {
    let frame1 = Frame::new(0x1000, 0x2000);
    let frame2 = Frame::new(0x1000, 0x3000);
    let frame3 = Frame::new(0x2000, 0x2000);

    assert!(frame1 < frame2);
    assert!(frame1 < frame3);
    assert!(frame2 < frame3);
}

#[test]
fn test_frame_equality() {
    let frame1 = Frame::new(0x1000, 0x2000);
    let frame2 = Frame::new(0x1000, 0x2000);
    let frame3 = Frame::new(0x1001, 0x2000);

    assert_eq!(frame1, frame2);
    assert_ne!(frame1, frame3);
}
