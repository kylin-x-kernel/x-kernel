// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A RAM disk driver backed by heap-allocated memory.

extern crate alloc;

use alloc::{boxed::Box, vec};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoPreempt;

use crate::BlockDeviceOperations;

const BLOCK_SIZE: usize = 512;

/// A RAM disk structure backed by heap memory.
///
/// The `SpinNoPreempt` lock serializes block reads and writes to the backing
/// storage so the device can be shared behind a `&self` interface. Capacity is
/// fixed at construction time and cached separately, so metadata queries do not
/// need to take the data lock.
pub struct RamDisk {
    storage: SpinNoPreempt<Box<[u8]>>,
    len: usize,
}

impl Default for RamDisk {
    fn default() -> Self {
        Self {
            storage: SpinNoPreempt::new(Box::<[u8]>::default()),
            len: 0,
        }
    }
}

impl RamDisk {
    /// Creates a new RAM disk with the specified size hint.
    ///
    /// The size is rounded up to be aligned to the block size (512 bytes).
    pub fn new(size_hint: usize) -> Self {
        let size = align_up(size_hint);
        Self {
            storage: SpinNoPreempt::new(vec![0u8; size].into_boxed_slice()),
            len: size,
        }
    }
}

impl From<&[u8]> for RamDisk {
    fn from(data: &[u8]) -> Self {
        let ramdisk = RamDisk::new(data.len());
        {
            let mut storage = ramdisk.storage.lock();
            storage[..data.len()].copy_from_slice(data);
        }
        ramdisk
    }
}

impl Device for RamDisk {
    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn name(&self) -> &str {
        "ramdisk"
    }
}

impl BlockDeviceOperations for RamDisk {
    #[inline]
    fn num_blocks(&self) -> u64 {
        (self.len / BLOCK_SIZE) as u64
    }

    #[inline]
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        if !buf.len().is_multiple_of(BLOCK_SIZE) {
            return Err(DriverError::InvalidInput);
        }
        let offset = block_id as usize * BLOCK_SIZE;
        if offset + buf.len() > self.len {
            return Err(DriverError::Io);
        }
        let storage = self.storage.lock();
        buf.copy_from_slice(&storage[offset..offset + buf.len()]);
        Ok(())
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        if !buf.len().is_multiple_of(BLOCK_SIZE) {
            return Err(DriverError::InvalidInput);
        }
        let offset = block_id as usize * BLOCK_SIZE;
        if offset + buf.len() > self.len {
            return Err(DriverError::Io);
        }
        let mut storage = self.storage.lock();
        storage[offset..offset + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

/// Aligns a given size upwards to the nearest multiple of `BLOCK_SIZE`.
const fn align_up(val: usize) -> usize {
    (val + BLOCK_SIZE - 1) & !(BLOCK_SIZE - 1)
}

unittest::path_mod_test!(tests_ramdisk, "../ut-tests/ramdisk.rs");
