// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common traits and types for block storage device drivers (i.e. disk).

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

// #[cfg(feature = "bcm2835-sdhci")]
// pub mod bcm2835sdhci;

#[cfg(feature = "ramdisk")]
pub mod ramdisk;

#[cfg(feature = "ramdisk-static")]
pub mod ramdisk_static;

// #[cfg(feature = "ahci")]
// pub mod ahci;
// #[cfg(feature = "sdmmc")]
// pub mod sdmmc;

#[doc(no_inline)]
pub use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};

/// Operations that require a block storage device driver to implement.
pub trait BlockDriverOps: DriverOps {
    /// The number of blocks in this storage device.
    ///
    /// The total size of the device is `num_blocks() * block_size()`.
    fn num_blocks(&self) -> u64;
    /// The size of each block in bytes.
    fn block_size(&self) -> usize;

    /// Reads blocked data from the given block.
    ///
    /// The size of the buffer may exceed the block size, in which case multiple
    /// contiguous blocks will be read.
    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult;

    /// Writes blocked data to the given block.
    ///
    /// The size of the buffer may exceed the block size, in which case multiple
    /// contiguous blocks will be written.
    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult;

    /// Flushes the device to write all pending data to the storage.
    fn flush(&mut self) -> DriverResult;
}
