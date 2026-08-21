// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Power control implementation for kplat-x86_64.

#[cfg(feature = "smp")]
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kerrno::{KError, KResult};
use kplat::sys::SysCtrl;
use x86_64::instructions::port::PortWriteOnly;

/// PM1 control register field positions (ACPI 5.0 §4.8.3.2): `SLP_TYP`
/// occupies bits [12:10], `SLP_EN` is bit 13.
const SLP_TYP_SHIFT: u16 = 10;
const SLP_EN: u16 = 1 << 13;

/// The S5 `SLP_TYP` this machine's firmware expects in the PM1a control
/// block (QEMU declares `0`).
///
/// A full ACPI implementation derives this from the `\_S5` package in the
/// DSDT, which requires an AML interpreter; until one exists the value is
/// machine-defined, exactly like the fixed `0x2000` write the platform
/// carried before the PM1a port was read from the FADT.
const S5_SLP_TYP: u8 = 0;

/// Resolves the ACPI PM1a control-block port from the FADT.
///
/// `None` means this boot handed over no usable FADT. There is deliberately
/// no machine-defined fallback port: every supported boot path (LinuxBoot,
/// UEFI) provides ACPI tables, so a missing FADT is a boot-protocol gap, not
/// a machine variant to paper over.
fn pm1a_control_port() -> Option<u16> {
    match acpi::find_pm1a_control_block_from_init() {
        Some(block) if block.length >= 2 => Some(block.port),
        _ => {
            warn!("FADT PM1a control block unavailable; cannot power off");
            None
        }
    }
}

#[impl_dev_interface]
impl SysCtrl {
    #[cfg(feature = "smp")]
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top_paddr: usize) -> KResult {
        use khal::mem::pa;

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
        // The register comes from the firmware declaration (FADT PM1a
        // control block); the sleep type is the machine constant above. A
        // boot without a FADT degrades to a halt with power kept.
        let Some(port) = pm1a_control_port() else {
            warn!("no FADT PM1a control port; halting instead of powering off");
            karch::stop_cpu()
        };
        let value = (u16::from(S5_SLP_TYP) << SLP_TYP_SHIFT) | SLP_EN;
        // SAFETY: `port` was taken from the validated FADT PM1a control
        // block, and a single 16-bit port write is the access this register
        // is specified to accept.
        unsafe { PortWriteOnly::new(port).write(value) };
        warn!("S5 request returned; halting the current CPU");
        karch::stop_cpu()
    }

    fn halt() -> ! {
        info!("Halting system...");
        // Halt must not touch the ACPI power-management registers: mask
        // local interrupts and park the calling CPU in HLT, leaving the
        // system powered.
        karch::stop_cpu()
    }

    fn suspend_to_ram() -> KResult {
        // Entering S3 needs the `\_S3` `SLP_TYP` from the DSDT (an AML
        // interpreter) plus a resume path; neither exists yet.
        Err(KError::OperationNotSupported)
    }
}
