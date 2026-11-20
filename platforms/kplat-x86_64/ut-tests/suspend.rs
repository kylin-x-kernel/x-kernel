// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

extern "C" fn entry_aborts(argument: *mut ()) {
    // SAFETY: the test passes its exclusively borrowed bool, which remains live
    // until the synchronous save/callback/restore operation returns.
    unsafe { argument.cast::<bool>().write(true) };
}

#[def_test(serial)]
fn aborted_sleep_restores_cpu_context_and_returns_to_saved_stack() {
    let mut context = SavedCpu::new();
    let mut called = false;
    let _critical = kspin::NoPreemptIrqSave::new();
    // SAFETY: this test keeps IRQs and migration disabled, owns stack storage,
    // and its callback simply returns without changing hardware or ownership.
    unsafe { context.save_and_call(entry_aborts, (&mut called as *mut bool).cast()) };
    assert!(called);
    assert!(!karch::local_irq_enabled());
}

core::arch::global_asm!(include_str!("suspend_registers.S"));

#[def_test(serial)]
fn clobbered_callee_saved_registers_are_restored_on_saved_stack() {
    unsafe extern "C" {
        fn s3_test_clobbered_register_roundtrip(context: *mut SavedCpu) -> u64;
    }
    let mut context = SavedCpu::new();
    let _critical = kspin::NoPreemptIrqSave::new();
    // SAFETY: exclusive aligned context storage and this stack remain live,
    // with IRQs/migration excluded. The assembly wrapper preserves the Rust
    // caller's ABI, clobbers all saved registers only across its nonreturning
    // assembly callback, and restores the original caller before returning.
    let mismatched = unsafe { s3_test_clobbered_register_roundtrip(&mut context) };
    assert_eq!(mismatched, 0, "callee-saved register restoration mask");
    assert!(!karch::local_irq_enabled());
}

extern "C" fn enter_qemu_s3(argument: *mut ()) {
    let port = argument as usize as u16;
    // SAFETY: this explicitly enabled QEMU-only bring-up test obtained the
    // PM1a control port from FADT. Type 1 is this machine's S3 declaration.
    // The context and firmware waking vector have been installed beforehand.
    unsafe {
        core::arch::asm!(
            "out dx, ax",
            "2: hlt",
            "jmp 2b",
            in("dx") port,
            in("ax") 0x2401u16,
            options(noreturn),
        );
    }
}

#[def_test(serial)]
fn firmware_s3_wake_restores_saved_execution_context() -> unittest::TestResult {
    if kcpu_id_map::nr_cpus() != 1
        || !khal::firmware::cmdline().is_some_and(|line| {
            line.split_whitespace()
                .any(|arg| arg == "xkernel.s3-context-test=1")
        })
    {
        return unittest::TestResult::Ignored;
    }
    let port = acpi::find_pm1a_control_block_from_init().unwrap().port;
    let mut context = SavedCpu::new();
    let critical = kspin::NoPreemptIrqSave::new();
    khal::quiesce_nmi();
    khal::time::disarm_timer();
    let clock = khal::time::suspend_monotonic_clock().unwrap();
    // SAFETY: the explicitly enabled single-CPU test excludes IRQs/migration
    // and other page users, and retains the context until firmware returns.
    let wake = unsafe { WakePage::install(&mut context) }.unwrap();
    unittest::ktest_println!("S3_CONTEXT: entering firmware sleep");
    // SAFETY: the wake page and boot mapping are installed; this test retains
    // the single CPU's context and stack through the non-returning S3 request.
    unsafe { context.save_and_call(enter_qemu_s3, (port as usize) as *mut ()) };
    x86_apic::with_local_apic(|lapic| {
        // SAFETY: resume restored APIC-base access, and IRQs remain disabled.
        unsafe { lapic.enable_masked() };
        // SAFETY: same restored local controller and configured error vector.
        unsafe { lapic.enable_error() };
    });
    khal::time::resume_monotonic_clock(&clock).unwrap();
    khal::time::restore_local_timer();
    khal::time::arm_timer(khal::time::monotonic_time() + ktime_types::TimeSpan::from_millis(1));
    drop(wake);
    unittest::ktest_println!("S3_CONTEXT: returned on the saved kernel stack");
    drop(critical);
    unittest::TestResult::Ok
}
