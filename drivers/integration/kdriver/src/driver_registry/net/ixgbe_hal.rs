// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ptr::NonNull;

use device_res::{DmaAllocation, DmaOp, DmaSpec};
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
// * `dma_alloc` / `dma_dealloc` going through the `device_res` provider's
//   `alloc_coherent` / `free_coherent`, which return a paired
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
        let spec = DmaSpec::new(size, 8);
        match crate::resource::resource_provider().alloc_coherent(spec) {
            Ok(alloc) => (
                alloc.bus_addr as IxgbePhysAddr,
                NonNull::new(alloc.cpu_addr as *mut u8)
                    .expect("ixgbe dma allocation stored a null CPU address"),
            ),
            Err(err) => {
                log::error!("ixgbe dma_alloc failed: size={}, err={:?}", size, err);
                (0, NonNull::dangling())
            }
        }
    }

    unsafe fn dma_dealloc(paddr: IxgbePhysAddr, vaddr: NonNull<u8>, size: usize) -> i32 {
        let spec = DmaSpec::new(size, 8);
        crate::resource::resource_provider().free_coherent(DmaAllocation {
            cpu_addr: vaddr.as_ptr() as usize,
            bus_addr: paddr as u64,
            spec,
        });
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
        let deadline = ktime_types::MonotonicInstant::from_span_since_origin(
            ktime_types::TimeSpan::from_core(duration),
        );
        khal::time::busy_wait_until(deadline);
        Ok(())
    }
}
