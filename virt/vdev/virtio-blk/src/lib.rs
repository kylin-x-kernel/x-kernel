// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO MMIO block device model (ArceOS `axvirtio-blk`) and its MMIO-bus
//! binding.
//!
//! Standard MMIO register handling is delegated to
//! [`vdev_virtio::VirtioMmioState`]; this crate owns the block configuration
//! space and the request data path (read / write / flush on descriptor chains)
//! plus a pluggable [`BlockBackend`]. [`bus::VirtioBlkDevice`] adapts the model
//! to the shared [`vdev_core::MmioDevice`] bus over a [`vdev_core::GuestDma`]
//! capability, so it carries no host-VMM dependency.

#![no_std]

extern crate alloc;

mod backend;
mod block;
/// VirtIO block constants (device config offsets, request types, features).
pub mod constants;
mod mmio;

/// MMIO-bus binding and in-memory RAM-disk backend.
pub mod bus;
/// Host-side device self-test (driver-sequence simulation).
pub mod selftest;

pub use backend::BlockBackend;
pub use block::config::VirtioBlockConfig;
pub use mmio::{BlockDeviceEvent, VirtioMmioBlockDevice};
pub use vdev_virtio::{VirtioConfig, VirtioError, VirtioQueue, VirtioResult};
