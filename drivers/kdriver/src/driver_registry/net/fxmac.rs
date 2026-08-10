// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

use axalloc::{UsageKind, global_allocator};
use driver_base::{DeviceKind, DriverError};
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};
use khal::mem::PAGE_SIZE_4K;

use crate::driver_registry::{BoxedDriver, firmware_specs::FXMAC};

pub struct FXmacKernelFunc;

#[crate_interface::impl_interface]
impl net::fxmac::KernelFunc for FXmacKernelFunc {
    fn virt_to_phys(addr: usize) -> usize {
        khal::mem::v2p(addr.into()).into()
    }

    fn phys_to_virt(addr: usize) -> usize {
        khal::mem::p2v(addr.into()).into()
    }

    fn dma_alloc_coherent(pages: usize) -> (usize, usize) {
        let Ok(vaddr) = global_allocator().alloc_pages(pages, PAGE_SIZE_4K, UsageKind::Dma) else {
            log::error!("failed to alloc pages");
            return (0, 0);
        };
        let paddr = khal::mem::v2p((vaddr).into());
        log::debug!("alloc pages @ vaddr={:#x}, paddr={:#x}", vaddr, paddr);
        (vaddr, paddr.as_usize())
    }

    fn dma_free_coherent(vaddr: usize, pages: usize) {
        global_allocator().dealloc_pages(vaddr, pages, UsageKind::Dma);
    }

    fn dma_request_irq(_irq: usize, _handler: fn()) {
        log::warn!("unimplemented dma_request_irq for fxmac");
    }
}

struct FxmacDriver;

impl DeviceDriver for FxmacDriver {
    fn name(&self) -> &'static str {
        "fxmac"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &FXMAC
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        let mmio = crate::first_mmio_resource(device.as_ref()).map_err(|err| {
            log::error!(
                "fxmac: no MMIO resource for {:?}; refusing to fall back to address 0",
                device.location(),
            );
            err
        })?;
        let mapped_regs = mmio.base;
        match net::fxmac::FXmacNic::init(mapped_regs) {
            Ok(dev) => kclass::publish_net(device, Box::new(dev)).map(drop),
            Err(_) => Err(DriverError::Io),
        }
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(FxmacDriver)
}
