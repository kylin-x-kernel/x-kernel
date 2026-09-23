// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Guest-memory bridge between the `vdev-core` DMA capability and the VirtIO
//! queue/MMIO data paths.

use vdev_core::GuestDma;

use crate::{GuestMemory, GuestPhysAddr, VirtioError, VirtioResult};

/// Scoped guest-memory capability backed by a `vdev-core` [`GuestDma`]
/// translation.
///
/// VMMs implement [`GuestDma`] once and reuse it for every VirtIO device; this
/// adapter exposes that translation through the VirtIO-local [`GuestMemory`]
/// view the queue and MMIO paths were written against.
pub struct DmaGuestMemory<'a> {
    dma: &'a dyn GuestDma,
}

impl<'a> DmaGuestMemory<'a> {
    /// Wrap one DMA capability for a scoped VirtIO operation.
    pub const fn new(dma: &'a dyn GuestDma) -> Self {
        Self { dma }
    }
}

impl GuestMemory for DmaGuestMemory<'_> {
    fn read(&mut self, guest_addr: GuestPhysAddr, data: &mut [u8]) -> VirtioResult<()> {
        self.dma
            .read(guest_addr.as_usize() as u64, data)
            .map_err(|_| VirtioError::InvalidAddress)
    }

    fn write(&mut self, guest_addr: GuestPhysAddr, data: &[u8]) -> VirtioResult<()> {
        self.dma
            .write(guest_addr.as_usize() as u64, data)
            .map_err(|_| VirtioError::InvalidAddress)
    }
}
