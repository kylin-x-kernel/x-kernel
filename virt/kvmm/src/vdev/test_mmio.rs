// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Test MMIO device for selftest data-abort verification.

use crate::mm::mmio::MmioDevice;

pub const TEST_MMIO_GPA: u64 = 0x1000_0000;
pub const TEST_MMIO_SIZE: u64 = 0x1000;

pub struct TestMmioDevice;

impl MmioDevice for TestMmioDevice {
    fn mmio_range(&self) -> (u64, u64) {
        (TEST_MMIO_GPA, TEST_MMIO_SIZE)
    }

    fn read(&self, offset: u64, _size: u8) -> u64 {
        log::info!("[test-mmio] read offset={:#x}", offset);
        0xDEAD_BEEF
    }

    fn write(&mut self, offset: u64, _size: u8, value: u64) {
        log::info!("[test-mmio] write offset={:#x} value={:#x}", offset, value);
    }
}
