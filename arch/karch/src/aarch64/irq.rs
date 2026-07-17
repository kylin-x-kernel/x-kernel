// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt control for AArch64.
//!
//! When the `pmr` feature is enabled and the GIC driver has marked
//! this CPU ready, IRQ masking uses the GIC priority mask instead of
//! `DAIF.I` — keeping pseudo‑NMIs deliverable.
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

use core::arch::asm;

use aarch64_cpu::registers::{DAIF, Readable, Writeable};

#[cfg(feature = "pmr")]
use super::pmr;

/// Enable normal IRQs on the current CPU.
///
/// In PMR mode: sets PMR to [`pmr::ALL`] (unmasking all IRQs), then clears
/// `DAIF.I`.  In non‑PMR mode: clears `DAIF.I` directly.  Both paths write
/// `DAIF.I = 0` unconditionally; see the [module‑level DAIF.I
/// invariant](self#pmrmode-daifi-invariant).
#[inline]
pub fn enable_local_irq() {
    #[cfg(feature = "pmr")]
    if pmr::is_ready() {
        pmr::write(pmr::ALL);
    }
    DAIF.write(DAIF::I::Unmasked);
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
/// In non‑PMR mode: sets `DAIF.I = 1` (Masked).
#[inline]
pub fn disable_local_irq() {
    #[cfg(feature = "pmr")]
    if pmr::is_ready() {
        pmr::write(pmr::NMI_ONLY);
        DAIF.write(DAIF::I::Unmasked);
        return;
    }
    DAIF.write(DAIF::I::Masked);
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
    #[cfg(feature = "pmr")]
    if pmr::is_ready() {
        DAIF.write(DAIF::I::Masked);
        pmr::write(pmr::ALL);
    }
}

#[inline]
pub fn local_irq_enabled() -> bool {
    #[cfg(feature = "pmr")]
    if pmr::is_ready() {
        // GIC default IRQ priority is 0xa0; PMR values numerically > 0xa0
        // mean normal IRQs are allowed.  See `pmr::NMI_ONLY` (0x80).
        return !DAIF.is_set(DAIF::I) && pmr::read() > pmr::IRQ_THRESHOLD;
    }
    !DAIF.is_set(DAIF::I)
}

/// Save the current IRQ mask state and disable normal IRQs.
///
/// Returns an opaque token to be passed to [`restore_irq`].
///
/// # PMR mode vs non‑PMR mode
///
/// |                      | Non‑PMR mode                       | PMR mode                                    |
/// |----------------------|------------------------------------|---------------------------------------------|
/// | **What is saved**    | `DAIF` (all mask bits)             | `PMR` (GIC priority mask register)          |
/// | **What is disabled** | All IRQs **and** NMIs (`DAIF.I=1`) | Normal IRQs only (`PMR=NMI_ONLY`); NMIs     |
/// |                      |                                    | can still preempt                            |
/// | **Token encoding**   | Raw `DAIF` value                   | `TOKEN_BIT \| (PMR as usize)`               |
///
/// This difference is by design: in PMR mode the system supports
/// pseudo‑NMIs (e.g. PMU counter overflow).  A critical section that
/// calls `save_irq_and_disable` / `restore_irq` in PMR mode is still
/// preemptible by NMIs.  Code that must exclude NMIs as well should use
/// [`disable_local_irq`] which sets `DAIF.I` in non‑PMR mode.
#[inline]
pub fn save_irq_and_disable() -> usize {
    #[cfg(feature = "pmr")]
    if pmr::is_ready() {
        let prev = pmr::read();
        pmr::write(pmr::NMI_ONLY);
        // PMR is an 8-bit register; the token encoding stores it in
        // bits 0-7 with TOKEN_BIT (bit 31) as the discriminator.
        return pmr::TOKEN_BIT | (prev as usize);
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
    #[cfg(feature = "pmr")]
    if (state & pmr::TOKEN_BIT) != 0 {
        pmr::write((state & pmr::TOKEN_MASK) as u8);
        return;
    }
    // SAFETY: `state` from `save_irq_and_disable`.
    unsafe {
        asm!("msr daif, {0}", in(reg) state, options(nomem, nostack));
    }
}

#[deprecated(note = "Use `enable_local_irq`")]
#[inline]
pub fn enable_irq() {
    enable_local_irq()
}
#[deprecated(note = "Use `disable_local_irq`")]
#[inline]
pub fn disable_irq() {
    disable_local_irq()
}
#[deprecated(note = "Use `local_irq_enabled`")]
#[inline]
pub fn irq_enabled() -> bool {
    local_irq_enabled()
}
