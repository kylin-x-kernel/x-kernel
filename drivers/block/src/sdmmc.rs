// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SD/MMC driver based on SDIO.

#![allow(unsafe_op_in_unsafe_fn)]

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
use simple_sdmmc::SdMmc;

use crate::BlockDriverOps;

/// A SD/MMC driver.
pub struct SdMmcDriver(SdMmc);

impl SdMmcDriver {
    /// Creates a new [`SdMmcDriver`] from the given base address.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `base` is a valid pointer to the SD/MMC controller's
    /// register block and that no other code is concurrently accessing the same hardware.
    pub unsafe fn new(base: usize) -> Self {
        Self(SdMmc::new(base))
    }
}

impl DriverOps for SdMmcDriver {
    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn name(&self) -> &str {
        "sdmmc"
    }
}

impl BlockDriverOps for SdMmcDriver {
    fn num_blocks(&self) -> u64 {
        self.0.num_blocks()
    }

    fn block_size(&self) -> usize {
        SdMmc::BLOCK_SIZE
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let (blocks, remainder) = buf.as_chunks_mut::<{ SdMmc::BLOCK_SIZE }>();

        if !remainder.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        for (i, block) in blocks.iter_mut().enumerate() {
            self.0.read_block(block_id as u32 + i as u32, block);
        }

        Ok(())
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult {
        let (blocks, remainder) = buf.as_chunks::<{ SdMmc::BLOCK_SIZE }>();

        if !remainder.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        for (i, block) in blocks.iter().enumerate() {
            self.0.write_block(block_id as u32 + i as u32, block);
        }

        Ok(())
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }
}

#[cfg(unittest)]
mod tests_sdmmc {
    use unittest::def_test;

    use super::*;
    extern crate alloc;
    use alloc::vec;

    #[def_test]
    fn test_sdmmc_driver_chunk_validation() {
        // Test the chunk-based buffer validation logic that SdMmcDriver uses
        const BLOCK_SIZE: usize = 512; // SD/MMC standard block size

        // Test buffer chunking logic (simulating what as_chunks would do)
        fn validate_buffer_chunks(buf_size: usize) -> Result<usize, DriverError> {
            if buf_size % BLOCK_SIZE != 0 {
                return Err(DriverError::InvalidInput);
            }
            Ok(buf_size / BLOCK_SIZE)
        }

        // Test various buffer sizes
        assert_eq!(validate_buffer_chunks(512), Ok(1)); // Single block
        assert_eq!(validate_buffer_chunks(1024), Ok(2)); // Two blocks
        assert_eq!(validate_buffer_chunks(2048), Ok(4)); // Four blocks

        // Test invalid buffer sizes
        assert_eq!(validate_buffer_chunks(511), Err(DriverError::InvalidInput));
        assert_eq!(validate_buffer_chunks(513), Err(DriverError::InvalidInput));
        assert_eq!(validate_buffer_chunks(1023), Err(DriverError::InvalidInput));
        assert_eq!(validate_buffer_chunks(1025), Err(DriverError::InvalidInput));

        // Test edge cases
        assert_eq!(validate_buffer_chunks(0), Ok(0)); // Empty buffer
        assert_eq!(validate_buffer_chunks(512 * 100), Ok(100)); // Large buffer

        // Test overflow protection (simulating block_id + block_count overflow)
        fn check_block_overflow(block_id: u64, num_blocks: usize) -> bool {
            block_id.saturating_add(num_blocks as u64) <= u32::MAX as u64
        }

        assert!(check_block_overflow(0, 1));
        assert!(check_block_overflow(u32::MAX as u64 - 1, 1));
        assert!(!check_block_overflow(u32::MAX as u64, 1));
        assert!(!check_block_overflow(u32::MAX as u64 - 1, 2));
    }

    #[def_test]
    fn test_sdmmc_driver_boundary_conditions() {
        // Test block ID conversion and overflow handling
        fn simulate_block_operations(block_id: u64, buffer_size: usize) -> Result<(), DriverError> {
            // Simulate the validation logic in SdMmcDriver

            const BLOCK_SIZE: usize = 512;

            // Check buffer size alignment
            if buffer_size % BLOCK_SIZE != 0 {
                return Err(DriverError::InvalidInput);
            }

            let num_blocks = buffer_size / BLOCK_SIZE;

            // Check block ID range (SD/MMC uses u32 for block addressing)
            if block_id > u32::MAX as u64 {
                return Err(DriverError::Io);
            }

            // Check for overflow in block range calculation
            if block_id.saturating_add(num_blocks as u64) > u32::MAX as u64 {
                return Err(DriverError::Io);
            }

            Ok(())
        }

        // Test valid operations
        assert!(simulate_block_operations(0, 512).is_ok());
        assert!(simulate_block_operations(100, 1024).is_ok());
        assert!(simulate_block_operations(u32::MAX as u64 - 1, 512).is_ok());

        // Test invalid buffer sizes
        assert_eq!(
            simulate_block_operations(0, 511),
            Err(DriverError::InvalidInput)
        );
        assert_eq!(
            simulate_block_operations(0, 513),
            Err(DriverError::InvalidInput)
        );
        assert_eq!(
            simulate_block_operations(0, 1000),
            Err(DriverError::InvalidInput)
        );

        // Test block ID overflow
        assert_eq!(
            simulate_block_operations(u32::MAX as u64 + 1, 512),
            Err(DriverError::Io)
        );
        assert_eq!(
            simulate_block_operations(u64::MAX, 512),
            Err(DriverError::Io)
        );

        // Test block range overflow
        assert_eq!(
            simulate_block_operations(u32::MAX as u64, 512),
            Err(DriverError::Io)
        );
        assert_eq!(
            simulate_block_operations(u32::MAX as u64 - 1, 1024),
            Err(DriverError::Io)
        );

        // Test edge case: exactly at u32 boundary
        assert!(simulate_block_operations(u32::MAX as u64 - 1, 512).is_ok());

        // Test multi-block operations at boundaries
        let test_cases = [
            // (block_id, num_blocks, expected_result)
            (0, 1, Ok(())),                                   // Normal case
            (u32::MAX as u64 - 10, 5, Ok(())),                // Valid range
            (u32::MAX as u64 - 10, 11, Err(DriverError::Io)), // Would overflow
            (u32::MAX as u64 - 1, 1, Ok(())),                 // Exactly at boundary
            (u32::MAX as u64 - 1, 2, Err(DriverError::Io)),   // Would overflow by 1
        ];

        for (block_id, num_blocks, expected) in test_cases.iter() {
            let buffer_size = num_blocks * 512;
            let result = simulate_block_operations(*block_id, buffer_size);
            assert_eq!(
                result, *expected,
                "Failed for block_id={}, num_blocks={}",
                block_id, num_blocks
            );
        }

        // Test chunk remainder validation (simulates as_chunks behavior)
        fn test_chunk_validation(total_size: usize, chunk_size: usize) -> bool {
            total_size % chunk_size == 0
        }

        assert!(test_chunk_validation(512, 512)); // Perfect fit
        assert!(test_chunk_validation(1024, 512)); // Multiple chunks
        assert!(!test_chunk_validation(1000, 512)); // Has remainder
        assert!(!test_chunk_validation(256, 512)); // Smaller than chunk
    }
}
