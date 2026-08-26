// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO device probing and HAL integration.
use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

use cfg_if::cfg_if;
use device_res::{
    DmaAllocation, DmaDirection, DmaMapping, DmaSpec, MmioRegion, dma_provider, mmio_provider,
    try_dma_provider,
};
use driver_base::{Device, DriverResult};
use virtio::{BufferDirection, PhysAddr, VirtIoHal};

cfg_if! {
    if #[cfg(feature = "virtio-net")] {
        pub struct VirtIoNet;

        impl VirtIoNet {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::NetDeviceImpl> {
                Ok(Box::new(virtio::VirtIoNetDev::<VirtIoHalImpl, T, 64>::try_new(
                    transport,
                    irq,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-blk")] {
        pub struct VirtIoBlk;

        impl VirtIoBlk {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::BlockDeviceImpl> {
                let device = virtio::VirtIoBlkDev::<VirtIoHalImpl, T>::try_new(transport)?;
                let name = device.name().into();
                let first_minor = device.index() << virtio::VIRTIO_BLK_PART_BITS;
                let disk = block::Gendisk::new(
                    name,
                    virtio::VIRTIO_BLK_MAJOR,
                    first_minor,
                    1 << virtio::VIRTIO_BLK_PART_BITS,
                    Box::new(device),
                )?;
                Ok(Arc::new(disk))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-gpu")] {
        pub struct VirtIoGpu;

        impl VirtIoGpu {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::DisplayDeviceImpl> {
                Ok(Box::new(virtio::VirtIoGpuDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-input")] {
        pub struct VirtIoInput;

        impl VirtIoInput {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::InputDeviceImpl> {
                Ok(Box::new(virtio::VirtIoInputDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-socket")] {
        pub struct VirtIoSocket;

        impl VirtIoSocket {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::VsockDeviceImpl> {
                Ok(Box::new(virtio::VirtIoVsockDev::<
                    VirtIoHalImpl,
                    T,
                >::try_new(transport, irq)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-9p")] {
        pub struct VirtIo9p;

        impl VirtIo9p {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::Virtio9pDeviceImpl> {
                Ok(Box::new(virtio::VirtIo9pDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(feature = "virtio-rng")] {
        pub struct VirtIoRng;

        impl VirtIoRng {
            pub fn try_new<T: virtio::Transport + 'static>(
                transport: T,
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::CharDeviceImpl> {
                Ok(Box::new(virtio::VirtIoRngDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
            }
        }
    }
}

use memaddr::PAGE_SIZE_4K;
pub struct VirtIoHalImpl;

// SAFETY: `VirtIoHal` requires that DMA buffers handed to the device are
// valid for the device's view of memory and stay alive until `dma_dealloc`
// is called, that MMIO physical-to-virtual translations produce valid kernel
// mappings of the MMIO window, and that `share` / `unshare` keep CPU and
// device views of a buffer coherent.
//
// This implementation upholds those invariants by routing every operation
// through the `device_res` provider rather than x-kernel APIs directly:
// * `dma_alloc` / `dma_dealloc` use `alloc_coherent` / `free_coherent`, which
//   return a paired (`cpu_addr`, `bus_addr`) for the same region and free it
//   in one call;
// * `mmio_phys_to_virt` uses `map_mmio`, which installs (or reuses) a kernel
//   mapping covering the requested MMIO range before returning a virtual
//   pointer;
// * `share` / `unshare` use `map_streaming` / `unmap_streaming`, which perform
//   the cache maintenance and IOMMU bookkeeping required to keep CPU and
//   device views in sync for the requested `BufferDirection`.
const fn hal_direction(direction: BufferDirection) -> DmaDirection {
    match direction {
        BufferDirection::DriverToDevice => DmaDirection::DriverToDevice,
        BufferDirection::DeviceToDriver => DmaDirection::DeviceToDriver,
        BufferDirection::Both => DmaDirection::Bidirectional,
    }
}

// SAFETY: every method routes hardware access through the `device_res`
// provider, so the HAL holds no x-kernel state; DMA / MMIO ownership
// invariants are delegated to the installed provider.
unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        let size = pages * PAGE_SIZE_4K;
        let spec = DmaSpec::new(size, PAGE_SIZE_4K);
        match dma_provider().and_then(|p| p.alloc_coherent(spec)) {
            Ok(alloc) => {
                let cpu_ptr = alloc.cpu_addr as *mut u8;
                // SAFETY: `alloc.cpu_addr` was just returned by the provider and
                // points to `size` bytes of valid, exclusively-owned DMA memory.
                // Zeroing prevents the device from reading stale kernel data.
                unsafe { core::ptr::write_bytes(cpu_ptr, 0, size) };
                // Ownership of the allocation is handed to the virtio transport
                // and returned via `dma_dealloc`. We deliberately do NOT wrap it
                // in `DmaCoherent` (whose Drop would free it on return).
                let paddr = alloc.bus_addr as PhysAddr;
                let ptr = NonNull::new(cpu_ptr).expect("dma allocation stored a null CPU address");
                (paddr, ptr)
            }
            Err(err) => {
                log::error!("dma_alloc failed: pages={}, error={:?}", pages, err);
                (0, NonNull::dangling())
            }
        }
    }

    #[allow(unused_variables)]
    unsafe fn dma_dealloc(
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        _access_platform: bool,
    ) -> i32 {
        let spec = DmaSpec::new(pages * PAGE_SIZE_4K, PAGE_SIZE_4K);
        if let Some(p) = try_dma_provider() {
            p.free_coherent(DmaAllocation {
                cpu_addr: vaddr.as_ptr() as usize,
                bus_addr: paddr,
                spec,
            });
        }
        0
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        let region = MmioRegion {
            base: paddr as usize,
            size,
        };
        match mmio_provider().and_then(|p| p.map_mmio(region, "virtio-hal")) {
            Ok(m) => NonNull::new(m.vaddr as *mut u8)
                .expect("virtio mmio mapping stored a null virtual address"),
            Err(err) => {
                log::error!(
                    "virtio mmio_phys_to_virt failed: paddr={:#x}, size={:#x}, err={:?}",
                    paddr,
                    size,
                    err
                );
                panic!("virtio mmio_phys_to_virt failed")
            }
        }
    }

    #[allow(unused_variables)]
    #[inline]
    unsafe fn share(
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        _access_platform: bool,
    ) -> PhysAddr {
        match dma_provider().and_then(|p| p.map_streaming(buffer, hal_direction(direction))) {
            Ok(m) => m.bus_addr as PhysAddr,
            Err(err) => {
                log::error!("virtio share failed: err={:?}", err);
                0
            }
        }
    }

    #[inline]
    #[allow(unused_variables)]
    unsafe fn unshare(
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        _access_platform: bool,
    ) {
        if paddr == 0 {
            return;
        }

        let dir = hal_direction(direction);
        let Some(p) = try_dma_provider() else {
            return;
        };
        // SAFETY: `buffer` is valid (caller contract); `paddr` / `buffer` come
        // from a prior `share` call, so reconstructing the mapping reverses it.
        let slice = unsafe { buffer.as_ref() };
        let cpu_addr = NonNull::from(slice).cast::<u8>();
        p.unmap_streaming(DmaMapping {
            cpu_addr: cpu_addr.as_ptr() as usize,
            bus_addr: paddr,
            len: slice.len(),
            direction: dir,
        });
    }
}

#[cfg(unittest)]
mod tests {
    use core::ptr::NonNull;

    use unittest::def_test;
    use virtio::{BufferDirection, VirtIoHal};

    use super::VirtIoHalImpl;

    #[def_test(serial)]
    fn unshare_ignores_zero_streaming_dma_mapping() {
        let mut byte = 0u8;
        let buffer = NonNull::slice_from_raw_parts(NonNull::from(&mut byte), 1);

        // SAFETY: this mirrors the virtio-drivers cleanup path after a prior
        // share failure returned the legacy zero sentinel.
        unsafe {
            VirtIoHalImpl::unshare(0, buffer, BufferDirection::DeviceToDriver, false);
        }
    }
}
