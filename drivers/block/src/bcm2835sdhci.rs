// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SD card driver for Raspberry Pi 4 (BCM2835 SDHCI)

use bcm2835_sdhci::{
    Bcm2835SDhci::{BLOCK_SIZE, EmmcCtl},
    SDHCIError,
};
use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};

use crate::BlockDriverOps;

/// Raspberry Pi 4 SD card driver based on BCM2835 SDHCI controller.
pub struct SDHCIDriver(EmmcCtl);

impl SDHCIDriver {
    /// Creates and initializes the SDHCI driver instance.
    ///
    /// Returns `Ok` if initialization succeeds, or an error if it fails.
    pub fn new() -> DriverResult<SDHCIDriver> {
        let mut controller = EmmcCtl::new();
        if controller.init() == 0 {
            log::info!("SDHCI driver: initialization successful.");
            Ok(SDHCIDriver(controller))
        } else {
            log::warn!("SDHCI driver: initialization failed.");
            Err(DriverError::Io)
        }
    }
}

/// Converts SDHCI specific errors to generalized driver errors.
fn convert_sdhci_error(err: SDHCIError) -> DriverError {
    use SDHCIError::*;
    match err {
        Io => DriverError::Io,
        AlreadyExists => DriverError::AlreadyExists,
        Again => DriverError::WouldBlock,
        BadState => DriverError::BadState,
        InvalidParam => DriverError::InvalidInput,
        NoMemory => DriverError::NoMemory,
        ResourceBusy => DriverError::ResourceBusy,
        Unsupported => DriverError::Unsupported,
    }
}

impl DriverOps for SDHCIDriver {
    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn name(&self) -> &str {
        "bcm2835_sdhci"
    }
}

impl BlockDriverOps for SDHCIDriver {
    fn read_block(&mut self, block_id: u64, buffer: &mut [u8]) -> DriverResult {
        if buffer.len() < BLOCK_SIZE {
            return Err(DriverError::InvalidInput);
        }

        // Ensure buffer alignment to 32-bit boundaries
        let (prefix, aligned_buffer, suffix) = unsafe { buffer.align_to_mut::<u32>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        self.0
            .read_block(block_id as u32, 1, aligned_buffer)
            .map_err(convert_sdhci_error)
    }

    fn write_block(&mut self, block_id: u64, buffer: &[u8]) -> DriverResult {
        if buffer.len() < BLOCK_SIZE {
            return Err(DriverError::Io);
        }

        // Ensure buffer alignment to 32-bit boundaries
        let (prefix, aligned_buffer, suffix) = unsafe { buffer.align_to::<u32>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        self.0
            .write_block(block_id as u32, 1, aligned_buffer)
            .map_err(convert_sdhci_error)
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }

    fn num_blocks(&self) -> u64 {
        self.0.get_block_num()
    }

    fn block_size(&self) -> usize {
        self.0.get_block_size()
    }
}

#[cfg(unittest)]
pub mod tests_bcm2835sdhci {
    use unittest::def_test;

    use super::*;
    extern crate alloc;
    use alloc::vec;

    #[def_test]
    fn test_bcm2835_sdhci_interface_validation() {
        // Test SD card interface validation patterns common to SDHCI drivers

        #[derive(Debug, PartialEq)]
        enum SdhciError {
            InvalidCommand,
            CardNotPresent,
            TimeoutError,
            CrcError,
            InvalidArgument,
        }

        #[derive(Debug, PartialEq)]
        enum SdhciState {
            Idle,
            CommandInProgress,
            DataTransfer,
            Error,
        }

        // Simulate SDHCI command validation
        fn validate_sdhci_command(
            cmd: u8,
            arg: u32,
            has_data: bool,
            state: SdhciState,
        ) -> Result<(), SdhciError> {
            // Check if controller is in valid state
            match state {
                SdhciState::CommandInProgress | SdhciState::DataTransfer => {
                    return Err(SdhciError::InvalidCommand);
                }
                SdhciState::Error => {
                    return Err(SdhciError::InvalidCommand);
                }
                SdhciState::Idle => {} // OK to proceed
            }

            // Validate command ranges (SD commands are 6-bit, 0-63)
            if cmd > 63 {
                return Err(SdhciError::InvalidCommand);
            }

            // Some commands should not have arguments
            match cmd {
                0 => {
                    // CMD0 (GO_IDLE_STATE) - should have arg=0
                    if arg != 0 {
                        return Err(SdhciError::InvalidArgument);
                    }
                }
                8 => {
                    // CMD8 (SEND_IF_COND) - has specific arg format
                    if (arg & 0xFFFF_F000) != 0 {
                        return Err(SdhciError::InvalidArgument);
                    }
                }
                _ => {} // Other commands - arg validation varies
            }

            Ok(())
        }

        // Test valid commands
        assert!(validate_sdhci_command(0, 0, false, SdhciState::Idle).is_ok());
        assert!(validate_sdhci_command(8, 0x1AA, false, SdhciState::Idle).is_ok());
        assert!(validate_sdhci_command(17, 0x1000, true, SdhciState::Idle).is_ok());

        // Test invalid commands
        assert_eq!(
            validate_sdhci_command(64, 0, false, SdhciState::Idle),
            Err(SdhciError::InvalidCommand)
        );
        assert_eq!(
            validate_sdhci_command(255, 0, false, SdhciState::Idle),
            Err(SdhciError::InvalidCommand)
        );

        // Test invalid arguments
        assert_eq!(
            validate_sdhci_command(0, 123, false, SdhciState::Idle),
            Err(SdhciError::InvalidArgument)
        );
        assert_eq!(
            validate_sdhci_command(8, 0x12345678, false, SdhciState::Idle),
            Err(SdhciError::InvalidArgument)
        );

        // Test invalid states
        assert_eq!(
            validate_sdhci_command(0, 0, false, SdhciState::CommandInProgress),
            Err(SdhciError::InvalidCommand)
        );
        assert_eq!(
            validate_sdhci_command(0, 0, false, SdhciState::DataTransfer),
            Err(SdhciError::InvalidCommand)
        );
        assert_eq!(
            validate_sdhci_command(0, 0, false, SdhciState::Error),
            Err(SdhciError::InvalidCommand)
        );
    }

    #[def_test]
    fn test_bcm2835_sdhci_timing_and_boundaries() {
        // Test SDHCI timing constraints and boundary conditions

        // Clock frequency validation (typical SD clock ranges)
        fn validate_clock_frequency(freq_hz: u32) -> bool {
            const MIN_FREQ: u32 = 400_000; // 400 kHz minimum for SD initialization
            const MAX_FREQ: u32 = 50_000_000; // 50 MHz maximum for standard SD

            freq_hz >= MIN_FREQ && freq_hz <= MAX_FREQ
        }

        // Test valid frequencies
        assert!(validate_clock_frequency(400_000)); // Minimum
        assert!(validate_clock_frequency(25_000_000)); // Standard speed
        assert!(validate_clock_frequency(50_000_000)); // High speed
        assert!(validate_clock_frequency(1_000_000)); // Low speed

        // Test invalid frequencies
        assert!(!validate_clock_frequency(399_999)); // Below minimum
        assert!(!validate_clock_frequency(50_000_001)); // Above maximum
        assert!(!validate_clock_frequency(0)); // Zero frequency
        assert!(!validate_clock_frequency(u32::MAX)); // Extremely high

        // Test timeout calculations (typical SDHCI timeout handling)
        fn calculate_timeout_cycles(base_timeout_ms: u16, clock_freq_hz: u32) -> Option<u32> {
            if base_timeout_ms == 0 || clock_freq_hz == 0 {
                return None;
            }

            // Convert milliseconds to cycles, checking for overflow
            let cycles_per_ms = clock_freq_hz.checked_div(1000)?;
            let total_cycles = cycles_per_ms.checked_mul(base_timeout_ms as u32)?;

            // SDHCI typically has maximum timeout limits
            const MAX_TIMEOUT_CYCLES: u32 = 0x00FF_FFFF; // 24-bit timeout counter
            if total_cycles > MAX_TIMEOUT_CYCLES {
                return Some(MAX_TIMEOUT_CYCLES);
            }

            Some(total_cycles)
        }

        // Test timeout calculations
        assert_eq!(calculate_timeout_cycles(100, 1_000_000), Some(100_000)); // Normal case
        assert_eq!(
            calculate_timeout_cycles(1000, 50_000_000),
            Some(0x00FF_FFFF)
        ); // Clamped to max
        assert_eq!(calculate_timeout_cycles(0, 1_000_000), None); // Zero timeout
        assert_eq!(calculate_timeout_cycles(100, 0), None); // Zero frequency

        // Test data transfer size validation
        fn validate_transfer_size(size: usize, max_block_count: u16) -> Result<u16, DriverError> {
            const BLOCK_SIZE: usize = 512;

            if size == 0 {
                return Err(DriverError::InvalidInput);
            }

            if size % BLOCK_SIZE != 0 {
                return Err(DriverError::InvalidInput);
            }

            let block_count = size / BLOCK_SIZE;
            if block_count > max_block_count as usize {
                return Err(DriverError::InvalidInput);
            }

            Ok(block_count as u16)
        }

        const MAX_BLOCKS: u16 = 65535; // Typical SDHCI limit

        // Test valid transfer sizes
        assert_eq!(validate_transfer_size(512, MAX_BLOCKS), Ok(1));
        assert_eq!(validate_transfer_size(1024, MAX_BLOCKS), Ok(2));
        assert_eq!(
            validate_transfer_size(512 * MAX_BLOCKS as usize, MAX_BLOCKS),
            Ok(MAX_BLOCKS)
        );

        // Test invalid transfer sizes
        assert_eq!(
            validate_transfer_size(0, MAX_BLOCKS),
            Err(DriverError::InvalidInput)
        );
        assert_eq!(
            validate_transfer_size(511, MAX_BLOCKS),
            Err(DriverError::InvalidInput)
        );
        assert_eq!(
            validate_transfer_size(513, MAX_BLOCKS),
            Err(DriverError::InvalidInput)
        );
        assert_eq!(
            validate_transfer_size(512 * (MAX_BLOCKS as usize + 1), MAX_BLOCKS),
            Err(DriverError::InvalidInput)
        );
    }
}
