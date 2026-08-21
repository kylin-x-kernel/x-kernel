// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(feature = "smp")]
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kerrno::{KError, KResult};
#[cfg(feature = "smp")]
use khal::mem::pa;
use khal::mem::{PhysAddr, VirtAddr};
use kplat::sys::SysCtrl;

/// The GED sleep-control register (ACPI 5.0 §4.8.3.7 on HW-reduced
/// platforms), mapped through the kernel device mapper.
///
/// This is the machine's power-off interface: on this platform the register
/// lives in the GED MMIO block, and one byte write naming S5 with `SLP_EN`
/// set asks the platform to remove power. The device tree's
/// `syscon-poweroff` node declares both the address and the whole byte to
/// write — with QEMU's `SLP_TYP` of 5 that byte is `0x34`.
struct SleepControlRegister {
    vaddr: VirtAddr,
}

impl SleepControlRegister {
    /// Maps the one-byte register at `paddr`.
    fn map(paddr: PhysAddr) -> Self {
        let vaddr = memspace::iomap_device(paddr, 1, "acpi-sleep-ctl")
            .unwrap_or_else(|err| panic!("failed to map ACPI sleep-control register: {err:?}"));
        Self { vaddr }
    }

    /// Writes the request byte: a single volatile byte store naming S5 with
    /// `SLP_EN` set.
    fn write(&self, value: u8) {
        // SAFETY: `vaddr` was produced by `iomap_device` for exactly this
        // one-byte register, and a single volatile byte store is the access
        // this register is specified to accept.
        unsafe { self.vaddr.as_mut_ptr().write_volatile(value) }
    }
}

/// The device-tree `syscon-poweroff` declaration, read at terminal time.
///
/// Walking the tree takes no locks and allocates nothing — it is a
/// read-only pass over the static DTB bytes — so the bare terminal consults
/// the firmware declaration directly. QEMU's direct-kernel-boot handover
/// carries no ACPI tables, but its DTB names the GED sleep-control register
/// and the S5 byte, so the firmware (not this crate) owns both values.
fn fdt_poweroff() -> Option<(PhysAddr, u8)> {
    of::syscon_poweroff().map(|control| (control.paddr, control.value as u8))
}

#[impl_dev_interface]
impl SysCtrl {
    #[cfg(feature = "smp")]
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top_paddr: usize) -> KResult {
        let raw_cpu_id = raw_cpu_id(logical_cpu_id).unwrap_or_else(|| {
            panic!(
                "missing raw CPU id mapping for logical CPU {}",
                logical_cpu_id.as_usize()
            )
        });
        crate::mp::start_secondary_cpu(raw_cpu_id, pa!(stack_top_paddr));
        Ok(())
    }

    fn power_off() -> ! {
        info!("Shutting down...");
        // Firmware declares the S5 request; this crate carries no
        // machine-defined values. A boot whose firmware declares nothing
        // degrades to a halt with power kept.
        let Some((paddr, value)) = fdt_poweroff() else {
            warn!("no firmware S5 declaration; halting instead of powering off");
            karch::stop_cpu()
        };
        SleepControlRegister::map(paddr).write(value);
        warn!("S5 request returned; halting the current CPU");
        karch::stop_cpu()
    }

    fn halt() -> ! {
        info!("Halting system...");
        // Halt must not touch the GED shutdown register: mask local interrupts
        // and park the calling CPU in the idle instruction, leaving the system
        // powered.
        karch::stop_cpu()
    }

    fn suspend_to_ram() -> KResult {
        // The GED sleep-control register could express S3 here, but no
        // wakeup path exists.
        Err(KError::OperationNotSupported)
    }
}
