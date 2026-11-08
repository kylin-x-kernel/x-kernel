// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU Local APIC lifecycle and local interrupt operations.
//!
//! The boot CPU installs its handle through [`init_local_primary`], every
//! application CPU through [`init_secondary`]. The handle lives in a per-CPU
//! slot for the kernel lifetime; [`with_local_apic`] is the single entry point
//! that pins the CPU, excludes local IRQs, and lends the handle out.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kalloc_types::{AllocFlags, KBox};
use kcpu_id_map::RawCpuId;
use kcpu_slot::{CpuLocalGuard, cpu_slot_cell};
use kerrno::KResult;
use kspin::NoPreemptIrqSave;
use memaddr::pa;
use memspace::iomap_device;
use x2apic::lapic::{LocalApic, LocalApicBuilder, xapic_base};
use x86_64::instructions::port::Port;

use crate::{APIC_ERROR_VECTOR, APIC_SPURIOUS_VECTOR, APIC_TIMER_VECTOR};

cpu_slot_cell! {
    static LOCAL_APIC_PTR: usize = 0;
}

static IS_X2APIC: AtomicBool = AtomicBool::new(false);
static XAPIC_BASE: AtomicU64 = AtomicU64::new(0);

/// Changes only this prepared CPU's LAPIC timer LVT mask. The source owner must
/// exclude competing timer programming; this does not disarm or drain a timer.
pub fn set_local_timer_enabled(enabled: bool) {
    with_local_apic(|lapic| {
        if enabled {
            // SAFETY: with_local_apic selects the prepared CPU-local controller
            // and retains IRQ exclusion; only its timer LVT mask is modified.
            unsafe { lapic.enable_timer() };
        } else {
            // SAFETY: with_local_apic selects this CPU's prepared, live LAPIC
            // and retains IRQ exclusion, serializing the timer LVT mask write.
            unsafe { lapic.disable_timer() };
        }
    });
}

/// Runs `f` with exclusive access to the current CPU's Local APIC.
///
/// # Panics
///
/// Panics if the current CPU has not installed its Local APIC handle
/// (i.e. `init_local_primary`/`init_secondary` has not run on this CPU).
pub fn with_local_apic<R>(f: impl FnOnce(&mut LocalApic) -> R) -> R {
    let irq_guard = NoPreemptIrqSave::new();
    let lapic = local_apic_ptr(&irq_guard);
    assert!(
        !lapic.is_null(),
        "local APIC is not initialized on this CPU"
    );
    // SAFETY: each CPU installs exactly one leaked `LocalApic` handle into its
    // own per-CPU slot during `init_local_primary`/`init_secondary`. Local IRQs stay
    // masked for the duration of this borrow, so the current CPU cannot re-enter
    // LAPIC access through an interrupt handler and create overlapping mutable
    // borrows to the same object.
    unsafe { f(&mut *lapic) }
}

/// Signals end-of-interrupt on the current CPU's Local APIC.
pub fn end_of_interrupt() {
    with_local_apic(|lapic| {
        // SAFETY: the current CPU's LAPIC handle is initialized before IRQ
        // dispatch, and EOI targets only that CPU's local APIC registers.
        unsafe { lapic.end_of_interrupt() };
    });
}

/// Captures ESR and completes the current CPU's in-service APIC error vector.
///
/// The IRQ backend calls this only for `APIC_ERROR_VECTOR`, instead of generic
/// Action dispatch. Do not issue another EOI for the same delivery.
pub fn handle_error_interrupt() {
    let flags = with_local_apic(|lapic| {
        // SAFETY: IRQ dispatch owns this in-service error event; the helper
        // pins the CPU and excludes local IRQ reentry into the installed APIC.
        unsafe { lapic.acknowledge_error() }
    });
    debug!("Local APIC error: {flags:?}");
}

/// Converts an 8-bit APIC id into the destination-id encoding used by
/// this platform: x2APIC uses the value directly, xAPIC shifts it into
/// the destination field of the redirect/IPI formats.
pub fn raw_apic_id(id_u8: u8) -> u32 {
    if IS_X2APIC.load(Ordering::Relaxed) {
        id_u8 as u32
    } else {
        (id_u8 as u32) << 24
    }
}

fn cpu_has_x2apic() -> bool {
    match raw_cpuid::CpuId::new().get_feature_info() {
        Some(finfo) => finfo.has_x2apic(),
        None => false,
    }
}

/// Brings up the boot CPU's Local APIC.
///
/// Masks the legacy PIC, selects x2APIC or xAPIC from CPUID, programs the
/// timer/error/spurious vectors, maps the xAPIC MMIO page when needed, and
/// publishes the CPU-local handle.
/// The LAPIC timer stays stopped and masked until the timer driver activates it.
/// LINT0/LINT1 stay masked; this platform has no LINT routing consumer.
/// Requires boot CPU context with IRQs and migration disabled. The kernel-lifetime
/// handle uses NOWAIT; no reclaim or sleeping allocation is permitted here.
///
/// # Errors
/// Owner allocation returns NoMemory before publishing the per-CPU handle or
/// enabling error delivery. MMIO errors propagate as InvalidInput, Io or NoMemory.
///
/// # Panics
///
/// Panics if this CPU is initialized twice or fixed builder invariants fail.
pub fn init_local_primary() -> KResult<()> {
    info!("Initialize Local APIC...");
    // SAFETY: CPL0 boot owns the master PIC data port; 0xff masks its legacy interrupt inputs.
    unsafe { Port::<u8>::new(0x21).write(0xff) };
    // SAFETY: CPL0 boot also owns the slave PIC data port; mask its inputs before APIC routing.
    unsafe { Port::<u8>::new(0xA1).write(0xff) };
    let mut builder = LocalApicBuilder::new();
    builder
        .timer_vector(APIC_TIMER_VECTOR as _)
        .error_vector(APIC_ERROR_VECTOR as _)
        .spurious_vector(APIC_SPURIOUS_VECTOR as _);
    if cpu_has_x2apic() {
        info!("Using x2APIC.");
        IS_X2APIC.store(true, Ordering::Relaxed);
    } else {
        info!("Using xAPIC.");
        // SAFETY: on the xAPIC path, `xapic_base` reads the architectural LAPIC
        // base address, which is then mapped before the builder uses it.
        let base_vaddr = iomap_device(pa!(unsafe { xapic_base() } as usize), 0x1000, "lapic")?;
        XAPIC_BASE.store(base_vaddr.as_usize() as u64, Ordering::Relaxed);
        builder.set_xapic_base(base_vaddr.as_usize() as u64);
    }
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: all required timer, error and spurious \
                  vectors are supplied, and the xAPIC path supplies the mapped base"
    )]
    let mut lapic = builder.build().unwrap();
    // SAFETY: Boot excludes IRQs and migration, this handle exclusively owns
    // the current CPU's hardware-enabled LAPIC, and its xAPIC mapping (if any)
    // is valid. No NMI handler accesses the handle during preparation.
    unsafe { lapic.enable_masked() };
    #[cfg(unittest)]
    tests_preparation::verify_masked_preparation();
    install_local_apic(lapic)?;
    enable_error_delivery();
    Ok(())
}

/// Brings up the Local APIC on an application CPU (spurious/error/timer
/// vectors only; the IO-APIC is owned by the boot CPU).
/// Leaves the LAPIC timer stopped and timer/LINT sources masked, as on the BSP.
/// Requires AP boot context with IRQs and migration disabled; owner allocation
/// uses NOWAIT, not sleeping reclaim.
///
/// # Errors
/// NoMemory leaves the handle unpublished and error delivery masked. Hardware
/// preparation has already happened; the platform must fail this CPU's boot.
///
/// # Panics
/// Panics on missing BSP preparation or duplicate per-CPU initialization.
pub fn init_secondary() -> KResult<()> {
    let mut lapic = build_local_apic();
    // SAFETY: AP bring-up excludes IRQs and migration and uses the supported
    // APIC mode/mapping established by the BSP. This CPU exclusively owns its
    // hardware-enabled LAPIC; no NMI handler accesses this unpublished handle.
    unsafe { lapic.enable_masked() };
    #[cfg(unittest)]
    tests_preparation::verify_masked_preparation();
    install_local_apic(lapic)?;
    enable_error_delivery();
    Ok(())
}

fn enable_error_delivery() {
    // The fixed error handler is statically installed in irq-driver, and the
    // per-CPU handle is now published. This unmasks LVTERR, but BSP/AP startup
    // keeps EFLAGS.IF clear until runtime enables local IRQs. The IDT alone
    // does not allow delivery; timer/LINT retain independent activation policy.
    with_local_apic(|lapic| {
        // SAFETY: Masked preparation completed; this borrow pins/excludes
        // local IRQs and the fixed error handler can now find its APIC handle.
        unsafe { lapic.enable_error() };
    });
}

unittest::path_mod_test!(tests_preparation, "../ut-tests/preparation.rs");

/// Sends an IPI to the current CPU with the given interrupt id.
pub fn send_ipi_self(interrupt_id: usize) {
    with_local_apic(|lapic| {
        // SAFETY: the current CPU's LAPIC handle is initialized before self-IPIs
        // are used, and the x2apic crate requires `&mut self` only to model the
        // CPU-local register programming performed here.
        unsafe { lapic.send_ipi_self(interrupt_id as _) };
    });
}

/// Sends an IPI to the CPU identified by `target_raw_apic_id`.
pub fn send_ipi_raw(interrupt_id: usize, target_raw_apic_id: RawCpuId) {
    let apic_id = raw_apic_id(target_raw_apic_id.as_usize() as u8);
    with_local_apic(|lapic| {
        // SAFETY: the current CPU's LAPIC handle is initialized before cross-CPU
        // IPIs are used, and programming the destination APIC ID touches only
        // this CPU's local APIC command register.
        unsafe { lapic.send_ipi(interrupt_id as _, apic_id as _) };
    });
}

/// Broadcasts an IPI to every other CPU.
pub fn send_ipi_all_but_self(interrupt_id: usize) {
    use x2apic::lapic::IpiAllShorthand;
    with_local_apic(|lapic| {
        // SAFETY: the current CPU's LAPIC handle is initialized before broadcast
        // IPIs are used, and the shorthand command programs only the local ICR.
        unsafe { lapic.send_ipi_all(interrupt_id as _, IpiAllShorthand::AllExcludingSelf) };
    });
}

fn build_local_apic() -> LocalApic {
    let mut builder = LocalApicBuilder::new();
    builder
        .timer_vector(APIC_TIMER_VECTOR as _)
        .error_vector(APIC_ERROR_VECTOR as _)
        .spurious_vector(APIC_SPURIOUS_VECTOR as _);
    if !IS_X2APIC.load(Ordering::Relaxed) {
        let xapic_base = XAPIC_BASE.load(Ordering::Relaxed);
        assert!(xapic_base != 0, "xAPIC base is not initialized");
        builder.set_xapic_base(xapic_base);
    }
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: The builder receives valid fixed APIC \
                  vectors and, for xAPIC, an asserted nonzero MMIO base"
    )]
    builder.build().expect("failed to build local APIC")
}

fn install_local_apic(lapic: LocalApic) -> KResult<()> {
    install_local_apic_with(lapic, KBox::new_with_flags)
}

fn install_local_apic_with(
    lapic: LocalApic,
    prepare: impl FnOnce(LocalApic, AllocFlags) -> KResult<KBox<LocalApic>>,
) -> KResult<()> {
    let owned = prepare(lapic, AllocFlags::NOWAIT)?;
    let guard = NoPreemptIrqSave::new();
    assert_eq!(LOCAL_APIC_PTR.read(&guard), 0);
    LOCAL_APIC_PTR.write(&guard, KBox::into_raw(owned) as usize);
    Ok(())
}

fn local_apic_ptr(pin: &impl CpuLocalGuard) -> *mut LocalApic {
    // The guard keeps execution on the CPU whose slot contains either 0 or the
    // pointer published by `install_local_apic`. The single-word load cannot
    // tear, and the leaked pointee remains valid for the kernel's life.
    LOCAL_APIC_PTR.read(pin) as *mut LocalApic
}
