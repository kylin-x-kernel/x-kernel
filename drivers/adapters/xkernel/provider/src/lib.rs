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

#[cfg(target_arch = "x86_64")]
pub mod acpi_osl;
mod claim;
mod device_dma;
mod dma;
mod irq;
mod memory;
mod mmio;
mod port;
mod time;
mod wait;

pub use device_dma::XKernelDeviceDma;
/// Read-only in-use DMA resource snapshot for unit-test baselines.
#[cfg(unittest)]
pub use kdma::{DmaTestStats, dma_test_stats};

/// X-Kernel implementation of the OS-agnostic resource provider traits.
///
/// The dispatch type itself is stateless and the framework owns its long-lived
/// instance. Process-wide host arbitration that a capability requires, such as
/// physical MMIO range claims, is kept in the adapter module that implements
/// it rather than in the type.
#[derive(Debug, Default)]
pub struct XKernelResourceProvider;

impl XKernelResourceProvider {
    /// Create a stateless X-Kernel resource provider.
    pub const fn new() -> Self {
        Self
    }
}
