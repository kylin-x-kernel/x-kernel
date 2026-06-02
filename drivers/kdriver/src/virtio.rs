// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO device probing and HAL integration.
use core::{marker::PhantomData, ptr::NonNull};

use cfg_if::cfg_if;
use driver_base::{DeviceKind, DriverOps, DriverResult};
use virtio::{BufferDirection, PhysAddr, VirtIoHal};

use crate::{DeviceEnum, drivers::DriverProbe, iomap_mmio};

cfg_if! {
    if #[cfg(bus = "pci")] {
        use pci::{MmioCam, PciConfigAccess, PciRoot, DeviceFunction, DeviceFunctionInfo};
        type VirtIoTransport = virtio::PciTransport;
    } else if #[cfg(bus =  "mmio")] {
        type VirtIoTransport = virtio::MmioTransport<'static>;
    }
}

/// Metadata describing a VirtIO device type and its driver bindings.
pub trait VirtIoDevMeta {
    /// The device category for this VirtIO device.
    const DEVICE_TYPE: DeviceKind;

    /// Concrete device type that implements driver operations.
    type Device: DriverOps;
    /// Driver type used for probing and instantiation.
    type Driver = VirtIoDriver<Self>;

    /// Try to construct a driver instance from a transport and optional IRQ.
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

cfg_if! {
    if #[cfg(virtio_9p_dev = "virtio-9p")] {
        pub struct VirtIo9p;

        impl VirtIoDevMeta for VirtIo9p {
            const DEVICE_TYPE: DeviceKind = DeviceKind::Virtio9p;
            type Device = virtio::VirtIo9pDev<VirtIoHalImpl, VirtIoTransport>;

            fn try_new(transport: VirtIoTransport, _irq:  Option<usize>) -> DriverResult<DeviceEnum> {
                Ok(DeviceEnum::from_virtio_9p(Self::Device::try_new(transport)?))
            }
        }
    }
}

/// A common driver for all VirtIO devices that implements [`DriverProbe`].
pub struct VirtIoDriver<D: VirtIoDevMeta + ?Sized>(PhantomData<D>);

impl<D: VirtIoDevMeta> DriverProbe for VirtIoDriver<D> {
    #[cfg(bus = "mmio")]
    fn probe_mmio(mmio_base: usize, mmio_size: usize) -> Option<DeviceEnum> {
        let base_vaddr = match iomap_mmio(mmio_base, mmio_size, "virtio-mmio") {
            Ok(vaddr) => vaddr,
            Err(err) => {
                warn!(
                    "failed to map MMIO device at [PA:{:#x}, PA:{:#x}): {:?}",
                    mmio_base,
                    mmio_base + mmio_size,
                    err
                );
                return None;
            }
        };
        // SAFETY: `base_vaddr` was obtained from `iomap_mmio` which maps a valid
        // physical MMIO region, and `mmio_size` matches the region size.
        if let Some((ty, transport)) =
            unsafe { virtio::probe_mmio_device(base_vaddr.as_ptr(), mmio_size) }
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
        root: &mut PciRoot<MmioCam<'static>>,
        config: &mut PciConfigAccess,
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
            (DeviceKind::Virtio9p, 0x1009) => {}
            _ => return None,
        }

        if let Some((ty, transport, irq)) =
            virtio::probe_pci_device::<VirtIoHalImpl, MmioCam<'static>>(root, bdf, dev_info, config)
            && ty == D::DEVICE_TYPE
        {
            match D::try_new(transport, Some(irq)) {
                Ok(dev) => {
                    return Some(dev);
                }
                Err(e) => {
                    warn!("failed to initialize PCI device at {bdf}({dev_info}): {e:?}");
                    return None;
                }
            }
        }
        None
    }
}

use memaddr::PAGE_SIZE_4K;
pub struct VirtIoHalImpl;

unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        use core::alloc::Layout;
        // For AMD SEV, use kdma which handles SHARED flag (clears C-Bit)
        let size = pages * PAGE_SIZE_4K;
        let layout = Layout::from_size_align(size, PAGE_SIZE_4K).unwrap();
        match unsafe { kdma::allocate_dma_memory(layout) } {
            Ok(dma_info) => {
                // Clear the allocated memory
                unsafe {
                    core::ptr::write_bytes(dma_info.cpu_addr.as_ptr(), 0, size);
                }
                let paddr = dma_info.bus_addr.as_u64() as PhysAddr;
                let ptr = dma_info.cpu_addr;
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
        unsafe { kdma::deallocate_dma_memory(dma_info, layout) };
        0
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
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
