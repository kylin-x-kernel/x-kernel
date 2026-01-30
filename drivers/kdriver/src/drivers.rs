//! Defines types and probe methods of all supported devices.

#![allow(unused_imports, dead_code)]

use driver_base::DeviceKind;
#[cfg(feature = "bus-pci")]
use pci::{DeviceFunction, DeviceFunctionInfo, PciRoot};

pub use super::dummy::*;
use crate::DeviceEnum;
#[cfg(feature = "virtio")]
use crate::virtio::{self, VirtIoDevMeta};

/// Probe entry points implemented by each driver type.
pub trait DriverProbe {
    /// Probe for globally discoverable devices.
    fn probe_global() -> Option<DeviceEnum> {
        None
    }

    #[cfg(bus = "mmio")]
    /// Probe an MMIO device at the given physical base and size.
    fn probe_mmio(_mmio_base: usize, _mmio_size: usize) -> Option<DeviceEnum> {
        None
    }

    #[cfg(bus = "pci")]
    /// Probe a PCI device described by BDF and device info.
    fn probe_pci(
        _root: &mut PciRoot,
        _bdf: DeviceFunction,
        _dev_info: &DeviceFunctionInfo,
    ) -> Option<DeviceEnum> {
        None
    }
}

#[cfg(net_dev = "virtio-net")]
register_net_driver!(
    <virtio::VirtIoNet as VirtIoDevMeta>::Driver,
    <virtio::VirtIoNet as VirtIoDevMeta>::Device
);

#[cfg(block_dev = "virtio-blk")]
register_block_driver!(
    <virtio::VirtIoBlk as VirtIoDevMeta>::Driver,
    <virtio::VirtIoBlk as VirtIoDevMeta>::Device
);

#[cfg(display_dev = "virtio-gpu")]
register_display_driver!(
    <virtio::VirtIoGpu as VirtIoDevMeta>::Driver,
    <virtio::VirtIoGpu as VirtIoDevMeta>::Device
);

#[cfg(input_dev = "virtio-input")]
register_input_driver!(
    <virtio::VirtIoInput as VirtIoDevMeta>::Driver,
    <virtio::VirtIoInput as VirtIoDevMeta>::Device
);

#[cfg(vsock_dev = "virtio-socket")]
register_vsock_driver!(
    <virtio::VirtIoSocket as VirtIoDevMeta>::Driver,
    <virtio::VirtIoSocket as VirtIoDevMeta>::Device
);

cfg_if::cfg_if! {
    if #[cfg(block_dev = "ramdisk")] {
        pub struct RamDiskDriver;
        register_block_driver!(RamDiskDriver, block::ramdisk::RamDisk);

        impl DriverProbe for RamDiskDriver {
            fn probe_global() -> Option<DeviceEnum> {
                // TODO: format RAM disk
                Some(DeviceEnum::from_block(
                    block::ramdisk::RamDisk::new(0x100_0000), // 16 MiB
                ))
            }
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(block_dev = "sdmmc")] {
        pub struct SdMmcDriver;
        register_block_driver!(SdMmcDriver, block::sdmmc::SdMmcDriver);

        impl DriverProbe for SdMmcDriver {
            fn probe_global() -> Option<DeviceEnum> {
                let sdmmc = unsafe {
                    block::sdmmc::SdMmcDriver::new(
                        khal::mem::p2v(platconfig::devices::SDMMC_PADDR.into()).into(),
                    )
                };
                Some(DeviceEnum::from_block(sdmmc))
            }
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(block_dev = "ahci")] {
        pub struct AhciHalImpl;
        impl block::ahci::AhciHal for AhciHalImpl {
            fn virt_to_phys(va: usize) -> usize {
                khal::mem::v2p(va.into()).as_usize()
            }

            fn current_ms() -> u64 {
                khal::time::monotonic_time_nanos() / 1_000_000
            }

            fn flush_dcache() {
                #[cfg(target_arch = "loongarch64")]
                unsafe {
                    // LoongArch64: Ensure data cache operations are synchronized for AHCI DMA coherency.
                    core::arch::asm!("dbar 0");
                }
            }
        }

        pub struct AhciDriver;
        register_block_driver!(AhciDriver, block::ahci::AhciDriver<AhciHalImpl>);

        impl DriverProbe for AhciDriver {
            fn probe_global() -> Option<DeviceEnum> {
                #[cfg(doc)]
                {
                    None
                }

                #[cfg(not(doc))]
                {
                    let ahci = unsafe {
                        block::ahci::AhciDriver::<AhciHalImpl>::new(
                            khal::mem::p2v(platconfig::devices::AHCI_PADDR.into()).into(),
                        )?
                    };
                    Some(DeviceEnum::from_block(ahci))
                }
            }
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(block_dev = "bcm2835-sdhci")]{
        pub struct BcmSdhciDriver;
        register_block_driver!(BcmSdhciDriver, block::bcm2835sdhci::SDHCIDriver);

        impl DriverProbe for BcmSdhciDriver {
            fn probe_global() -> Option<DeviceEnum> {
                debug!("mmc probe");
                block::bcm2835sdhci::SDHCIDriver::try_new()
                    .ok()
                    .map(DeviceEnum::from_block)
            }
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(net_dev = "ixgbe")] {
        use crate::ixgbe::IxgbeHalImpl;
        pub struct IxgbeDriver;
        register_net_driver!(IxgbeDriver, net::ixgbe::IxgbeNic<IxgbeHalImpl, 1024, 1>);
        impl DriverProbe for IxgbeDriver {
            #[cfg(bus = "pci")]
            fn probe_pci(
                root: &mut pci::PciRoot,
                bdf: pci::DeviceFunction,
                dev_info: &pci::DeviceFunctionInfo,
            ) -> Option<crate::DeviceEnum> {
                use net::ixgbe::{INTEL_82599, INTEL_VEND, IxgbeNic};
                if dev_info.vendor_id == INTEL_VEND && dev_info.device_id == INTEL_82599 {
                    // Intel 10Gb Network
                    info!("ixgbe PCI device found at {:?}", bdf);

                    // Initialize the device
                    // These can be changed according to the requirments specified in the ixgbe init
                    // function.
                    const QN: u16 = 1;
                    const QS: usize = 1024;
                    let bar_info = root.bar_info(bdf, 0).unwrap();
                    match bar_info {
                        pci::BarInfo::Memory { address, size, .. } => {
                            let ixgbe_nic = IxgbeNic::<IxgbeHalImpl, QS, QN>::init(
                                khal::mem::p2v((address as usize).into()).into(),
                                size as usize,
                            )
                            .expect("failed to initialize ixgbe device");
                            return Some(DeviceEnum::from_net(ixgbe_nic));
                        }
                        pci::BarInfo::IO { .. } => {
                            error!("ixgbe: BAR0 is of I/O type");
                            return None;
                        }
                    }
                }
                None
            }
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(net_dev = "fxmac")]{
        use axalloc::{UsageKind, global_allocator};
        use khal::mem::PAGE_SIZE_4K;

        #[crate_interface::impl_interface]
        impl net::fxmac::KernelFunc for FXmacDriver {
            fn virt_to_phys(addr: usize) -> usize {
                khal::mem::v2p(addr.into()).into()
            }

            fn phys_to_virt(addr: usize) -> usize {
                khal::mem::p2v(addr.into()).into()
            }

            fn dma_alloc_coherent(pages: usize) -> (usize, usize) {
                let Ok(vaddr) = global_allocator().alloc_pages(pages, PAGE_SIZE_4K, UsageKind::Dma)
                else {
                    error!("failed to alloc pages");
                    return (0, 0);
                };
                let paddr = khal::mem::v2p((vaddr).into());
                debug!("alloc pages @ vaddr={:#x}, paddr={:#x}", vaddr, paddr);
                (vaddr, paddr.as_usize())
            }

            fn dma_free_coherent(vaddr: usize, pages: usize) {
                global_allocator().dealloc_pages(vaddr, pages, UsageKind::Dma);
            }

            fn dma_request_irq(_irq: usize, _handler: fn()) {
                warn!("unimplemented dma_request_irq for fxmax");
            }
        }

        register_net_driver!(FXmacDriver, net::fxmac::FXmacNic);

        pub struct FXmacDriver;
        impl DriverProbe for FXmacDriver {
            fn probe_global() -> Option<DeviceEnum> {
                info!("fxmac for phytiumpi probe global");
                net::fxmac::FXmacNic::init(0)
                    .ok()
                    .map(DeviceEnum::from_net)
            }
        }
    }
}
