// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use crate::{DmaInfo, DmaWindow, ResError};

const FULL_MASK: u64 = u64::MAX;

fn info(cpu_start: u64, bus_start: u64, len: u64, mask: u64) -> DmaInfo {
    DmaInfo::new(
        mask,
        DmaWindow::new(cpu_start, bus_start, len).unwrap(),
        true,
    )
    .unwrap()
}

#[def_test]
fn rejects_empty_or_overflowing_windows() {
    assert_eq!(
        DmaWindow::new(0, 0, 0).unwrap_err(),
        ResError::InvalidResource
    );
    assert_eq!(
        DmaWindow::new(u64::MAX, 0, 2).unwrap_err(),
        ResError::InvalidResource
    );
    assert_eq!(
        DmaWindow::new(0, u64::MAX, 2).unwrap_err(),
        ResError::InvalidResource
    );

    // An inclusive end exactly at u64::MAX is representable.
    assert_eq!(
        DmaWindow::new(u64::MAX - 1, u64::MAX - 1, 2),
        Ok(DmaWindow::new(u64::MAX - 1, u64::MAX - 1, 2).unwrap())
    );
}

#[def_test]
fn translates_complete_intervals_within_the_window() {
    let view = info(0x1000, 0x8000_0000, 0x1000, FULL_MASK);

    assert_eq!(view.translate(0x1000), Some(0x8000_0000));
    assert_eq!(view.translate_range(0x1000, 1), Some(0x8000_0000));
    assert_eq!(view.translate_range(0x1000, 0x1000), Some(0x8000_0000));
    assert_eq!(view.translate_range(0x1fff, 1), Some(0x8000_0fff));

    // Before the window, crossing its end, and zero length are all rejected.
    assert_eq!(view.translate(0x0fff), None);
    assert_eq!(view.translate_range(0x1000, 0x1001), None);
    assert_eq!(view.translate_range(0x1fff, 2), None);
    assert_eq!(view.translate_range(0x1000, 0), None);

    // The last offset must not overflow the 64-bit arithmetic either.
    let edge = info(u64::MAX - 1, 0, 2, FULL_MASK);
    assert_eq!(edge.translate_range(u64::MAX - 1, 2), Some(0));
    assert_eq!(edge.translate_range(u64::MAX, 2), None);
}

#[def_test]
fn validates_the_whole_interval_against_the_device_mask() {
    let view = info(0, 0x100, 0x100, 0xff);

    assert_eq!(view.translate_range(0, 0x100), None);
    assert_eq!(view.translate_range(0, 0x101), None);
    assert_eq!(view.translate_range(0x100, 1), None);

    let limited = info(0, 0x80, 0x100, 0xff);
    assert_eq!(limited.translate_range(0, 0x80), Some(0x80));
    assert_eq!(limited.translate_range(0, 0x81), None);
}

#[def_test]
fn rejects_noncontiguous_masks_instead_of_rounding() {
    let window = DmaWindow::new(0, 0, 0x1000).unwrap();

    assert_eq!(DmaInfo::new(0b101, window, true).unwrap_err(), ResError::InvalidResource);
    assert_eq!(
        DmaInfo::new(0xffff_fff0, window, true).unwrap_err(),
        ResError::InvalidResource
    );
    core::assert!(DmaInfo::new(0, window, true).is_ok());
    core::assert!(DmaInfo::new(0xffff, window, true).is_ok());
    core::assert!(DmaInfo::new(u64::MAX, window, true).is_ok());
}

#[def_test]
fn identity_must_be_explicit() {
    let identity = DmaInfo::new(u64::MAX, DmaWindow::IDENTITY, true).unwrap();

    core::assert!(identity.window().is_identity());
    assert_eq!(identity.window().cpu_start(), 0);
    assert_eq!(identity.window().bus_start(), 0);
    assert_eq!(identity.translate(0), Some(0));
    assert_eq!(identity.translate(u64::MAX), Some(u64::MAX));

    let offset = info(0x1000, 0x2000, 0x1000, FULL_MASK).window();
    core::assert!(!offset.is_identity());
    assert_eq!(offset.cpu_start(), 0x1000);
    assert_eq!(offset.bus_start(), 0x2000);
}
