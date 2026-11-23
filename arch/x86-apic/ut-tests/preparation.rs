// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

static VERIFIED_CPUS: AtomicUsize = AtomicUsize::new(0);

fn read_register(msr: u32, offset: usize) -> u32 {
    if super::IS_X2APIC.load(Ordering::Relaxed) {
        // SAFETY: Boot selected x2APIC; callers supply only readable LAPIC MSRs.
        unsafe { x86_64::registers::model_specific::Msr::new(msr).read() as u32 }
    } else {
        let base = super::XAPIC_BASE.load(Ordering::Relaxed) as usize;
        // SAFETY: Boot mapped the LAPIC page before this check. Each offset
        // below is a readable, aligned register within that device mapping.
        unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
    }
}

pub(super) fn verify_masked_preparation() {
    // Runs on each booting CPU before handle publication or timer activation.
    // Boot assertions deliberately abort if a caller regresses to enable().
    for (msr, offset) in [
        (0x832, 0x320),
        (0x835, 0x350),
        (0x836, 0x360),
        (0x837, 0x370),
    ] {
        core::assert_ne!(read_register(msr, offset) & (1 << 16), 0);
    }
    core::assert_eq!(read_register(0x838, 0x380), 0);
    core::assert_eq!(read_register(0x832, 0x320) & (3 << 17), 0);
    core::assert_eq!(
        read_register(0x837, 0x370) & 0xff,
        u32::from(super::APIC_ERROR_VECTOR)
    );
    VERIFIED_CPUS.fetch_add(1, Ordering::Relaxed);
}

#[unittest::def_test]
fn boot_checked_masked_preparation() {
    unittest::assert!(VERIFIED_CPUS.load(Ordering::Relaxed) > 0);
}

#[unittest::def_test]
fn owner_failure_preserves_cpu_slot_and_uses_nowait() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let before = super::LOCAL_APIC_PTR
        .current_shared()
        .load(Ordering::Acquire);
    let result = super::install_local_apic_with(super::build_local_apic(), |_, flags| {
        assert_eq!(flags, kalloc_types::AllocFlags::NOWAIT);
        Err(kerrno::KError::NoMemory)
    });
    assert_eq!(result, Err(kerrno::KError::NoMemory));
    assert_eq!(
        super::LOCAL_APIC_PTR
            .current_shared()
            .load(Ordering::Acquire),
        before
    );
}

#[unittest::def_test]
fn mmio_errors_reach_the_kernel_error_boundary() {
    assert_eq!(
        kerrno::KError::from(memspace::IoMapError::InvalidRange),
        kerrno::KError::InvalidInput
    );
    assert_eq!(
        kerrno::KError::from(memspace::IoMapError::MappingFailed),
        kerrno::KError::Io
    );
    assert_eq!(
        kerrno::KError::from(memspace::IoMapError::NoMemory),
        kerrno::KError::NoMemory
    );
}

#[unittest::def_test]
fn directed_fixed_ipi_rejects_unrepresentable_ids_without_writing_icr() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let uses_x2apic = super::IS_X2APIC.load(Ordering::Relaxed);
    // The hardware busy bit can clear independently of this rejected send.
    let command_before = read_register(0x830, 0x300) & !(1 << 12);
    let destination_before = (!uses_x2apic).then(|| read_register(0x830, 0x310));
    assert_eq!(
        super::send_ipi_raw(0x55, super::RawCpuId::new(usize::MAX)),
        Err(kerrno::KError::InvalidInput)
    );
    if !uses_x2apic {
        assert_eq!(
            super::send_ipi_raw(0x55, super::RawCpuId::new(0x100)),
            Err(kerrno::KError::InvalidInput)
        );
    }
    assert_eq!(read_register(0x830, 0x300) & !(1 << 12), command_before);
    if let Some(destination) = destination_before {
        assert_eq!(read_register(0x830, 0x310), destination);
    }
}

#[unittest::def_test]
fn startup_errors_report_busy_invalid_targets_and_pending_completion() -> unittest::TestResult {
    if super::IS_X2APIC.load(Ordering::Relaxed) {
        return unittest::TestResult::Ignored;
    }
    #[repr(C, align(4096))]
    struct RegisterPage([u32; 1024]);
    let mut page = RegisterPage([0; 1024]);
    page.0[0x300 / 4] = 1 << 12;
    page.0[0x310 / 4] = 0x7700_0000;
    let _guard = kspin::NoPreemptIrqSave::new();
    let mut builder = x2apic::lapic::LocalApicBuilder::new();
    builder
        .timer_vector(0xf0)
        .error_vector(0xf2)
        .spurious_vector(0xf1)
        .set_xapic_base(page.0.as_mut_ptr() as u64);
    let lapic = builder.build().unwrap().into_runtime();
    let target = super::IpiTarget::Destination(0x23);
    assert_eq!(
        super::submit_command(&lapic, super::IpiCommand::Init, target, 2),
        Err(kerrno::KError::ResourceBusy)
    );
    assert_eq!(
        super::submit_command(
            &lapic,
            super::IpiCommand::Startup(8),
            super::IpiTarget::Destination(0x100),
            2
        ),
        Err(kerrno::KError::InvalidInput)
    );
    assert_eq!(
        super::finish_command(&lapic, 2),
        Err(kerrno::KError::TimedOut)
    );
    assert_eq!(page.0[0x300 / 4], 1 << 12);
    assert_eq!(page.0[0x310 / 4], 0x7700_0000);
    page.0[0x300 / 4] = 0;
    assert_eq!(
        super::submit_command(
            &lapic,
            super::IpiCommand::Init,
            super::IpiTarget::AllExcludingSelf,
            2
        ),
        Ok(())
    );
    assert_eq!(super::finish_command(&lapic, 2), Ok(()));
    assert_eq!(page.0[0x300 / 4], 0xc4500);
    assert_eq!(page.0[0x310 / 4], 0x7700_0000);
    unittest::TestResult::Ok
}
