// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for display drivers using the unittest framework.

#![cfg(unittest)]

use unittest::{assert, assert_eq, assert_ne, def_test};

use super::{DisplayInfo, ScanoutFormat, ScanoutRect, ScanoutResource};

// ============================================================================
// DisplayInfo Tests
// ============================================================================

#[def_test]
fn test_display_info_boundary_values() {
    // Minimum valid display (1x1).
    let min_display = DisplayInfo {
        width: 1,
        height: 1,
    };
    assert_eq!(min_display.width, 1);
    assert_eq!(min_display.height, 1);

    // Large display dimensions (4K).
    let large_display = DisplayInfo {
        width: 4096,
        height: 2160,
    };
    assert_eq!(large_display.width, 4096);
    assert_eq!(large_display.height, 2160);

    // Zero dimensions (edge case).
    let zero_display = DisplayInfo {
        width: 0,
        height: 0,
    };
    assert_eq!(zero_display.width, 0);
    assert_eq!(zero_display.height, 0);
}

#[def_test]
fn test_display_info_common_resolutions() {
    // Verify DisplayInfo carries resolution correctly for typical modes.
    let resolutions = [
        (640, 480),   // VGA
        (1280, 720),  // HD
        (1920, 1080), // Full HD
        (2560, 1440), // QHD
    ];

    for &(width, height) in &resolutions {
        let info = DisplayInfo { width, height };
        assert_eq!(info.width, width);
        assert_eq!(info.height, height);

        // A scanout emulation layer sizes its shadow buffer as width*height*4
        // (BGRA8888); sanity-check the byte footprint stays in a reasonable
        // range and never overflows u32 backing for the modes we care about.
        let bytes = (width as u64) * (height as u64) * 4;
        assert!(bytes <= u32::MAX as u64, "shadow size overflows u32");
    }
}

// ============================================================================
// ScanoutRect Tests
// ============================================================================

#[def_test]
fn test_scanout_rect_default_and_fields() {
    // A zero rect is the degenerate "nothing to present" value.
    assert_eq!(
        ScanoutRect::default(),
        ScanoutRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    );

    // Full-screen rect for a 1920x1080 display.
    let rect = ScanoutRect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    assert_eq!(rect.x, 0);
    assert_eq!(rect.y, 0);
    assert_eq!(rect.width, 1920);
    assert_eq!(rect.height, 1080);
    // Copy semantics: ScanoutRect is a plain value type.
    let copy = rect;
    assert_eq!(rect, copy);
}

#[def_test]
fn test_scanout_rect_u32_bounds() {
    // Rect coordinates live at the u32 limit; x + width must not wrap when
    // a driver converts the rect for a device command.
    let max_rect = ScanoutRect {
        x: u32::MAX - 1,
        y: u32::MAX - 1,
        width: 1,
        height: 1,
    };
    assert_eq!(max_rect.x + max_rect.width, u32::MAX);
    assert_eq!(max_rect.y + max_rect.height, u32::MAX);

    // A sub-rect of a larger surface stays inside its parent bounds.
    let parent = ScanoutRect {
        x: 0,
        y: 0,
        width: 4096,
        height: 2160,
    };
    let sub = ScanoutRect {
        x: 16,
        y: 32,
        width: 1920,
        height: 1080,
    };
    assert!(sub.x + sub.width <= parent.width);
    assert!(sub.y + sub.height <= parent.height);
}

// ============================================================================
// ScanoutResource Tests
// ============================================================================

#[def_test]
fn test_scanout_resource_pitch_consistency() {
    // BGRA8888 resources are packed: pitch == width * 4 bytes, and the byte
    // footprint must fit the u32 length field used by attach-backing.
    for &(width, height) in &[(640u32, 480u32), (1920, 1080), (4096, 2160)] {
        let resource = ScanoutResource {
            id: 1,
            width,
            height,
            pitch: width * 4,
            format: ScanoutFormat::Bgra8888,
        };
        assert_eq!(resource.width, width);
        assert_eq!(resource.height, height);
        assert_eq!(resource.pitch, width * 4);
        let footprint = (resource.pitch as u64) * (resource.height as u64);
        assert!(footprint <= u32::MAX as u64, "backing length overflows u32");
    }
}

#[def_test]
fn test_scanout_resource_equality() {
    let base = ScanoutResource {
        id: 7,
        width: 640,
        height: 480,
        pitch: 2560,
        format: ScanoutFormat::Bgra8888,
    };
    assert_eq!(base, base);
    // Resources are distinguished by their id, not their geometry.
    let different_id = ScanoutResource { id: 8, ..base };
    assert_ne!(base, different_id);
    // Geometry changes also break equality.
    let different_size = ScanoutResource {
        width: 1280,
        pitch: 5120,
        ..base
    };
    assert_ne!(base, different_size);
}
