// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU Local APIC lifecycle and local interrupt operations.
//!
//! The boot CPU installs its handle through [`init_local_primary`], every
//! application CPU through [`init_secondary`]. The handle lives in a per-CPU
//! slot for the kernel lifetime. Startup consumes its exclusive handle before
//! publication; runtime borrows only a shared register handle. Source-specific
//! operations serialize hardware state. xAPIC NMI reentry uses low-word-only
//! shorthand writes; x2APIC submits each complete command with one MSR write.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kalloc_types::{AllocFlags, KBox};
use kcpu_id_map::RawCpuId;
use kcpu_slot::cpu_slot;
use kerrno::{KError, KResult};
use kspin::NoPreemptIrqSave;
use memaddr::pa;
use memspace::iomap_device;
use x2apic::lapic::{
    IpiCommand, IpiSendError, IpiTarget, LocalApic, LocalApicBuilder, RuntimeLocalApic, xapic_base,
};
use x86_64::instructions::port::Port;

use crate::{APIC_ERROR_VECTOR, APIC_SPURIOUS_VECTOR, APIC_TIMER_VECTOR};

cpu_slot! {
    static LOCAL_APIC_PTR: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);
}

static IS_X2APIC: AtomicBool = AtomicBool::new(false);
static XAPIC_BASE: AtomicU64 = AtomicU64::new(0);

/// Changes only this prepared CPU's LAPIC timer LVT mask. The source owner must
/// exclude competing timer programming; this does not disarm or drain a timer.
pub fn set_local_timer_enabled(enabled: bool) {
    with_local_apic(|lapic| {
        // SAFETY: caller owns local timer programming; the helper pins this
        // prepared CPU and excludes ordinary timer IRQ reentry.
        unsafe { lapic.set_timer_enabled(enabled) };
    });
}

/// Prepares this CPU's owned timer as one-shot, divide-by-one, then enables it.
/// Requires local timer-source ownership; normal task/IRQ context is accepted.
/// Does not sleep. Panics if the local APIC has not been prepared.
pub fn prepare_local_timer() {
    with_local_apic(|lapic| {
        // SAFETY: the timer driver owns this CPU's timer source; ordinary IRQs
        // and migration are excluded for the whole local setup transaction.
        unsafe { lapic.prepare_timer() };
        // SAFETY: same source ownership; the timer handler is installed.
        unsafe { lapic.set_timer_enabled(true) };
    });
}

/// Programs this CPU's owned timer countdown; zero stops it.
/// Requires timer-source ownership. Runs pinned with ordinary IRQs excluded,
/// without sleeping. Panics if local APIC preparation is missing.
pub fn set_local_timer_initial(initial: u32) {
    with_local_apic(|lapic| {
        // SAFETY: caller owns the local timer and this helper retains exclusion.
        unsafe { lapic.set_timer_initial(initial) };
    });
}

// All runtime access is shared. Register metadata never changes after
// publication, and operations cannot expose or reconstruct an exclusive borrow.
fn with_local_apic<R>(f: impl FnOnce(&RuntimeLocalApic) -> R) -> R {
    let _irq_guard = NoPreemptIrqSave::new();
    let lapic = local_apic_ptr();
    assert!(
        !lapic.is_null(),
        "local APIC is not initialized on this CPU"
    );
    // SAFETY: boot publishes a leaked immutable runtime owner on this CPU.
    // The guard pins lookup and access; NMI reentry only creates shared borrows.
    unsafe { f(&*lapic) }
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

/// Reconstructs this CPU's LAPIC after reset without replacing its runtime owner.
/// Stops/masks the timer and LINT inputs, restores fixed vectors, then enables
/// error delivery. Clock recovery must reconfigure and rearm the timer later.
/// Does not allocate or sleep. Panics if local APIC preparation is missing.
///
/// # Safety
/// Call on the pinned owner CPU with IRQs disabled after restoring APIC-base
/// access, the permanent xAPIC mapping and CPU-local storage. The sleep owner
/// must quiesce NMI producers and exclude all LAPIC operations through recovery;
/// no ICR writer may span reset. The error handler must already be ready.
pub unsafe fn restore_local_after_reset() {
    let _guard = NoPreemptIrqSave::new();
    assert!(
        !local_apic_ptr().is_null(),
        "local APIC is not initialized on this CPU"
    );
    // Reset recovery temporarily owns all hardware sources. Rebuild register
    // metadata on the stack; the published immutable runtime owner stays live.
    let mut lapic = build_local_apic();
    // SAFETY: caller restored local access and excludes every source/sender.
    unsafe { lapic.enable_masked() };
    // SAFETY: masked preparation completed and the error handler is ready.
    unsafe { lapic.enable_error() };
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

// Startup and NMI sends have a bounded hardware polling budget. Ordinary fixed
// IPIs wait for idle like Linux; no sender waits for a software ICR owner.
const MAX_ICR_SEND_POLLS: usize = 1_000_000;

fn send_fixed_ipi(vector: u8, target: IpiTarget) -> KResult {
    with_local_apic(|lapic| {
        // SAFETY: normal callers supply installed fixed-vector handlers. The
        // helper pins the published runtime owner and excludes ordinary IRQs;
        // xAPIC NMI senders use only shorthand and complete before returning.
        // The vendor encoder rejects unrepresentable targets before MMIO.
        unsafe { lapic.send_fixed_ipi(vector, target) }.map_err(|error| match error {
            IpiSendError::InvalidDestination => KError::InvalidInput,
            IpiSendError::Busy => KError::ResourceBusy,
        })
    })
}

fn submit_command(
    lapic: &RuntimeLocalApic,
    command: IpiCommand,
    target: IpiTarget,
    max_polls: usize,
) -> KResult {
    for _ in 0..max_polls {
        // SAFETY: callers retain the live CPU-local runtime handle, CPU
        // pinning and IRQ exclusion. Startup callers own targets/trampolines;
        // NMI callers establish handlers and exclude xAPIC directed reentry.
        match unsafe { lapic.try_send_ipi(command, target) } {
            Ok(()) => return Ok(()),
            Err(IpiSendError::Busy) => core::hint::spin_loop(),
            Err(IpiSendError::InvalidDestination) => return Err(KError::InvalidInput),
        }
    }
    Err(KError::ResourceBusy)
}

// Poll only autonomous hardware progress after submission, never a software
// owner that could be suspended by this CPU's NMI entry.
fn finish_command(lapic: &RuntimeLocalApic, max_polls: usize) -> KResult {
    for _ in 0..max_polls {
        // SAFETY: production install_local_apic leaks the immutable owner and
        // Release-publishes it; local_apic_ptr's Acquire lookup supplies this
        // kernel-lifetime reference. with_local_apic's NoPreemptIrqSave or the
        // NMI caller's required pinning/IRQ exclusion retains the owner CPU.
        // S3 quiesces LAPIC users and NMI producers before reset and retains
        // the permanent register mapping, excluding reset during this read.
        // Tests instead retain a private aligned mock page and runtime handle
        // on their stack under NoPreemptIrqSave for the entire read sequence.
        if !unsafe { lapic.delivery_pending() } {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(KError::TimedOut)
}

fn raw_ipi_target(target: RawCpuId) -> KResult<IpiTarget> {
    let id = u32::try_from(target.as_usize()).map_err(|_| KError::InvalidInput)?;
    Ok(IpiTarget::Destination(id))
}

/// Sends a fixed IPI to this prepared CPU in normal task/IRQ context.
/// Retains CPU/IRQ exclusion and waits for the previous xAPIC command without
/// a timeout. Does not sleep or wait for the target handler to execute.
pub fn send_ipi_self(interrupt_id: usize) {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: ThisCpu is shorthand with no destination \
                  field to reject; the fixed-send implementation waits for hardware idle rather \
                  than returning Busy, so this operation has no error result"
    )]
    send_fixed_ipi(interrupt_id as u8, IpiTarget::ThisCpu).expect("self IPI shorthand failed");
}

/// Sends a fixed IPI to a raw APIC destination in normal task/IRQ context.
/// Retains CPU/IRQ exclusion and waits for the previous xAPIC command without
/// a timeout; does not sleep or wait for the target handler to execute.
///
/// # Errors
/// Returns InvalidInput before any ICR write when `target` exceeds 8 bits in
/// xAPIC mode or 32 bits in x2APIC mode. The rejected command is not submitted.
pub fn send_ipi_raw(interrupt_id: usize, target: RawCpuId) -> KResult {
    send_fixed_ipi(interrupt_id as u8, raw_ipi_target(target)?)
}

/// Broadcasts a fixed IPI to every other CPU from normal task/IRQ context.
/// Retains CPU/IRQ exclusion and waits for the previous xAPIC command without
/// a timeout. Does not sleep or wait for remote handlers to execute.
pub fn send_ipi_all_but_self(interrupt_id: usize) {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: AllExcludingSelf is shorthand with no \
                  destination field to reject; the fixed-send implementation waits for hardware \
                  idle rather than returning Busy, so no error can be returned"
    )]
    send_fixed_ipi(interrupt_id as u8, IpiTarget::AllExcludingSelf)
        .expect("broadcast IPI shorthand failed");
}

fn send_startup_command(command: IpiCommand, target: IpiTarget) -> KResult {
    with_local_apic(|lapic| {
        submit_command(lapic, command, target, MAX_ICR_SEND_POLLS)?;
        finish_command(lapic, MAX_ICR_SEND_POLLS)
    })
}

/// Sends INIT to a validated AP startup destination during serialized bring-up.
/// Does not sleep. Waits for local submission/completion with a bounded budget.
///
/// # Safety
/// The caller owns serialized AP bring-up and has prepared the target's startup
/// memory/state. This must not reset an already executing CPU.
///
/// # Errors
/// InvalidInput rejects an unencodable target; ResourceBusy means no command
/// was submitted. TimedOut means submission occurred but may still be pending.
pub unsafe fn send_init_ipi_raw(target: RawCpuId) -> KResult {
    let target = u32::try_from(target.as_usize()).map_err(|_| KError::InvalidInput)?;
    send_startup_command(IpiCommand::Init, IpiTarget::Destination(target))
}

/// Resets every other CPU during serialized startup or sleep recovery.
/// Does not sleep. Waits for local submission/completion with a bounded budget.
///
/// # Safety
/// The caller must own every remote CPU's startup/recovery state and retained
/// execution context. No remote CPU may be executing ordinary kernel work.
///
/// # Errors
/// ResourceBusy means no command was submitted. TimedOut means the submitted
/// command may remain pending; callers must retain all recovery resources.
pub unsafe fn send_init_ipi_all_but_self() -> KResult {
    send_startup_command(IpiCommand::Init, IpiTarget::AllExcludingSelf)
}

/// Sends SIPI to a validated AP using a prepared, live low-memory startup page.
/// Requires serialized AP bring-up. Does not sleep; waits with a bounded budget.
///
/// # Safety
/// The target must be awaiting SIPI and `page` must name live, prepared AP
/// startup code whose memory and stack remain valid through CPU initialization.
///
/// # Errors
/// InvalidInput rejects an unencodable target; ResourceBusy means no command
/// was submitted. TimedOut means the submitted command may still be pending.
pub unsafe fn send_startup_ipi_raw(page: u8, target: RawCpuId) -> KResult {
    let target = u32::try_from(target.as_usize()).map_err(|_| KError::InvalidInput)?;
    send_startup_command(IpiCommand::Startup(page), IpiTarget::Destination(target))
}

/// Sends a directed NMI with bounded hardware-idle/completion polling.
/// Does not allocate or wait for a software owner. xAPIC requires ordinary
/// context; x2APIC also supports NMI context through one complete MSR write.
///
/// # Safety
/// Call only on a prepared, pinned CPU with ordinary IRQs masked, including
/// NMI context. Every target CPU must have a ready NMI entry and source handler.
/// Hardware NMI callers must already be inside kirq's effective NMI scope so
/// the xAPIC directed-send context check observes their entry correctly.
/// If completion times out in xAPIC NMI context, retain the interrupted sender
/// until delivery becomes idle or the source owner selects terminal recovery.
///
/// # Errors
/// BadState means this CPU has no published APIC. ResourceBusy means hardware
/// delivery stayed busy; no ICR write occurred. InvalidInput
/// rejects a destination wider than the selected mode can encode. xAPIC NMI
/// context returns OperationNotSupported without a write. TimedOut means a
/// submitted command may still be pending; the error does not cancel delivery.
pub unsafe fn try_send_nmi_raw(target: RawCpuId) -> KResult<()> {
    let destination = u32::try_from(target.as_usize()).map_err(|_| KError::InvalidInput)?;
    // SAFETY: the caller supplies the NMI entry, pinning and target contract.
    unsafe { try_send_nmi(IpiTarget::Destination(destination)) }
}

/// Sends an NMI broadcast with bounded hardware-only polling. xAPIC uses a
/// low-word-only shorthand write, preserving an interrupted sender's target.
/// Never waits for a software owner. Usable in ordinary and NMI context.
///
/// # Safety
/// Same as [`try_send_nmi_raw`], for every other CPU, including booting CPUs.
///
/// # Errors
/// BadState or ResourceBusy means no command was submitted. TimedOut means a
/// submitted command may still be pending; callers retain recovery ownership.
pub unsafe fn try_send_nmi_all_but_self() -> KResult<()> {
    // SAFETY: the caller validated the entry contract on every remote CPU.
    unsafe { try_send_nmi(IpiTarget::AllExcludingSelf) }
}

/// Submits a single already-validated NMI destination.
///
/// # Safety
/// Caller retains CPU pinning and IRQ exclusion and establishes ready NMI
/// handlers on every destination, as required by the public NMI helpers.
unsafe fn try_send_nmi(target: IpiTarget) -> KResult<()> {
    let _guard = kspin::NoPreempt::new();
    let pointer = local_apic_ptr();
    if pointer.is_null() {
        return Err(KError::BadState);
    }
    if !IS_X2APIC.load(Ordering::Relaxed)
        && matches!(target, IpiTarget::Destination(_))
        && kirq::context::is_in_nmi()
    {
        return Err(KError::OperationNotSupported);
    }
    // SAFETY: this pinned CPU published a permanent immutable runtime owner.
    let lapic = unsafe { &*pointer };
    submit_command(lapic, IpiCommand::Nmi, target, MAX_ICR_SEND_POLLS)?;
    // Only autonomous hardware progress is awaited, never an interrupted
    // software owner.
    finish_command(lapic, MAX_ICR_SEND_POLLS)
}

/// Sets the exclusively owned local performance LVT to NMI delivery.
///
/// # Safety
/// Call pinned with ordinary IRQs excluded, including NMI context, after LAPIC
/// initialization and validation of LVTPC support. The caller must own LVTPC
/// and its producer and serialize initial setup, quiescence and rearming.
/// This operation does not stop counters. Panics on missing LAPIC preparation.
pub unsafe fn set_local_performance_nmi_enabled(enabled: bool) {
    let _guard = kspin::NoPreempt::new();
    let pointer = local_apic_ptr();
    assert!(
        !pointer.is_null(),
        "local APIC is not initialized on this CPU"
    );
    // SAFETY: the pinned CPU owns the leaked immutable runtime handle.
    let lapic = unsafe { &*pointer };
    // SAFETY: caller supplies source ownership, capability and setup ordering.
    unsafe { lapic.set_performance_nmi_enabled(enabled) };
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
    prepare: impl FnOnce(RuntimeLocalApic, AllocFlags) -> KResult<KBox<RuntimeLocalApic>>,
) -> KResult<()> {
    let owned = prepare(lapic.into_runtime(), AllocFlags::NOWAIT)?;
    let _guard = NoPreemptIrqSave::new();
    let slot = LOCAL_APIC_PTR.current_shared();
    assert_eq!(slot.load(Ordering::Relaxed), 0);
    // Release publishes the fully initialized immutable runtime handle to NMI
    // lookup. An NMI arriving before publication sees no local owner and fails.
    slot.store(KBox::into_raw(owned) as usize, Ordering::Release);
    Ok(())
}

fn local_apic_ptr() -> *const RuntimeLocalApic {
    // Caller retains CPU pinning. Acquire observes initialization before the
    // unique Release publication; only shared runtime borrows follow it.
    LOCAL_APIC_PTR.current_shared().load(Ordering::Acquire) as *const RuntimeLocalApic
}
