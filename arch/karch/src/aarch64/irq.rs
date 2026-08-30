// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt control for AArch64.
//!
//! When the `nmi-pseudo` feature is enabled and the GIC driver has marked
//! this CPU ready, IRQ masking uses the GIC priority mask instead of
//! `DAIF.I` — keeping pseudo‑NMIs deliverable.
//!
//! All helpers in this module modify **only** the `DAIF.I` bit (via the
//! `aarch64-cpu` `DAIF.modify` read-modify-write); the D/A/F mask bits
//! (SError, debug, FIQ) are preserved, so masking state established by
//! [`save_irq_and_disable`] or by explicit SError/debug masking is never
//! lifted as a side effect.
//!
//! # PMR‑mode DAIF.I invariant
//!
//! Once `pmr::is_ready()` returns `true` on a CPU, **`DAIF.I` must always
//! remain 0 (Unmasked)**.  Normal IRQ masking is performed exclusively by
//! PMR; `DAIF.I` is held open so pseudo‑NMIs (whose priority exceeds
//! `pmr::NMI_ONLY`) can preempt the current context.  Do **not** write
//! `DAIF.I` directly via `asm!` or raw register access in PMR mode —
//! [`disable_local_irq`] and [`enable_local_irq`] will silently overwrite
//! the bit without restoring a caller‑set value.
//!
//! The one deliberate exception is [`disable_local_exceptions`], the
//! terminal-path helper behind the park in [`super::cpu::stop_cpu`]: it
//! masks all four DAIF exception classes and is never followed by a
//! restore, so no restore path exists.

//! # Hardware‑NMI ALLINT invariant
//!
//! With FEAT_NMI enabled (`SCTLR_EL1.NMI=1`, `SPINTMASK=0`), taking any
//! exception to EL1 sets `PSTATE.ALLINT=1`, which masks IRQs and NMIs
//! alike until software opens them.  There are two opening points:
//!
//! - [`enable_local_irq`] (task / context-switch paths) clears ALLINT;
//! - the GIC IRQ dispatch path clears ALLINT for a normal (non‑NMI)
//!   interrupt after ack (`gicv3::dispatch_irq_from_irqson`), so a
//!   Superpriority NMI can preempt a normal IRQ handler while ordinary IRQs
//!   stay masked by `DAIF.I`.  The NMI path keeps ALLINT set, so NMIs do
//!   not preempt NMIs.
//!
//! Exception entry saves the *pre‑entry* PSTATE into `SPSR_EL1` before
//! setting ALLINT, so exception handling must never modify the saved SPSR;
//! `ERET` restores the interrupted context's exact PSTATE.
//! [`local_irq_enabled`] reports ALLINT as part of the effective mask state.

use core::arch::asm;

use aarch64_cpu::registers::{DAIF, ReadWriteable, Readable, Writeable};

/// Token bit that marks a [`save_irq_and_disable`] state as a PMR value
/// rather than a raw `DAIF` value.
#[cfg(feature = "nmi-pseudo")]
const TOKEN_BIT: usize = 1 << 31;
/// Bit mask covering the 8‑bit PMR payload inside a save token.
#[cfg(feature = "nmi-pseudo")]
const TOKEN_MASK: usize = 0xFF;
/// `PSTATE.ALLINT`, bit 13 of the ALLINT system register.  Only meaningful
/// when FEAT_NMI is enabled (`SCTLR_ELx.NMI=1`).
#[cfg(feature = "nmi-hardware")]
const ALLINT_MASK: u64 = 1 << 13;

/// Per‑CPU: hardware‑NMI (ALLINT) mechanism active on this CPU.
///
/// Set by the platform's NMI `late_init` when `detect_mode()` confirmed
/// `NmiMode::Hardware`; every `ALLINT` register access is gated on it so a
/// degraded build (CPU without FEAT_NMI) never touches the register
/// (UNDEFINED).  The NMI mode itself lives in the platform; this is only a
/// derived readiness flag, mirroring [`pmr::READY`].
#[cfg(feature = "nmi-hardware")]
#[percpu::def_percpu]
static ALLINT_ACTIVE: bool = false;

#[cfg(feature = "nmi-hardware")]
#[inline]
pub fn allint_active() -> bool {
    // SAFETY: `ALLINT_ACTIVE` is a `def_percpu` static with independent
    // slots per CPU, accessed via `TPIDR_EL1` on AArch64; no other CPU can
    // alias this slot.
    unsafe { ALLINT_ACTIVE.read_current_raw() }
}

#[cfg(feature = "nmi-hardware")]
#[inline]
pub fn mark_allint_active() {
    // SAFETY: per‑CPU slot; written once during per‑CPU init with no
    // concurrent access.
    unsafe { ALLINT_ACTIVE.write_current_raw(true) };
}

/// Read `PSTATE.ALLINT` (ALLINT system register, `s3_0_c4_c3_0`).
///
/// Only meaningful when FEAT_NMI is enabled (`SCTLR_ELx.NMI=1`); the
/// `nmi-hardware` mode validates that at boot before any call site runs.
#[cfg(feature = "nmi-hardware")]
#[inline]
pub fn allint_is_set() -> bool {
    // Gate on the mechanism readiness flag: without an active hardware-NMI
    // mechanism (degraded build) ALLINT is not managed by the kernel, and
    // the register access would be UNDEFINED on CPUs without FEAT_NMI.
    if !allint_active() {
        return false;
    }
    let allint: u64;
    // LLVM accepts the symbolic `ALLINT` name only when the target enables
    // `+nmi`.  This kernel probes FEAT_NMI at runtime, so use the architectural
    // encoding without raising the binary's baseline CPU requirement.
    // SAFETY: `allint_active()` above is set only after boot-time FEAT_NMI
    // validation; ALLINT is therefore accessible from EL1, and reading it has
    // no memory side effects.
    unsafe { asm!("mrs {}, s3_0_c4_c3_0", out(reg) allint, options(nomem, nostack)) };
    allint & ALLINT_MASK != 0
}

/// Clear `PSTATE.ALLINT` (ALLINT system register, `s3_0_c4_c3_0`).
///
/// Only meaningful when FEAT_NMI is enabled (`SCTLR_ELx.NMI=1`); the
/// `nmi-hardware` mode validates that at boot before any call site runs.
#[cfg(feature = "nmi-hardware")]
#[inline]
pub fn allint_clear() {
    // Gate on the mechanism readiness flag: see `allint_is_set`.
    if !allint_active() {
        return;
    }
    // LLVM accepts the symbolic `ALLINT` name only when the target enables
    // `+nmi`.  This kernel probes FEAT_NMI at runtime, so use the architectural
    // encoding without raising the binary's baseline CPU requirement.
    // SAFETY: `allint_active()` above is set only after boot-time FEAT_NMI
    // validation; ALLINT is therefore accessible from EL1, and writing it has
    // no memory side effects.
    unsafe { asm!("msr s3_0_c4_c3_0, xzr", options(nomem, nostack)) };
}

/// Enable normal IRQs on the current CPU.
///
/// In PMR mode: sets PMR to [`pmr::ALL`] (unmasking all IRQs), then clears
/// `DAIF.I`.  In non‑PMR mode: clears `DAIF.I` directly.  Both paths write
/// `DAIF.I = 0` unconditionally; see the [module‑level DAIF.I
/// invariant](self#pmrmode-daifi-invariant).
#[inline]
pub fn enable_local_irq() {
    #[cfg(feature = "nmi-pseudo")]
    if pmr::is_ready() {
        pmr::write(pmr::ALL);
    }
    // `modify` clears only the DAIF.I bit, preserving the D/A/F masks.
    DAIF.modify(DAIF::I::Unmasked);
    // Hardware-NMI mode: exception entry set ALLINT=1 (SPINTMASK=0) and it
    // masks IRQs and NMIs alike.  Opening local IRQs is the point where the
    // kernel decides nesting is allowed, so open ALLINT too; otherwise a
    // task waiting with "IRQs enabled" (e.g. the idle loop) can never
    // receive the wake-up interrupt.
    #[cfg(feature = "nmi-hardware")]
    allint_clear();
}

/// Disable normal IRQs on the current CPU.
///
/// In PMR mode ([`pmr::is_ready`] returns `true`): sets PMR to
/// [`pmr::NMI_ONLY`] (blocking normal IRQs while keeping pseudo‑NMIs
/// deliverable) and ensures `DAIF.I` is cleared.  **This unconditionally
/// writes `DAIF.I = 0`** — callers must not rely on a previously‑set
/// `DAIF.I` value being preserved.  See the [module‑level DAIF.I
/// invariant](self#pmrmode-daifi-invariant).
///
/// In non‑PMR mode: sets `DAIF.I = 1` (Masked).  Under `nmi-hardware` this
/// blocks ordinary IRQs only — Superpriority NMIs bypass `DAIF.I` and are
/// gated solely by `PSTATE.ALLINT`, which this function does not touch.
#[inline]
pub fn disable_local_irq() {
    #[cfg(feature = "nmi-pseudo")]
    if pmr::is_ready() {
        pmr::write(pmr::NMI_ONLY);
        // `modify` clears only the DAIF.I bit, preserving the D/A/F masks.
        DAIF.modify(DAIF::I::Unmasked);
        return;
    }
    // `modify` sets only the DAIF.I bit, preserving the D/A/F masks.
    DAIF.modify(DAIF::I::Masked);
}

/// Prepare to return from EL1 to EL0 with normal IRQs available in userspace.
///
/// In PMR mode, [`disable_local_irq`] masks normal IRQs by lowering PMR to
/// [`pmr::NMI_ONLY`] while keeping `DAIF.I` clear so pseudo-NMIs can still
/// arrive. That PMR mask must not be carried into EL0, otherwise ordinary
/// timer IRQs remain blocked for user code. This helper is called from the
/// AArch64 user-entry assembly while still on a kernel stack: it masks `DAIF.I`
/// for the remaining EL1 register-restore window, then restores PMR to
/// [`pmr::ALL`]. The final EL0 interrupt state is taken from `SPSR_EL1` by
/// `eret`.
#[inline]
pub fn prepare_enter_user_irq() {
    #[cfg(feature = "nmi-pseudo")]
    if pmr::is_ready() {
        // `modify` sets only the DAIF.I bit, preserving the D/A/F masks.
        DAIF.modify(DAIF::I::Masked);
        pmr::write(pmr::ALL);
    }
}

#[inline]
pub fn local_irq_enabled() -> bool {
    #[cfg(feature = "nmi-pseudo")]
    if pmr::is_ready() {
        // GIC default IRQ priority is 0xa0; PMR values numerically > 0xa0
        // mean normal IRQs are allowed.  See `pmr::NMI_ONLY` (0x80).
        return !DAIF.is_set(DAIF::I) && pmr::read() > pmr::IRQ_THRESHOLD;
    }
    // Hardware-NMI mode: ALLINT also gates delivery of every IRQ, including
    // NMIs, so an exception context entered with ALLINT=1 is not enabled
    // even when DAIF.I is clear.
    #[cfg(feature = "nmi-hardware")]
    if allint_is_set() {
        return false;
    }
    !DAIF.is_set(DAIF::I)
}

/// Save the current IRQ mask state and disable normal IRQs.
///
/// Returns an opaque token to be passed to [`restore_irq`].
///
/// This helper manages the **normal IRQ mask only**; it never manages NMI
/// delivery.  In particular, under `nmi-hardware`, `DAIF.I` does not mask
/// Superpriority NMIs — only `PSTATE.ALLINT` does, and that is controlled by
/// exception entry and the GIC dispatch window, not by this API.
///
/// # PMR mode vs non‑PMR mode
///
/// |                      | Non‑PMR mode                       | PMR mode                                    |
/// |----------------------|------------------------------------|---------------------------------------------|
/// | **What is saved**    | `DAIF` (all mask bits)             | `PMR` (GIC priority mask register)          |
/// | **What is disabled** | Normal IRQs (`DAIF.I=1`)           | Normal IRQs only (`PMR=NMI_ONLY`)          |
/// | **NMI effect**       | Not managed: Superpriority NMIs    | Not managed: priority‑0 pseudo‑NMIs        |
/// |                      | bypass `DAIF.I`                    | can still preempt                           |
/// | **Token encoding**   | Raw `DAIF` value                   | `TOKEN_BIT \| (PMR as usize)`               |
///
/// This difference is by design: the system supports NMIs (e.g. PMU counter
/// overflow), and a critical section that calls `save_irq_and_disable` /
/// `restore_irq` remains preemptible by NMIs in both modes.  Code that must
/// exclude NMIs as well is outside this API's contract: under `nmi-hardware`
/// it must manage `PSTATE.ALLINT` itself; under `nmi-pseudo` NMIs are
/// deliverable by definition.
#[inline]
pub fn save_irq_and_disable() -> usize {
    #[cfg(feature = "nmi-pseudo")]
    if pmr::is_ready() {
        let prev = pmr::read();
        pmr::write(pmr::NMI_ONLY);
        // PMR is an 8-bit register; the token encoding stores it in
        // bits 0-7 with TOKEN_BIT (bit 31) as the discriminator.
        return TOKEN_BIT | (prev as usize);
    }
    let flags: usize;
    // SAFETY: standard CPU interrupt‑control.
    unsafe {
        asm!("mrs {0}, daif", "msr daifset, #2", out(reg) flags,
             options(nomem, nostack, preserves_flags));
    }
    flags
}

/// Restore the IRQ mask state saved by [`save_irq_and_disable`].
///
/// The `state` token is the opaque value returned by `save_irq_and_disable`.
///
/// # Token decoding
///
/// - `TOKEN_BIT` set → PMR mode: restores the PMR value from bits 0‑7.
///   `DAIF.I` is **not** modified (PMR‑mode invariant: it stays 0).
/// - `TOKEN_BIT` clear → non‑PMR mode: restores the full `DAIF` value
///   via `msr daif`.
///
/// See [`save_irq_and_disable`] for the full semantic comparison.
#[inline]
pub fn restore_irq(state: usize) {
    #[cfg(feature = "nmi-pseudo")]
    if (state & TOKEN_BIT) != 0 {
        pmr::write((state & TOKEN_MASK) as u8);
        return;
    }
    // SAFETY: `state` from `save_irq_and_disable`.
    unsafe {
        asm!("msr daif, {0}", in(reg) state, options(nomem, nostack));
    }
}

/// Mask every DAIF exception class on the current CPU: debug exceptions,
/// SError, IRQs (including PMR-mode pseudo-NMIs), and FIQs.
///
/// This deliberately breaks the PMR-mode rule that keeps `DAIF.I` clear
/// (see the [module-level DAIF.I invariant](self#pmrmode-daifi-invariant)):
/// setting all four mask bits is the only architectural way to also block
/// the pseudo-NMIs, SError, and debug exceptions that could otherwise
/// resume a parked CPU. It is therefore reserved for one-way terminal
/// paths that never restore interrupts — currently only
/// [`super::cpu::stop_cpu`]. PMR is not touched: once every DAIF class is
/// masked, the CPU takes no interrupt at any priority.
#[inline]
pub fn disable_local_exceptions() {
    DAIF.write(DAIF::D::Masked + DAIF::A::Masked + DAIF::I::Masked + DAIF::F::Masked);
}

/// GIC priority mask (PMR) access for pseudo‑NMI support.
///
/// PMR is the GIC CPU‑interface priority mask (`ICC_PMR_EL1` on GICv3).
/// Masking normal IRQs via PMR instead of `DAIF.I` keeps pseudo‑NMIs
/// deliverable into critical sections.  This submodule owns the register
/// access and its value vocabulary; the masking policy lives in the
/// surrounding `irq` module.
pub mod pmr {
    // ── Per‑CPU readiness ───────────────────────────────────────────

    #[percpu::def_percpu]
    static READY: bool = false;

    #[inline]
    pub fn is_ready() -> bool {
        // SAFETY: `READY` is a `def_percpu` static, so each CPU has its own
        // independent slot.  `read_current_raw` accesses only the current
        // CPU's slot (via `TPIDR_EL1` on AArch64); no other CPU can alias
        // this slot.
        unsafe { READY.read_current_raw() }
    }

    /// Mark the current CPU's PMR interface as initialised.
    ///
    /// Idempotent (writes `true` to a per‑CPU flag); safe to call multiple
    /// times during boot and per‑CPU bringup.  After the first call,
    /// [`is_ready`] returns `true` and the fast IRQ mask/unmask path
    /// (PMR‑based instead of DAIF) is active on this CPU.
    #[inline]
    pub fn mark_ready() {
        // SAFETY: `READY` is a `def_percpu` static with independent slots per
        // CPU.  `write_current_raw` accesses only the current CPU's slot; no
        // other CPU writes this slot, and the write is a simple boolean store
        // with no lifetime or aliasing concerns.
        unsafe { READY.write_current_raw(true) };
    }

    // ── Register access (GICv3 `ICC_PMR_EL1`) ──────────────────────

    #[cfg(feature = "nmi-pseudo")]
    #[inline]
    pub fn read() -> u8 {
        let v: u64;
        // SAFETY: `ICC_PMR_EL1` is accessible from EL1 at all times (the
        // kernel runs at EL1 with system‑register access enabled).  The
        // `nomem` option tells the compiler this asm has no memory
        // side‑effects; `nostack` indicates no stack pointer modification.
        // Reading a system register is a pure operation — it cannot produce
        // UB.
        unsafe { core::arch::asm!("mrs {0}, ICC_PMR_EL1", out(reg) v, options(nomem, nostack)) };
        v as u8
    }

    #[cfg(feature = "nmi-pseudo")]
    #[inline]
    pub fn write(v: u8) {
        // SAFETY: `ICC_PMR_EL1` is writable from EL1 at all times.  `nomem`
        // / `nostack` correctly describe this pure register write.
        unsafe {
            core::arch::asm!("msr ICC_PMR_EL1, {0}", in(reg) v as u64, options(nomem, nostack));
        }
    }

    // ── Value vocabulary ────────────────────────────────────────────

    pub const ALL: u8 = 0xff;
    pub const NMI_ONLY: u8 = 0x80;
    /// GIC default IRQ priority, and the floor for non‑NMI interrupt
    /// priorities.
    ///
    /// Interrupts programmed below this threshold (other than NMI sources,
    /// which use priority 0) would be delivered and misclassified as NMIs
    /// while PMR is lowered to [`NMI_ONLY`].  PMR values > this mean normal
    /// IRQs are unmasked.
    pub const IRQ_THRESHOLD: u8 = 0xa0;
}
