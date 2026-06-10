// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

use driver_base::DeviceKind;
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};

use crate::driver_registry::{BoxedDriver, firmware_specs::AHCI};

struct AhciHalImpl;

impl block::ahci::AhciHal for AhciHalImpl {
    fn virt_to_phys(va: usize) -> usize {
        khal::mem::v2p(va.into()).as_usize()
    }

    fn current_ms() -> u64 {
        khal::time::monotonic_time_nanos() / 1_000_000
    }

    fn flush_dcache() {
        #[cfg(target_arch = "loongarch64")]
        // SAFETY: `dbar 0` is a LoongArch64 data barrier instruction that
        // ensures preceding data cache operations complete before subsequent
        // memory accesses. This is required for AHCI DMA coherency on
        // LoongArch64 where the DMA controller may observe stale cache
        // lines. The instruction is privileged but safe to execute in
        // kernel context and has no memory safety implications — it only
        // affects ordering, not addressability.
        unsafe {
            core::arch::asm!("dbar 0");
        }
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
        kclass::publish_block(device, Box::new(ahci))
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(AhciDriver)
}
