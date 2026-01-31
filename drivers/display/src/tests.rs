//! Unit tests for display drivers using the unittest framework.

#![cfg(unittest)]

extern crate alloc;

use alloc::vec;

use unittest::{assert, assert_eq, def_test};

use super::{DisplayInfo, FrameBuffer};

// ============================================================================
// DisplayInfo Tests
// ============================================================================

#[def_test]
fn test_display_info_boundary_values() {
    // Test boundary values for display dimensions and buffer sizes

    // Minimum valid display (1x1)
    let min_display = DisplayInfo {
        width: 1,
        height: 1,
        fb_base_vaddr: 0x1000,
        fb_size: 4, // 1 pixel * 4 bytes (RGBA)
    };

    assert_eq!(min_display.width, 1);
    assert_eq!(min_display.height, 1);
    assert_eq!(min_display.fb_size, 4);

    // Large display dimensions
    let large_display = DisplayInfo {
        width: 4096,
        height: 2160, // 4K resolution
        fb_base_vaddr: 0x10000000,
        fb_size: 4096 * 2160 * 4, // 4 bytes per pixel
    };

    assert_eq!(large_display.width, 4096);
    assert_eq!(large_display.height, 2160);
    assert_eq!(large_display.fb_size, 4096 * 2160 * 4);

    // Test framebuffer size calculation consistency
    let calculated_size = (large_display.width * large_display.height * 4) as usize;
    assert_eq!(large_display.fb_size, calculated_size);

    // Zero dimensions (edge case)
    let zero_display = DisplayInfo {
        width: 0,
        height: 0,
        fb_base_vaddr: 0,
        fb_size: 0,
    };

    assert_eq!(zero_display.width, 0);
    assert_eq!(zero_display.height, 0);
    assert_eq!(zero_display.fb_size, 0);
}

#[def_test]
fn test_display_info_memory_alignment_validation() {
    // Test various memory alignment scenarios for framebuffer addresses

    let alignments = [
        0x1000,    // 4KB aligned
        0x10000,   // 64KB aligned
        0x100000,  // 1MB aligned
        0x1000000, // 16MB aligned
    ];

    for &base_addr in &alignments {
        let display = DisplayInfo {
            width: 1920,
            height: 1080,
            fb_base_vaddr: base_addr,
            fb_size: 1920 * 1080 * 4,
        };

        // Verify address alignment
        assert_eq!(
            display.fb_base_vaddr % 0x1000,
            0,
            "Address should be 4KB aligned"
        );
        assert!(
            display.fb_base_vaddr >= 0x1000,
            "Address should be non-zero and reasonable"
        );

        // Test framebuffer size is reasonable for given dimensions
        let expected_min_size = (display.width * display.height) as usize;
        assert!(
            display.fb_size >= expected_min_size,
            "Framebuffer size too small"
        );

        // Common pixel formats: 1, 2, 3, 4 bytes per pixel
        let common_bpp = [1, 2, 3, 4];
        let is_valid_bpp = common_bpp
            .iter()
            .any(|&bpp| display.fb_size == (display.width * display.height * bpp) as usize);
        assert!(
            is_valid_bpp || display.fb_size >= expected_min_size * 4,
            "Framebuffer size should match common pixel formats"
        );
    }
}

// ============================================================================
// FrameBuffer Tests
// ============================================================================

#[def_test]
fn test_framebuffer_creation_and_access_patterns() {
    // Test different framebuffer creation methods and access patterns

    // Create test data
    let mut test_data = vec![0u8; 1920 * 1080 * 4]; // 1080p RGBA buffer

    // Fill with test pattern
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = (i % 256) as u8;
    }

    // Create framebuffer from slice
    let _fb = FrameBuffer::from_slice(&mut test_data);

    // Test that framebuffer wraps the data correctly
    // (We can't directly access _raw due to privacy, but we can test behavior)

    // Test with different sizes
    let sizes = [
        4,               // Single pixel
        1920 * 4,        // Single row
        1920 * 1080 * 4, // Full HD
        3840 * 2160 * 4, // 4K
    ];

    for &size in &sizes {
        let mut buffer = vec![0u8; size];

        // Initialize with pattern
        for (i, byte) in buffer.iter_mut().enumerate() {
            *byte = ((i + 0xAA) % 256) as u8;
        }

        let _fb = FrameBuffer::from_slice(&mut buffer);

        // Verify the original buffer still contains our pattern
        for (i, &byte) in buffer.iter().enumerate() {
            assert_eq!(
                byte,
                ((i + 0xAA) % 256) as u8,
                "Buffer corruption at index {}",
                i
            );
        }
    }
}

#[def_test]
fn test_framebuffer_unsafe_operations_and_boundaries() {
    // Test unsafe framebuffer creation and boundary conditions

    let test_sizes = [
        1,               // Minimum size
        4096,            // Page size
        1920 * 1080 * 4, // Common resolution
        0,               // Edge case: zero size
    ];

    for &size in &test_sizes {
        if size == 0 {
            // Test zero-size buffer (edge case)
            unsafe {
                let ptr = core::ptr::NonNull::dangling().as_ptr();
                let _fb = FrameBuffer::from_raw_parts_mut(ptr, 0);
                // Should not crash on creation with zero size
            }
            continue;
        }

        // Allocate aligned memory for testing
        let mut buffer = vec![0u8; size];
        let ptr = buffer.as_mut_ptr();

        // Fill with test pattern
        for (i, byte) in buffer.iter_mut().enumerate() {
            *byte = (i % 255 + 1) as u8; // Avoid zeros for better testing
        }

        unsafe {
            let _fb = FrameBuffer::from_raw_parts_mut(ptr, size);
        }

        // Verify buffer integrity after framebuffer creation
        for (i, &byte) in buffer.iter().enumerate() {
            assert_eq!(
                byte,
                (i % 255 + 1) as u8,
                "Memory corruption at index {}",
                i
            );
        }

        // Test boundary access patterns
        if size >= 4 {
            // Test first and last 4 bytes
            assert!(buffer[0] != 0);
            assert!(buffer[size - 1] != 0);

            // Test pattern consistency across boundaries
            if size >= 8 {
                let first_pattern = &buffer[0..4];
                let last_pattern = &buffer[size - 4..size];

                // Patterns should be different (unless size is very specific)
                assert!(
                    first_pattern != last_pattern || size == 8,
                    "Unexpected pattern repetition"
                );
            }
        }
    }
}

// ============================================================================
// Integration Tests
// ============================================================================

#[def_test]
fn test_display_framebuffer_integration() {
    // Test DisplayInfo and FrameBuffer working together

    let resolutions = [
        (640, 480),   // VGA
        (1280, 720),  // HD
        (1920, 1080), // Full HD
        (2560, 1440), // QHD
    ];

    let bytes_per_pixel = [1, 2, 3, 4]; // Various pixel formats

    for &(width, height) in &resolutions {
        for &bpp in &bytes_per_pixel {
            let fb_size = (width * height * bpp) as usize;

            let display_info = DisplayInfo {
                width,
                height,
                fb_base_vaddr: 0x10000000,
                fb_size,
            };

            // Create corresponding framebuffer
            let mut buffer = vec![0u8; fb_size];
            let _fb = FrameBuffer::from_slice(&mut buffer);

            // Verify size calculations
            assert_eq!(display_info.fb_size, buffer.len());
            assert_eq!(display_info.width, width);
            assert_eq!(display_info.height, height);

            // Test pixel addressing calculations
            let total_pixels = (width * height) as usize;
            assert_eq!(display_info.fb_size, total_pixels * bpp as usize);

            // Simulate pixel operations
            if bpp >= 3 {
                // RGB pixel at position (100, 100) if resolution allows
                if width > 100 && height > 100 {
                    let pixel_offset = ((100 * width + 100) * bpp) as usize;
                    if pixel_offset + 2 < buffer.len() {
                        buffer[pixel_offset] = 0xFF; // Red
                        buffer[pixel_offset + 1] = 0x80; // Green
                        buffer[pixel_offset + 2] = 0x40; // Blue

                        // Verify pixel was set
                        assert_eq!(buffer[pixel_offset], 0xFF);
                        assert_eq!(buffer[pixel_offset + 1], 0x80);
                        assert_eq!(buffer[pixel_offset + 2], 0x40);
                    }
                }
            }
        }
    }
}
