// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// Copyright (C) 2025 Yuekai Jia <equation618@gmail.com>
// Copyright (C) 2025 ChengXiang Qi <kuangjux@outlook.com>
// See LICENSE for license details.
//
// This file has been modified by KylinSoft on 2025.
use core::{marker::PhantomData, ptr::NonNull};
use axalloc::{UsageKind, global_allocator};
use axdriver_base::{BaseDriverOps, DevResult, DeviceType};
use axdriver_virtio::{BufferDirection, PhysAddr, VirtIoHal};
use axhal::mem::{phys_to_virt, virt_to_phys};
#[cfg(feature = "crosvm")]
use axhal::psci::{share_dma_buffer, unshare_dma_buffer};
#[cfg(feature = "sev")]
use axhal::psci::{share_dma_buffer, unshare_dma_buffer};
use cfg_if::cfg_if;

use crate::{AxDeviceEnum, drivers::DriverProbe};
cfg_if! {
    if #[cfg(bus = "pci")] {
        use axdriver_pci::{PciRoot, DeviceFunction, DeviceFunctionInfo};
        type VirtIoTransport = axdriver_virtio::PciTransport;
    } else if #[cfg(bus =  "mmio")] {
        type VirtIoTransport = axdriver_virtio::MmioTransport;
    }
}
/// A trait for VirtIO device meta information.
pub trait VirtIoDevMeta {
    const DEVICE_TYPE: DeviceType;
    type Device: BaseDriverOps;
    type Driver = VirtIoDriver<Self>;
    fn try_new(transport: VirtIoTransport, irq: Option<usize>) -> DevResult<AxDeviceEnum>;
}
cfg_if! {
    if #[cfg(net_dev = "virtio-net")] {
        pub struct VirtIoNet;
        impl VirtIoDevMeta for VirtIoNet {
            const DEVICE_TYPE: DeviceType = DeviceType::Net;
            type Device = axdriver_virtio::VirtIoNetDev<VirtIoHalImpl, VirtIoTransport, 64>;
            fn try_new(transport: VirtIoTransport, irq: Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_net(Self::Device::try_new(transport, irq)?))
            }
        }
    }
}
cfg_if! {
    if #[cfg(block_dev = "virtio-blk")] {
        pub struct VirtIoBlk;
        impl VirtIoDevMeta for VirtIoBlk {
            const DEVICE_TYPE: DeviceType = DeviceType::Block;
            type Device = axdriver_virtio::VirtIoBlkDev<VirtIoHalImpl, VirtIoTransport>;
            fn try_new(transport: VirtIoTransport, _irq: Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_block(Self::Device::try_new(transport)?))
            }
        }
    }
}
cfg_if! {
    if #[cfg(display_dev = "virtio-gpu")] {
        pub struct VirtIoGpu;
        impl VirtIoDevMeta for VirtIoGpu {
            const DEVICE_TYPE: DeviceType = DeviceType::Display;
            type Device = axdriver_virtio::VirtIoGpuDev<VirtIoHalImpl, VirtIoTransport>;
            fn try_new(transport: VirtIoTransport, _irq: Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_display(Self::Device::try_new(transport)?))
            }
        }
    }
}
cfg_if! {
    if #[cfg(input_dev = "virtio-input")] {
        pub struct VirtIoInput;
        impl VirtIoDevMeta for VirtIoInput {
            const DEVICE_TYPE: DeviceType = DeviceType::Input;
            type Device = axdriver_virtio::VirtIoInputDev<VirtIoHalImpl, VirtIoTransport>;
            fn try_new(transport: VirtIoTransport, _irq: Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_input(Self::Device::try_new(transport)?))
            }
        }
    }
}
cfg_if! {
    if #[cfg(vsock_dev = "virtio-socket")] {
        pub struct VirtIoSocket;
        impl VirtIoDevMeta for VirtIoSocket {
            const DEVICE_TYPE: DeviceType = DeviceType::Vsock;
            type Device = axdriver_virtio::VirtIoSocketDev<VirtIoHalImpl, VirtIoTransport>;
            fn try_new(transport: VirtIoTransport, _irq:  Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_vsock(Self::Device::try_new(transport)?))
            }
        }
    }
}
/// A common driver for all VirtIO devices that implements [`DriverProbe`].
pub struct VirtIoDriver<D: VirtIoDevMeta + ?Sized>(PhantomData<D>);
impl<D: VirtIoDevMeta> DriverProbe for VirtIoDriver<D> {
    #[cfg(bus = "mmio")]
    fn probe_mmio(mmio_base: usize, mmio_size: usize) -> Option<AxDeviceEnum> {
        let base_vaddr = phys_to_virt(mmio_base.into());
        if let Some((ty, transport)) =
            axdriver_virtio::probe_mmio_device(base_vaddr.as_mut_ptr(), mmio_size)
            && ty == D::DEVICE_TYPE
        {
            match D::try_new(transport, None) {
                Ok(dev) => return Some(dev),
                Err(e) => {
                    warn!(
                        "failed to initialize MMIO device at [PA:{:#x}, PA:{:#x}): {:?}",
                        mmio_base,
                        mmio_base + mmio_size,
                        e
                    );
                    return None;
                }
            }
        }
        None
    }
    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        dev_info: &DeviceFunctionInfo,
    ) -> Option<AxDeviceEnum> {
        if dev_info.vendor_id != 0x1af4 {
            return None;
        }
        match (D::DEVICE_TYPE, dev_info.device_id) {
            (DeviceType::Net, 0x1000) | (DeviceType::Net, 0x1041) => {}
            (DeviceType::Block, 0x1001) | (DeviceType::Block, 0x1042) => {}
            (DeviceType::Input, 0x1052) => {}
            (DeviceType::Display, 0x1050) => {}
            (DeviceType::Vsock, 0x1053) => {}
            _ => return None,
        }
        if let Some((ty, transport, irq)) =
            axdriver_virtio::probe_pci_device::<VirtIoHalImpl>(root, bdf, dev_info)
            && ty == D::DEVICE_TYPE
        {
            match D::try_new(transport, Some(irq)) {
                Ok(dev) => return Some(dev),
                Err(e) => {
                    warn!("failed to initialize PCI device at {bdf}({dev_info}): {e:?}");
                    return None;
                }
            }
        }
        None
    }
}
pub struct VirtIoHalImpl;
cfg_if! {
    if #[cfg(feature = "crosvm")] {
        use hashbrown::HashMap;
        use axsync::Mutex;
        use spin::Lazy;
        const PAGE_SIZE: usize = 0x1000; // define page size as 4KB
        const VIRTIO_QUEUE_SIZE: usize = 32;
        struct VirtIoFramePool
        {
            pool_paddr: PhysAddr,
            bitmap: [bool; VIRTIO_QUEUE_SIZE],
            v2p_map: HashMap<usize, usize>,
        }
        static VIRTIO_FRAME_POOL: Lazy<Mutex<VirtIoFramePool>> = Lazy::new(|| {
            let vaddr = global_allocator().alloc_pages(VIRTIO_QUEUE_SIZE,0x1000,UsageKind::Dma).expect("virtio frame pool alloc failed");
            let paddr = virt_to_phys(vaddr.into());
            share_dma_buffer(paddr.as_usize(), VIRTIO_QUEUE_SIZE * PAGE_SIZE);
            let pool = VirtIoFramePool {
                pool_paddr: paddr.into(),
                bitmap: [false; VIRTIO_QUEUE_SIZE],
                v2p_map: HashMap::new(),
            };
            Mutex::new(pool)
        });
        impl VirtIoFramePool {
            fn alloc_page_from_pool(&mut self, vaddr: usize) -> PhysAddr {
                let frame_index = {
                    let mut fram_index = usize::MAX;
                    for i in 0..VIRTIO_QUEUE_SIZE {
                        if !self.bitmap[i] {
                            fram_index = i;
                            self.bitmap[i] = true;
                            break;
                        }
                    }
                    assert!(fram_index != usize::MAX);
                    fram_index
                };
                self.v2p_map.insert(vaddr, frame_index);
                let paddr = self.pool_paddr + (PAGE_SIZE * frame_index);
                paddr
            }
            fn free_page_to_pool(&mut self, vaddr: usize) {
                let frame_index = self.v2p_map.remove(&vaddr).unwrap();
                assert!(self.bitmap[frame_index]);
                self.bitmap[frame_index] = false;
            }
        }
    } else if #[cfg(feature = "sev")] {
        use hashbrown::HashMap;
        use axsync::Mutex;
        use spin::Lazy;

        const PAGE_SIZE: usize = 0x1000; // 4KB page size

        // AMD SEV shared memory pool configuration
        // These should match axconfig values for axplat-x86-csv
        const SEV_SHARED_MEM_BASE: usize = 0x0100_0000;  // 16MB
        const SEV_SHARED_MEM_SIZE: usize = 0x0020_0000;  // 2MB
        const SEV_MAX_PAGES: usize = SEV_SHARED_MEM_SIZE / PAGE_SIZE;
        const SEV_BITMAP_SIZE: usize = (SEV_MAX_PAGES + 63) / 64;

        /// AMD SEV shared memory pool for VirtIO DMA buffers.
        ///
        /// This pool is pre-allocated in a memory region that is mapped without
        /// the C-Bit, making it accessible to both guest and host.
        struct SevSharedPool {
            /// Bitmap tracking allocated pages (1 = allocated, 0 = free)
            bitmap: [u64; SEV_BITMAP_SIZE],
            /// Maps virtual address to (shared_paddr, size) for bounce buffers
            v2p_map: HashMap<usize, (PhysAddr, usize)>,
            /// Next allocation hint
            next_hint: usize,
        }

        static SEV_SHARED_POOL: Lazy<Mutex<SevSharedPool>> = Lazy::new(|| {
            log::info!(
                "SEV VirtIO shared pool: base={:#x}, size={:#x}, pages={}",
                SEV_SHARED_MEM_BASE, SEV_SHARED_MEM_SIZE, SEV_MAX_PAGES
            );
            Mutex::new(SevSharedPool {
                bitmap: [0; SEV_BITMAP_SIZE],
                v2p_map: HashMap::new(),
                next_hint: 0,
            })
        });

        impl SevSharedPool {
            /// Allocates contiguous pages from the shared memory pool.
            fn alloc_pages(&mut self, pages: usize) -> Option<PhysAddr> {
                if pages == 0 || pages > SEV_MAX_PAGES {
                    return None;
                }

                // For single page, use fast path
                if pages == 1 {
                    return self.alloc_single_page();
                }

                // Multi-page allocation: find contiguous free pages
                let mut start = 0;
                let mut count = 0;

                for i in 0..SEV_MAX_PAGES {
                    if self.is_page_free(i) {
                        if count == 0 {
                            start = i;
                        }
                        count += 1;
                        if count == pages {
                            // Found enough contiguous pages
                            for j in start..start + pages {
                                self.set_page_allocated(j);
                            }
                            self.next_hint = start + pages;
                            return Some((SEV_SHARED_MEM_BASE + start * PAGE_SIZE) as PhysAddr);
                        }
                    } else {
                        count = 0;
                    }
                }

                None
            }

            fn alloc_single_page(&mut self) -> Option<PhysAddr> {
                // Search from hint
                for i in self.next_hint..SEV_MAX_PAGES {
                    if self.is_page_free(i) {
                        self.set_page_allocated(i);
                        self.next_hint = i + 1;
                        return Some((SEV_SHARED_MEM_BASE + i * PAGE_SIZE) as PhysAddr);
                    }
                }

                // Wrap around
                for i in 0..self.next_hint {
                    if self.is_page_free(i) {
                        self.set_page_allocated(i);
                        self.next_hint = i + 1;
                        return Some((SEV_SHARED_MEM_BASE + i * PAGE_SIZE) as PhysAddr);
                    }
                }

                None
            }

            /// Frees pages back to the pool.
            fn free_pages(&mut self, paddr: PhysAddr, pages: usize) {
                if paddr < SEV_SHARED_MEM_BASE || paddr >= SEV_SHARED_MEM_BASE + SEV_SHARED_MEM_SIZE {
                    log::warn!("free_pages: paddr {:#x} outside shared region", paddr);
                    return;
                }

                let start_page = (paddr - SEV_SHARED_MEM_BASE) / PAGE_SIZE;
                for i in 0..pages {
                    let page_idx = start_page + i;
                    if page_idx < SEV_MAX_PAGES {
                        self.set_page_free(page_idx);
                    }
                }

                if start_page < self.next_hint {
                    self.next_hint = start_page;
                }
            }

            /// Allocates a shared page and records the mapping for a bounce buffer.
            fn alloc_bounce_buffer(&mut self, vaddr: usize, size: usize) -> Option<PhysAddr> {
                let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
                let paddr = self.alloc_pages(pages)?;
                self.v2p_map.insert(vaddr, (paddr, size));
                Some(paddr)
            }

            /// Frees a bounce buffer and removes the mapping.
            fn free_bounce_buffer(&mut self, vaddr: usize) -> Option<(PhysAddr, usize)> {
                if let Some((paddr, size)) = self.v2p_map.remove(&vaddr) {
                    let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
                    self.free_pages(paddr, pages);
                    Some((paddr, size))
                } else {
                    None
                }
            }

            #[inline]
            fn is_page_free(&self, page_idx: usize) -> bool {
                let word_idx = page_idx / 64;
                let bit_idx = page_idx % 64;
                (self.bitmap[word_idx] & (1u64 << bit_idx)) == 0
            }

            #[inline]
            fn set_page_allocated(&mut self, page_idx: usize) {
                let word_idx = page_idx / 64;
                let bit_idx = page_idx % 64;
                self.bitmap[word_idx] |= 1u64 << bit_idx;
            }

            #[inline]
            fn set_page_free(&mut self, page_idx: usize) {
                let word_idx = page_idx / 64;
                let bit_idx = page_idx % 64;
                self.bitmap[word_idx] &= !(1u64 << bit_idx);
            }
        }
    }
}

unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        #[cfg(feature = "sev")]
        {
            // For AMD SEV, allocate from the shared memory pool (no C-Bit)
            let mut pool = SEV_SHARED_POOL.lock();
            if let Some(paddr) = pool.alloc_pages(pages) {
                let vaddr = phys_to_virt(paddr.into());
                let ptr = NonNull::new(vaddr.as_mut_ptr()).unwrap();
                // Clear the allocated memory
                unsafe {
                    core::ptr::write_bytes(vaddr.as_mut_ptr(), 0, pages * PAGE_SIZE);
                }
                return (paddr, ptr);
            }
            return (0, NonNull::dangling());
        }

        #[cfg(not(feature = "sev"))]
        {
            let vaddr = if let Ok(vaddr) = global_allocator().alloc_pages(pages, 0x1000, UsageKind::Dma)
            {
                vaddr
            } else {
                return (0, NonNull::dangling());
            };
            let paddr = virt_to_phys(vaddr.into());
            let ptr = NonNull::new(vaddr as _).unwrap();

            #[cfg(feature = "crosvm")]
            {
                share_dma_buffer(paddr.as_usize(), pages * 0x1000);
            }
            (paddr.as_usize(), ptr)
        }
    }

    #[allow(unused_variables)]
    unsafe fn dma_dealloc(paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        #[cfg(feature = "sev")]
        {
            let mut pool = SEV_SHARED_POOL.lock();
            pool.free_pages(paddr, pages);
            return 0;
        }

        #[cfg(not(feature = "sev"))]
        {
            global_allocator().dealloc_pages(vaddr.as_ptr() as usize, pages, UsageKind::Dma);
            #[cfg(feature = "crosvm")]
            {
                unshare_dma_buffer(paddr as usize, pages * 0x1000);
            }
            0
        }
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(phys_to_virt(paddr.into()).as_mut_ptr()).unwrap()
    }
    #[allow(unused_variables)]
    #[inline]
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
        #[cfg(feature = "sev")]
        {
            let vaddr = buffer.as_ptr() as *mut u8 as usize;
            let len = buffer.len();

            // Allocate a bounce buffer from the shared pool
            let paddr = {
                let mut pool = SEV_SHARED_POOL.lock();
                pool.alloc_bounce_buffer(vaddr, len)
                    .expect("SEV: failed to allocate shared bounce buffer")
            };

            // If data flows from driver to device, copy to shared buffer
            if direction != BufferDirection::DeviceToDriver {
                let shared_ptr = phys_to_virt(paddr.into()).as_mut_ptr();
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buffer.as_ptr() as *const u8,
                        shared_ptr,
                        len,
                    );
                }
            }

            paddr
        }

        #[cfg(all(feature = "crosvm", not(feature = "sev")))]
        {
            let vaddr = buffer.as_ptr() as *mut u8 as usize;
            let len = buffer.len();
            assert!(len <= 0x1000, "only support share buffer size <= 4KB");
            let paddr = {
                let mut pool = VIRTIO_FRAME_POOL.lock();
                pool.alloc_page_from_pool(vaddr)
            };
            if direction != BufferDirection::DeviceToDriver {
                let data = unsafe {
                    let data = phys_to_virt(paddr.into()).as_usize() as *mut u8;
                    core::slice::from_raw_parts_mut(data, len)
                };
                data.clone_from_slice(unsafe { &buffer.as_ref() });
            }
            paddr
        }

        #[cfg(not(any(feature = "crosvm", feature = "sev")))]
        {
            let vaddr = buffer.as_ptr() as *mut u8 as usize;
            virt_to_phys(vaddr.into()).into()
        }
    }
    #[inline]
    #[allow(unused_variables)]
    unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        #[cfg(feature = "sev")]
        {
            let mut buffer = buffer;
            let vaddr = buffer.as_ptr() as *mut u8 as usize;
            let len = buffer.len();

            // If data flows from device to driver, copy back from shared buffer
            if direction != BufferDirection::DriverToDevice {
                let shared_ptr = phys_to_virt(paddr.into()).as_ptr();
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        shared_ptr,
                        buffer.as_ptr() as *mut u8,
                        len,
                    );
                }
            }

            // Free the bounce buffer
            let mut pool = SEV_SHARED_POOL.lock();
            pool.free_bounce_buffer(vaddr);
        }

        #[cfg(all(feature = "crosvm", not(feature = "sev")))]
        {
            let mut buffer = buffer;
            let vaddr = buffer.as_ptr() as *mut u8 as usize;
            if direction != BufferDirection::DriverToDevice {
                let data = unsafe {
                    let data = phys_to_virt(paddr.into()).as_usize() as *mut u8;
                    core::slice::from_raw_parts(data, buffer.len())
                };
                unsafe { buffer.as_mut().clone_from_slice(&data) };
            }
            let mut pool = VIRTIO_FRAME_POOL.lock();
            pool.free_page_to_pool(vaddr);
        }
    }
}