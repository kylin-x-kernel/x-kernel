// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, format, sync::Arc};

use driver_base::{Device, DeviceKind};
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};

use crate::driver_registry::{BoxedDriver, firmware_specs::AHCI};

struct AhciHalImpl;

impl block::ahci::AhciHal for AhciHalImpl {
    fn virt_to_phys(va: usize) -> usize {
        khal::mem::v2p(va.into()).as_usize()
    }

    fn current_ms() -> u64 {
        khal::time::monotonic_time().as_nanos_u64_saturating() / ktime_types::NANOS_PER_MILLIS
    }

    fn flush_dcache() {
        // Orders the command-list/command-table stores before the DMA engine
        // reads them; executes `dbar 0` on LoongArch64 and is a no-op on
        // cache-coherent architectures.
        karch::dma_read_barrier();
    }
}

struct AhciDriver;

impl DeviceDriver for AhciDriver {
    fn name(&self) -> &'static str {
        "ahci"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &AHCI
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        let (vaddr, _) = crate::iomap_first_mmio(device.as_ref(), "ahci")?;
        let vaddr = vaddr.as_ptr() as usize;

        // SAFETY: `iomap_first_mmio` returned a kernel virtual address that
        // maps the device's first MMIO region described by firmware and
        // installed in `memspace`. The mapping is exclusively owned by this
        // driver instance and lives at least as long as `device`, so the
        // HBA registers reachable from `vaddr` satisfy `AhciDriver::new`'s
        // precondition of pointing to a valid, exclusively-accessible MMIO
        // window.
        let ahci = match unsafe { block::ahci::AhciDriver::<AhciHalImpl>::new(vaddr) } {
            Ok(d) => d,
            Err(e) => return Err(e),
        };
        let (index, first_minor) = super::allocate_scsi_disk()?;
        let name = format!("{}{}", ahci.name(), index);
        let disk = block::Gendisk::new(name, 8, first_minor, 16, Box::new(ahci))?;
        kclass::publish_block(device, Arc::new(disk)).map(drop)
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(AhciDriver)
}
