// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// Copyright (C) 2025 Yuekai Jia <equation618@gmail.com>
// Copyright (C) 2025 ChengXiang Qi <kuangjux@outlook.com>
// See LICENSE for license details.
//
// This file has been modified by KylinSoft on 2025.

use core::{marker::PhantomData, ptr::NonNull};

use cfg_if::cfg_if;
use driver_base::{DeviceKind, DriverOps, DriverResult};
use khal::mem::p2v;
#[cfg(feature = "crosvm")]
use khal::psci::{dma_share, dma_unshare};
use virtio::{BufferDirection, PhysAddr, VirtIoHal};

use crate::{DeviceEnum, drivers::DriverProbe};

cfg_if! {
    if #[cfg(bus = "pci")] {
        use pci::{PciRoot, DeviceFunction, DeviceFunctionInfo};
        type VirtIoTransport = virtio::PciTransport;
    } else if #[cfg(bus =  "mmio")] {
        type VirtIoTransport = virtio::MmioTransport;
    }
}

/// A trait for VirtIO device meta information.
pub trait VirtIoDevMeta {
    const DEVICE_TYPE: DeviceKind;

    type Device: DriverOps;
    type Driver = VirtIoDriver<Self>;

    fn try_new(transport: VirtIoTransport, irq: Option<usize>) -> DriverResult<DeviceEnum>;
}

cfg_if! {
    if #[cfg(net_dev = "virtio-net")] {
        pub struct VirtIoNet;

        impl VirtIoDevMeta for VirtIoNet {
            const DEVICE_TYPE: DeviceKind = DeviceKind::Net;
            type Device = virtio::VirtIoNetDev<VirtIoHalImpl, VirtIoTransport, 64>;

            fn try_new(transport: VirtIoTransport, irq: Option<usize>) -> DriverResult<DeviceEnum> {
                Ok(DeviceEnum::from_net(Self::Device::try_new(transport, irq)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(block_dev = "virtio-blk")] {
        pub struct VirtIoBlk;

        impl VirtIoDevMeta for VirtIoBlk {
            const DEVICE_TYPE: DeviceKind = DeviceKind::Block;
            type Device = virtio::VirtIoBlkDev<VirtIoHalImpl, VirtIoTransport>;

            fn try_new(transport: VirtIoTransport, _irq: Option<usize>) -> DriverResult<DeviceEnum> {
                Ok(DeviceEnum::from_block(Self::Device::try_new(transport)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(display_dev = "virtio-gpu")] {
        pub struct VirtIoGpu;

        impl VirtIoDevMeta for VirtIoGpu {
            const DEVICE_TYPE: DeviceKind = DeviceKind::Display;
            type Device = virtio::VirtIoGpuDev<VirtIoHalImpl, VirtIoTransport>;

            fn try_new(transport: VirtIoTransport, _irq: Option<usize>) -> DriverResult<DeviceEnum> {
                Ok(DeviceEnum::from_display(Self::Device::try_new(transport)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(input_dev = "virtio-input")] {
        pub struct VirtIoInput;

        impl VirtIoDevMeta for VirtIoInput {
            const DEVICE_TYPE: DeviceKind = DeviceKind::Input;
            type Device = virtio::VirtIoInputDev<VirtIoHalImpl, VirtIoTransport>;

            fn try_new(transport: VirtIoTransport, _irq: Option<usize>) -> DriverResult<DeviceEnum> {
                Ok(DeviceEnum::from_input(Self::Device::try_new(transport)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(vsock_dev = "virtio-socket")] {
        pub struct VirtIoSocket;

        impl VirtIoDevMeta for VirtIoSocket {
            const DEVICE_TYPE: DeviceKind = DeviceKind::Vsock;
            type Device = virtio::VirtIoSocketDev<VirtIoHalImpl, VirtIoTransport>;

            fn try_new(transport: VirtIoTransport, _irq:  Option<usize>) -> DriverResult<DeviceEnum> {
                Ok(DeviceEnum::from_vsock(Self::Device::try_new(transport)?))
            }
        }
    }
}

/// A common driver for all VirtIO devices that implements [`DriverProbe`].
pub struct VirtIoDriver<D: VirtIoDevMeta + ?Sized>(PhantomData<D>);

impl<D: VirtIoDevMeta> DriverProbe for VirtIoDriver<D> {
    #[cfg(bus = "mmio")]
    fn probe_mmio(mmio_base: usize, mmio_size: usize) -> Option<AxDeviceEnum> {
        let base_vaddr = p2v(mmio_base.into());
        if let Some((ty, transport)) = virtio::probe_mmio_device(base_vaddr.as_mut_ptr(), mmio_size)
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
    ) -> Option<DeviceEnum> {
        if dev_info.vendor_id != 0x1af4 {
            return None;
        }
        match (D::DEVICE_TYPE, dev_info.device_id) {
            (DeviceKind::Net, 0x1000) | (DeviceKind::Net, 0x1041) => {}
            (DeviceKind::Block, 0x1001) | (DeviceKind::Block, 0x1042) => {}
            (DeviceKind::Input, 0x1052) => {}
            (DeviceKind::Display, 0x1050) => {}
            (DeviceKind::Vsock, 0x1053) => {}
            _ => return None,
        }

        if let Some((ty, transport, irq)) =
            virtio::probe_pci_device::<VirtIoHalImpl>(root, bdf, dev_info)
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

const PAGE_SIZE: usize = 0x1000; // 4KB page size
pub struct VirtIoHalImpl;

unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        use core::alloc::Layout;
        // For AMD SEV, use kdma which handles SHARED flag (clears C-Bit)
        let size = pages * PAGE_SIZE;
        let layout = Layout::from_size_align(size, PAGE_SIZE).unwrap();
        match unsafe { kdma::allocate_dma_memory(layout) } {
            Ok(dma_info) => {
                // Clear the allocated memory
                unsafe {
                    core::ptr::write_bytes(dma_info.cpu_addr.as_ptr(), 0, size);
                }
                let paddr = dma_info.bus_addr.as_u64() as PhysAddr;
                let ptr = dma_info.cpu_addr;
                #[cfg(feature = "crosvm")]
                {
                    dma_share(paddr, pages * 0x1000);
                }
                // bus_addr is the physical address for DMA
                (paddr, ptr)
            }
            Err(e) => {
                log::error!("dma_alloc failed: pages={}, error={:?}", pages, e);
                (0, NonNull::dangling())
            }
        }
    }

    #[allow(unused_variables)]
    unsafe fn dma_dealloc(paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        use core::alloc::Layout;
        let size = pages * PAGE_SIZE;
        let layout = Layout::from_size_align(size, PAGE_SIZE).unwrap();
        let dma_info = kdma::DMAInfo {
            cpu_addr: vaddr,
            bus_addr: kdma::DmaBusAddress::new(paddr as u64),
        };
        unsafe { kdma::deallocate_dma_memory(dma_info, layout) };
        #[cfg(feature = "crosvm")]
        {
            dma_unshare(paddr as usize, pages * 0x1000);
        }
        0
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(p2v(paddr.into()).as_mut_ptr()).unwrap()
    }

    #[allow(unused_variables)]
    #[inline]
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
        #[cfg(any(feature = "sev", feature = "crosvm"))]
        {
            use core::{
                alloc::Layout,
                sync::atomic::{Ordering, fence},
            };

            let len = buffer.len();
            let aligned_size = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            let layout = Layout::from_size_align(aligned_size, PAGE_SIZE).unwrap();

            // Allocate a bounce buffer using kdma (with SHARED flag)
            let dma_info = unsafe { kdma::allocate_dma_memory(layout) }
                .expect("failed to allocate shared bounce buffer via kdma");
            let paddr = dma_info.bus_addr.as_u64() as PhysAddr;
            let vaddr = dma_info.cpu_addr.as_ptr() as usize;
            // For crosvm, also call share_dma_buffer
            #[cfg(feature = "crosvm")]
            {
                dma_share(paddr as usize, aligned_size);
            }
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            // If data flows from driver to device, copy to shared buffer
            if direction != BufferDirection::DeviceToDriver {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buffer.as_ptr() as *const u8,
                        vaddr as *mut u8,
                        len,
                    );
                }
                // Ensure the copy is not optimized away and is visible
                core::sync::atomic::fence(Ordering::SeqCst);
            }

            // Full memory barrier to ensure data is visible to the device
            fence(Ordering::SeqCst);

            paddr
        }

        #[cfg(not(any(feature = "crosvm", feature = "sev")))]
        {
            let vaddr = buffer.as_ptr() as *mut u8 as usize;
            khal::mem::v2p(vaddr.into()).into()
        }
    }

    #[inline]
    #[allow(unused_variables)]
    unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        #[cfg(any(feature = "sev", feature = "crosvm"))]
        {
            use core::{
                alloc::Layout,
                sync::atomic::{Ordering, fence},
            };

            let len = buffer.len();
            let aligned_size = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            fence(Ordering::SeqCst);

            // If data flows from device to driver, copy back from shared buffer
            if direction != BufferDirection::DriverToDevice {
                let shared_ptr = p2v(paddr.into()).as_ptr();
                unsafe {
                    core::ptr::copy_nonoverlapping(shared_ptr, buffer.as_ptr() as *mut u8, len);
                }
                // Ensure the copy is not optimized away and create a final
                // ordering point before we proceed.
                core::sync::atomic::fence(Ordering::SeqCst);
            }

            // For crosvm, call unshare_dma_buffer before freeing
            #[cfg(feature = "crosvm")]
            {
                dma_unshare(paddr as usize, aligned_size);
            }

            // Free the bounce buffer via kdma
            let layout = Layout::from_size_align(aligned_size, PAGE_SIZE).unwrap();
            let dma_info = kdma::DMAInfo {
                cpu_addr: NonNull::new(p2v(paddr.into()).as_mut_ptr()).unwrap(),
                bus_addr: kdma::DmaBusAddress::new(paddr as u64),
            };
            unsafe { kdma::deallocate_dma_memory(dma_info, layout) };
        }
    }
}
