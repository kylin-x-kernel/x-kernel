// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel provider adapter for [`driver_os_api`].
//!
//! The OS-agnostic resource model in [`driver_os_api`] describes *what* a driver
//! needs; this crate binds those operations to X-Kernel's MMIO mapping, IRQ
//! manager, CPU and DMA allocators, and monotonic clock. For IRQs, this crate is the adapter
//! between `driver_os_api` vocabulary and `kirq`; the IRQ core does not depend on
//! devres.

#![no_std]

extern crate alloc;

mod device_dma;
mod dma;
mod irq;
mod memory;
mod mmio;
mod time;

pub use device_dma::XKernelDeviceDma;

/// X-Kernel implementation of the OS-agnostic resource provider traits.
///
/// The type is intentionally stateless. The driver framework owns the
/// long-lived instance and passes it explicitly to `driver_os_api`.
#[derive(Debug, Default)]
pub struct XKernelResourceProvider;

impl XKernelResourceProvider {
    /// Create a stateless X-Kernel resource provider.
    pub const fn new() -> Self {
        Self
    }
}
