// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO device probing and HAL integration.
use alloc::boxed::Box;
use core::ptr::NonNull;

use cfg_if::cfg_if;
use driver_base::DriverResult;
use virtio::{BufferDirection, PhysAddr, VirtIoHal};

use crate::iomap_mmio;

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
                Ok(Box::new(virtio::VirtIoBlkDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
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
                _irq: Option<usize>,
            ) -> DriverResult<kclass::prelude::VsockDeviceImpl> {
                Ok(Box::new(virtio::VirtIoSocketDev::<VirtIoHalImpl, T>::try_new(
                    transport,
                )?))
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

use memaddr::PAGE_SIZE_4K;
pub struct VirtIoHalImpl;

// SAFETY: `VirtIoHal` requires that DMA buffers handed to the device are
// valid for the device's view of memory and stay alive until
// `dma_dealloc` is called, that MMIO physical-to-virtual translations
// produce valid kernel mappings of the MMIO window, and that `share` /
// `unshare` keep CPU and device views of a buffer coherent.
//
// This implementation upholds those invariants by:
// * `dma_alloc` / `dma_dealloc` going through `kdma::allocate_dma_memory`
//   / `kdma::deallocate_dma_memory`, which return a paired
//   (`cpu_addr`, `bus_addr`) for the same DMA region and free the entire
//   region in one call;
// * `mmio_phys_to_virt` resolving the address through `iomap_mmio`,
//   which installs (or reuses) a kernel mapping covering the requested
//   MMIO range before returning a virtual pointer;
// * `share` / `unshare` delegating to `kdma::map_dma_buffer` /
//   `kdma::unmap_dma_buffer`, which perform the cache maintenance and
//   IOMMU bookkeeping required to keep CPU and device views in sync for
//   the requested `BufferDirection`.
unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        use core::alloc::Layout;
        let size = pages * PAGE_SIZE_4K;
        let layout = Layout::from_size_align(size, PAGE_SIZE_4K).unwrap();
        // SAFETY: `layout` is a valid non-zero layout constructed from
        // `pages * PAGE_SIZE_4K` with `PAGE_SIZE_4K` alignment. The
        // returned buffer is owned exclusively by the VirtIO transport
        // until `dma_dealloc` is called.
        match unsafe { kdma::allocate_dma_memory(layout) } {
            Ok(dma_info) => {
                // SAFETY: `dma_info.cpu_addr` was just returned by
                // `allocate_dma_memory` and points to `size` bytes of
                // valid, exclusively-owned DMA memory. Zeroing prevents
                // the device from reading stale kernel data.
                unsafe {
                    core::ptr::write_bytes(dma_info.cpu_addr.as_ptr(), 0, size);
                }
                let paddr = dma_info.bus_addr.as_u64() as PhysAddr;
                let ptr = dma_info.cpu_addr;
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
        use core::alloc::Layout;
        let size = pages * PAGE_SIZE_4K;
        let layout = Layout::from_size_align(size, PAGE_SIZE_4K).unwrap();
        let dma_info = kdma::DMAInfo {
            cpu_addr: vaddr,
            bus_addr: kdma::DmaBusAddress::new(paddr),
        };
        // SAFETY: `dma_info` and `layout` describe a coherent buffer
        // previously returned by `dma_alloc` with the same `pages`.
        // The caller (VirtIO transport) guarantees the buffer is no
        // longer in use by the device.
        unsafe { kdma::deallocate_dma_memory(dma_info, layout) };
        0
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        // SAFETY: The caller (VirtIO transport) has confirmed the device
        // exists at `paddr` before calling this function. `iomap_mmio`
        // delegates to `memspace::iomap_device` which validates the
        // physical address range.
        iomap_mmio(paddr as usize, size, "virtio-mmio-hal")
            .expect("failed to iomap virtio MMIO region")
    }

    #[allow(unused_variables)]
    #[inline]
    unsafe fn share(
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        _access_platform: bool,
    ) -> PhysAddr {
        // SAFETY: `buffer` is a valid DMA buffer allocated by the VirtIO
        // transport layer (via `dma_alloc` or an upper-layer provider).
        // `kdma::map_dma_buffer` performs the necessary cache maintenance
        // and IOMMU mapping to make the buffer visible to the device.
        unsafe { kdma::map_dma_buffer(buffer, dma_direction(direction)) }
            .expect("failed to map shared DMA buffer via kdma")
            .bus_addr
            .as_u64() as PhysAddr
    }

    #[inline]
    #[allow(unused_variables)]
    unsafe fn unshare(
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        _access_platform: bool,
    ) {
        // SAFETY: `paddr` and `buffer` are the same values returned by
        // a previous `share` call on this buffer. `kdma::unmap_dma_buffer`
        // reverses the cache maintenance and IOMMU mapping performed by
        // `map_dma_buffer`. The caller (VirtIO transport) guarantees the
        // device is no longer accessing the buffer.
        unsafe {
            kdma::unmap_dma_buffer(
                kdma::DmaBusAddress::new(paddr),
                buffer,
                dma_direction(direction),
            )
        };
    }
}

const fn dma_direction(direction: BufferDirection) -> kdma::DmaDirection {
    match direction {
        BufferDirection::DriverToDevice => kdma::DmaDirection::DriverToDevice,
        BufferDirection::DeviceToDriver => kdma::DmaDirection::DeviceToDriver,
        BufferDirection::Both => kdma::DmaDirection::Bidirectional,
    }
}
