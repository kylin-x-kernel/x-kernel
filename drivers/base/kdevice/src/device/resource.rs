// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hardware resource descriptions owned by the device model.

pub use driver_os_api::{DmaSpec, IoPortRange, IrqResource, IrqTrigger, MmioRegion};

/// A single resource associated with a device.
#[derive(Debug, Clone, Copy)]
pub enum ResourceDesc {
    /// Memory-mapped I/O region.
    Mmio(driver_os_api::MmioRegion),
    /// x86 I/O port range.
    IoPort(driver_os_api::IoPortRange),
    /// Interrupt line.
    Irq(driver_os_api::IrqResource),
    /// Coherent DMA buffer request.
    Dma(driver_os_api::DmaSpec),
}

/// A collection of resources for a single device.
pub type ResourceSet = smallvec::SmallVec<[ResourceDesc; 4]>;
