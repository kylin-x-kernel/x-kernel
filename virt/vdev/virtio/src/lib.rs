// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reusable VirtIO MMIO infrastructure (ported from ArceOS `axvirtio-common`).
//!
//! Shared VirtIO device-model infrastructure: split virtqueue rings, the
//! standard VirtIO MMIO transport register set, guest-memory access
//! abstractions, constants and error types. Guest memory is reached through
//! [`vdev_core::GuestDma`] via [`DmaGuestMemory`]; concrete devices such as
//! `vdev-virtio-blk` build on top of this crate and carry no host-VMM
//! dependency of their own.

#![no_std]

extern crate alloc;

pub mod config;
pub mod constants;
mod device_type;
pub mod error;
pub mod memory;
pub mod mmio;
pub mod queue;

mod addr;
mod dma;

/// Re-export commonly used types
pub use addr::{AccessWidth, GuestPhysAddr, PhysAddr};
pub use config::VirtioConfig;
pub use constants::*;
pub use device_type::VirtioDeviceID;
pub use dma::DmaGuestMemory;
pub use error::{VirtioError, VirtioResult};
pub use memory::{AddressSpaceMemory, GuestMemory, GuestMemoryAccessor, NoGuestMemoryAccessor};
pub use mmio::state::{MmioReadOutcome, MmioWriteAction, VirtioMmioState};
pub use queue::{DescriptorChain, VirtioQueue};
