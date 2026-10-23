// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A RAM disk driver implemented with a static slice as storage.

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoPreempt;

use crate::BlockDeviceOperations;

const BLOCK_SIZE: usize = 512;

/// RAM disk structure backed by a static mutable slice.
///
/// The `SpinNoPreempt` lock serializes all block reads and writes so the
/// device can be shared behind a `&self` interface. `SpinNoPreempt` is
/// sufficient because this is a pure-software driver with no IRQ interaction.
/// `base_addr`/`len` describe the backing allocation without storing a global
/// `&'static mut [u8]`; the lock serializes temporary mutable slice
/// reconstruction for each I/O operation.
pub struct RamDisk {
    lock: SpinNoPreempt<()>,
    base_addr: usize,
    len: usize,
}

impl Default for RamDisk {
    fn default() -> Self {
        RamDisk {
            lock: SpinNoPreempt::new(()),
            base_addr: [0u8; 0].as_mut_ptr().addr(),
            len: 0,
        }
    }
}

impl RamDisk {
    /// Constructs a new RAM disk from the provided static buffer.
    ///
    /// # Panics
    /// Panics if the buffer is not aligned to the block size or its size is
    /// not a multiple of the block size.
    pub fn new(buffer: &'static mut [u8]) -> Self {
        assert_eq!(
            buffer.as_ptr().addr() & (BLOCK_SIZE - 1),
            0,
            "Buffer not aligned to block size."
        );
        assert_eq!(
            buffer.len() % BLOCK_SIZE,
            0,
            "Buffer size is not a multiple of block size."
        );
        let len = buffer.len();
        RamDisk {
            lock: SpinNoPreempt::new(()),
            base_addr: buffer.as_mut_ptr().addr(),
            len,
        }
    }

    fn with_data<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let _guard = self.lock.lock();
        // SAFETY: `base_addr..base_addr+len` is the original static backing
        // buffer provided at construction. The spinlock serializes all
        // temporary mutable slice reconstruction, so no two mutable borrows of
        // the same storage can coexist.
        let data = unsafe { core::slice::from_raw_parts_mut(self.base_addr as *mut u8, self.len) };
        f(data)
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
    /// Returns the number of blocks the RAM disk can hold.
    #[inline]
    fn num_blocks(&self) -> u64 {
        (self.len / BLOCK_SIZE) as u64
    }

    /// Returns the block size of the RAM disk.
    #[inline]
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }
}

unittest::path_mod_test!(pub mod tests_ramdisk_static, "../ut-tests/ramdisk_static.rs");

impl crate::BioOperations for RamDisk {
    fn submit_bio(&self, mut bio: crate::Bio) {
        let transfer = |data: &mut [u8]| -> DriverResult {
            let mut offset = usize::try_from(bio.sector())
                .ok()
                .and_then(|sector| sector.checked_mul(512))
                .ok_or(DriverError::InvalidInput)?;
            let len: usize = bio.vectors_mut().map(|vector| vector.bytes().len()).sum();
            if offset.checked_add(len).is_none_or(|end| end > data.len()) {
                return Err(DriverError::Io);
            }
            let operation = bio.operation();
            for mut vector in bio.vectors_mut() {
                let end = offset + vector.bytes().len();
                match operation {
                    crate::BioOperation::Read => {
                        vector.bytes_mut()?.copy_from_slice(&data[offset..end])
                    }
                    crate::BioOperation::Write => data[offset..end].copy_from_slice(vector.bytes()),
                    crate::BioOperation::Flush => unreachable!(),
                }
                offset = end;
            }
            Ok(())
        };
        let result = self.with_data(transfer);
        bio.end_io(result);
    }
}
