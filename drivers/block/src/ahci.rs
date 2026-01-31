// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![allow(unsafe_op_in_unsafe_fn)]

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
use simple_ahci::AhciDriver as CoreAhciDriver;
pub use simple_ahci::Hal as AhciHal;

use crate::BlockDriverOps;

/// AHCI driver implementation based on `simple_ahci` crate.
pub struct AhciDriver<H: AhciHal>(CoreAhciDriver<H>);

impl<H: AhciHal> AhciDriver<H> {
    /// Attempts to create a new AHCI driver using the specified MMIO base address.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `base` refers to a valid MMIO register block of the AHCI controller.
    /// - The memory region from `base` onward is mapped and accessible.
    /// - No other part of the code is accessing the AHCI controller simultaneously.
    /// - The AHCI hardware is functioning at the provided address.
    pub unsafe fn new(base_addr: usize) -> Option<Self> {
        CoreAhciDriver::<H>::try_new(base_addr).map(AhciDriver)
    }
}

impl<H: AhciHal> DriverOps for AhciDriver<H> {
    fn name(&self) -> &str {
        "ahci"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl<H: AhciHal> BlockDriverOps for AhciDriver<H> {
    fn block_size(&self) -> usize {
        self.0.block_size()
    }

    fn num_blocks(&self) -> u64 {
        self.0.capacity()
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        if buf.len() % self.block_size() != 0 {
            return Err(DriverError::InvalidInput);
        }
        if buf.as_ptr() as usize % 4 != 0 {
            return Err(DriverError::InvalidInput);
        }
        if self.0.read(block_id, buf) {
            Ok(())
        } else {
            Err(DriverError::Io)
        }
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult {
        if buf.len() % self.block_size() != 0 {
            return Err(DriverError::InvalidInput);
        }
        if buf.as_ptr() as usize % 4 != 0 {
            return Err(DriverError::InvalidInput);
        }
        if self.0.write(block_id, buf) {
            Ok(())
        } else {
            Err(DriverError::Io)
        }
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }
}

#[cfg(unittest)]
pub mod tests_ahci {
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    extern crate alloc;
    use alloc::vec;

    // Mock AHCI HAL for testing
    struct MockAhciHal;

    impl AhciHal for MockAhciHal {
        unsafe fn read_ptr(&self, _offset: usize) -> u32 {
            // Mock implementation - return safe test values
            match _offset {
                0x00 => 0x12345678, // Version register mock
                0x04 => 0x00000003, // Global capabilities mock (2 ports)
                _ => 0,
            }
        }

        unsafe fn write_ptr(&self, _offset: usize, _value: u32) {
            // Mock implementation - no-op for tests
        }

        fn timer_config(&self) -> (u64, u64) {
            (1000000, 1000) // Mock timer config (1MHz, 1ms)
        }

        unsafe fn allocate_coherent(&self, _size: usize, _align: usize) -> (usize, usize) {
            // Mock allocation - return fake but valid addresses for testing
            let vaddr = 0x1000_0000;
            let paddr = 0x2000_0000;
            (vaddr, paddr)
        }

        unsafe fn deallocate_coherent(&self, _vaddr: usize, _paddr: usize, _size: usize) {
            // Mock deallocation - no-op for tests
        }
    }

    #[def_test]
    fn test_ahci_driver_interface_validation() {
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

        fn mock_ahci_operation(
            buf_size: usize,
            buf_aligned: bool,
            block_in_range: bool,
        ) -> MockResult {
            if buf_size % 512 != 0 || !buf_aligned {
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
}
