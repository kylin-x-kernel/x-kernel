// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel provider adapter for [`device_res`].
//!
//! The OS-agnostic resource model in [`device_res`] describes *what* a driver
//! needs; this crate binds those operations to X-Kernel's MMIO mapping, IRQ
//! manager, coherent DMA allocator, and monotonic clock. For IRQs, this crate is the adapter
//! between `device_res` vocabulary and `kirq`; the IRQ core does not depend on
//! devres.

#![no_std]

extern crate alloc;

mod dma;
mod irq;
mod mmio;
mod time;

/// X-Kernel implementation of the OS-agnostic resource provider traits.
///
/// The type is intentionally stateless. The driver framework owns the
/// long-lived instance and passes it explicitly to `device_res`.
#[derive(Debug, Default)]
pub struct XKernelResourceProvider;

impl XKernelResourceProvider {
    /// Create a stateless X-Kernel resource provider.
    pub const fn new() -> Self {
        Self
    }
}
