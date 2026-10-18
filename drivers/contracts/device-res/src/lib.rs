// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS-agnostic device resource model and capability abstraction.
//!
//! This crate describes the hardware resources a driver consumes (MMIO
//! regions, I/O port ranges, interrupts, coherent/streaming DMA buffers, and
//! ordinary aligned CPU storage and small capabilities such as a monotonic clock)
//! without binding to any particular kernel. A host kernel provides capability
//! implementations — [`MmioOp`], [`DmaOp`], [`CpuMemoryOp`], [`IrqOp`], [`TimeOp`] — to its
//! driver framework; framework code then acquires resources through RAII
//! handles ([`Io`], [`Irq`], [`DmaCoherent`], [`CpuBuffer`]) or the device-managed `devm_*`
//! helpers.
//!
//! Keeping the OS-semantic operations (map/unmap, request/release IRQ,
//! allocate/free DMA) behind per-resource-type traits isolates driver code
//! from the host kernel: porting a driver to another kernel only requires
//! implementing the providers, not rewriting the driver.
#![no_std]

extern crate alloc;

mod dma;
mod irq;
mod memory;
mod mmio;
mod provider;
mod time;

use alloc::boxed::Box;

/// A single resource associated with a device.
#[derive(Debug, Clone, Copy)]
pub enum ResourceDesc {
    /// Memory-mapped I/O region.
    Mmio(crate::mmio::MmioRegion),
    /// x86 I/O port range.
    IoPort(crate::mmio::IoPortRange),
    /// Interrupt line.
    Irq(crate::irq::IrqResource),
    /// Coherent DMA buffer request.
    Dma(crate::dma::DmaSpec),
}

/// A collection of resources for a single device.
pub type ResourceSet = smallvec::SmallVec<[ResourceDesc; 4]>;

/// Error returned by resource acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResError {
    /// The resource descriptor was invalid (e.g. zero-sized region).
    InvalidResource,
    /// The host failed to establish the requested mapping.
    MappingFailed,
    /// The host ran out of memory while acquiring the resource.
    NoMemory,
    /// The resource is already in use and cannot be shared.
    Busy,
    /// The host does not support this kind of resource.
    Unsupported,
}

/// Result type for resource operations.
pub type ResResult<T = ()> = Result<T, ResError>;

/// OS-agnostic view of a device that a driver binds resources to.
///
/// A host kernel implements this for its concrete device object so the
/// device-managed (`devm_*`) helpers and driver code can read the discovered
/// resources and attach resource teardown without depending on a kernel type.
/// Drivers can therefore be written against `&dyn DeviceResource` and ported
/// by reimplementing only the host adapter and the three capability providers.
pub trait DeviceResource {
    /// The hardware resources discovered for this device.
    fn resources(&self) -> &[ResourceDesc];

    /// Register a teardown callback bound to the device's lifetime.
    ///
    /// Callbacks run in reverse (LIFO) order when the device's probe fails or
    /// when the device is removed.
    fn register_cleanup(&self, cleanup: Box<dyn FnOnce() + Send>);
}

pub use dma::*;
pub use irq::*;
pub use memory::*;
pub use mmio::*;
pub use provider::*;
pub use time::*;

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
