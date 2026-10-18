// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

// Mock AHCI HAL for testing
struct MockAhciHal;

impl AhciControllerHal for MockAhciHal {
    fn virt_to_phys(address: usize) -> usize {
        address
    }

    fn current_ms() -> u64 {
        0
    }

    fn flush_dcache() {}
}

// SAFETY: this legacy fixture makes no coherent DMA claim and cannot admit
// a hardware controller. Its original validation tables remain below.
unsafe impl AhciHal for MockAhciHal {
    const DMA_COHERENT: bool = false;
}

#[def_test]
fn test_ahci_driver_interface_validation() {
    assert!(!MockAhciHal::DMA_COHERENT);
    // Test input validation logic that would be present in a real AHCI driver

    // Test various error conditions that the AHCI driver should handle:

    // 1. Buffer alignment requirements
    let test_cases = [
        (512, 4, true),  // Valid: 512 bytes, 4-byte aligned
        (1024, 4, true), // Valid: 1024 bytes, 4-byte aligned
        (511, 4, false), // Invalid: not multiple of block size
        (513, 4, false), // Invalid: not multiple of block size
        (512, 1, false), // Invalid: not 4-byte aligned
        (512, 2, false), // Invalid: not 4-byte aligned
        (512, 8, true),  // Valid: over-aligned is OK
    ];

    for (size, alignment, should_be_valid) in test_cases.iter() {
        let is_size_valid = size % 512 == 0; // Block size validation
        let is_aligned = alignment % 4 == 0; // Alignment validation
        let expected_valid = *should_be_valid;
        let actual_valid = is_size_valid && is_aligned;

        assert_eq!(
            actual_valid, expected_valid,
            "Test case: size={}, align={}, expected={}",
            size, alignment, should_be_valid
        );
    }
}

#[def_test]
fn test_ahci_driver_boundary_conditions() {
    // Test block ID boundary checking logic
    const MAX_BLOCKS: u64 = 1000;
    let boundary_test_cases = [
        (0, 1, true),               // Valid: first block
        (MAX_BLOCKS - 1, 1, true),  // Valid: last block
        (MAX_BLOCKS, 1, false),     // Invalid: beyond capacity
        (MAX_BLOCKS + 1, 1, false), // Invalid: way beyond capacity
        (u64::MAX, 1, false),       // Invalid: maximum value
        (0, 2, true),               // Valid: two blocks from start
        (MAX_BLOCKS - 2, 2, true),  // Valid: two blocks ending at limit
        (MAX_BLOCKS - 1, 2, false), // Invalid: would exceed capacity
    ];

    for (block_id, block_count, should_be_valid) in boundary_test_cases.iter() {
        let would_fit = block_id.saturating_add(*block_count) <= MAX_BLOCKS;
        assert_eq!(
            would_fit, *should_be_valid,
            "Boundary test: block_id={}, count={}, expected={}",
            block_id, block_count, should_be_valid
        );
    }

    // Test error propagation patterns
    #[derive(Debug, PartialEq)]
    enum MockResult {
        Success,
        IoError,
        InvalidInput,
    }

    fn mock_ahci_operation(buf_size: usize, buf_aligned: bool, block_in_range: bool) -> MockResult {
        if !buf_size.is_multiple_of(512) || !buf_aligned {
            return MockResult::InvalidInput;
        }
        if !block_in_range {
            return MockResult::IoError;
        }
        MockResult::Success
    }

    // Test error precedence: InvalidInput should be caught before IoError
    assert_eq!(
        mock_ahci_operation(511, true, true),
        MockResult::InvalidInput
    );
    assert_eq!(
        mock_ahci_operation(512, false, true),
        MockResult::InvalidInput
    );
    assert_eq!(mock_ahci_operation(512, true, false), MockResult::IoError);
    assert_eq!(mock_ahci_operation(512, true, true), MockResult::Success);
}
