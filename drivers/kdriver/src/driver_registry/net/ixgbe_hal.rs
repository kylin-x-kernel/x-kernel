// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{alloc::Layout, ptr::NonNull};

use kdma::{DMAInfo, DmaBusAddress, allocate_dma_memory, deallocate_dma_memory};
use khal::mem::{p2v, v2p};
use net::ixgbe::{IxgbeHal, PhysAddr as IxgbePhysAddr};

/// HAL implementation for the ixgbe driver.
///
/// This is a placeholder — the `ixgbe` feature does not enable downstream
/// dependencies, so this HAL is never invoked at runtime.
pub struct IxgbeHalImpl;

// SAFETY: `IxgbeHal` requires that DMA buffers are valid for the device's
// view of memory and stay alive until `dma_dealloc`, and that
// `mmio_p2v`/`mmio_v2p` produce valid kernel mappings for the given
// physical address.
//
// This implementation upholds those invariants by:
// * `dma_alloc` / `dma_dealloc` going through `kdma::allocate_dma_memory`
//   / `kdma::deallocate_dma_memory`, which return a paired
//   (`cpu_addr`, `bus_addr`) for the same DMA region;
// * `mmio_p2v` / `mmio_v2p` delegating to `khal::mem::p2v` / `khal::mem::v2p`,
//   which assume the caller (ixgbe driver probe) has already validated the
//   physical address.
//
// The `ixgbe` feature is currently a placeholder; this HAL is not
// instantiated at runtime. The safety contract is documented here
// so that when the feature is fully enabled the invariants are clear.
unsafe impl IxgbeHal for IxgbeHalImpl {
    fn dma_alloc(size: usize) -> (IxgbePhysAddr, NonNull<u8>) {
        let layout = Layout::from_size_align(size, 8).unwrap();
        // SAFETY: `layout` is a valid non-zero layout constructed from
        // `size` with 8-byte alignment. The returned buffer is owned
        // exclusively by the caller until `dma_dealloc`.
        match unsafe { allocate_dma_memory(layout) } {
            Ok(dma_info) => (dma_info.bus_addr.as_u64() as usize, dma_info.cpu_addr),
            Err(_) => (0, NonNull::dangling()),
        }
    }

    unsafe fn dma_dealloc(paddr: IxgbePhysAddr, vaddr: NonNull<u8>, size: usize) -> i32 {
        let layout = Layout::from_size_align(size, 8).unwrap();
        let dma_info = DMAInfo {
            cpu_addr: vaddr,
            bus_addr: DmaBusAddress::from(paddr as u64),
        };
        // SAFETY: `dma_info` and `layout` describe a coherent buffer
        // previously returned by `dma_alloc` with the same `size`.
        // The caller guarantees this buffer is no longer in use by
        // the device.
        unsafe { deallocate_dma_memory(dma_info, layout) };
        0
    }

    /// # Safety
    ///
    /// `paddr` must be a valid MMIO physical address previously validated
    /// by the ixgbe driver probe path.
    unsafe fn mmio_p2v(paddr: IxgbePhysAddr, _size: usize) -> NonNull<u8> {
        // SAFETY: The caller guarantees `paddr` is a valid MMIO physical
        // address. `khal::mem::p2v` returns the kernel direct-mapped
        // virtual address for the given physical page.
        NonNull::new(p2v(paddr.into()).as_mut_ptr()).unwrap()
    }

    /// # Safety
    ///
    /// `vaddr` must be a valid kernel virtual address that maps to a
    /// device MMIO region.
    unsafe fn mmio_v2p(vaddr: NonNull<u8>, _size: usize) -> IxgbePhysAddr {
        // SAFETY: The caller guarantees `vaddr` is a valid kernel virtual
        // address. `khal::mem::v2p` performs the reverse translation.
        v2p((vaddr.as_ptr() as usize).into()).into()
    }

    fn wait_until(duration: core::time::Duration) -> Result<(), &'static str> {
        khal::time::busy_wait_until(duration);
        Ok(())
    }
}
