// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GIC Priority Mask Register access.
//!
//! `init(addr)` is called by the GIC driver.  Afterwards the GIC version
//! is transparent — `read` / `write` go through MMIO (GICv2, `addr != 0`,
//! with `addr` pre-computed to `GICC base + 0x04`)
//! or `ICC_PMR_EL1` system register (GICv3, `addr == 0`).

#[cfg(feature = "pmr")]
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(feature = "pmr")]
static ADDR: AtomicUsize = AtomicUsize::new(0);

/// Optimization: on GICv3, `ADDR` is statically 0 and never written (gicv3::init
/// does not call `pmr::init`).  Once we confirm `ADDR == 0` via an Acquire load,
/// we cache this fact so subsequent hot-path `read()` / `write()` calls skip the
/// Acquire barrier entirely.  This matters because these functions are called on
/// the spinlock lock/unlock and irq enable/disable fast paths.
///
/// On GICv2, `ADDR` holds the pre-computed PMR MMIO pointer (`GICC base + 0x04`)
/// so the hot path avoids a per-call `+ 0x4` offset addition and pointer cast.
///
/// # Why `Relaxed` is sufficient for this flag
///
/// The flag is monotonic: it transitions `false → true` exactly once
/// (when the first `read()` / `write()` on a CPU observes `ADDR == 0`) and
/// is never reset.  On ARM64, a `Relaxed` store eventually becomes visible
/// to all CPUs (the architecture guarantees write propagation without
/// requiring barriers).  The worst case is benign:
///
/// - CPU A sets `ADDR_IS_ZERO = true` (Relaxed store).
/// - CPU B does not see the store yet and takes the slow path.
/// - CPU B's Acquire load of `ADDR` returns 0 (GICv3), so it re-executes
///   `ADDR_IS_ZERO.store(true, Relaxed)` — a redundant but harmless write
///   of the same value.
/// - CPU B's subsequent calls hit the fast path.
///
/// Each CPU therefore incurs **at most one** extra slow-path Acquire load
/// before converging.  Stronger ordering (Release / Acquire on the flag)
/// would save that single Acquire load per CPU at the cost of a DMB on
/// every fast-path access — a net loss given the flag is read on every
/// spinlock and irq enable/disable operation.
#[cfg(feature = "pmr")]
static ADDR_IS_ZERO: AtomicBool = AtomicBool::new(false);

// ── Per‑CPU readiness ───────────────────────────────────────────

#[percpu::def_percpu]
static READY: bool = false;

#[inline]
pub fn is_ready() -> bool {
    // SAFETY: `READY` is a `def_percpu` static, so each CPU has its own
    // independent slot.  `read_current_raw` accesses only the current CPU's
    // slot (via `TPIDR_EL1` on AArch64); no other CPU can alias this slot.
    unsafe { READY.read_current_raw() }
}

/// Mark the current CPU's PMR interface as initialised.
///
/// Idempotent (writes `true` to a per-CPU flag); safe to call multiple times
/// during boot and per-CPU bringup.  After the first call, [`is_ready`] returns
/// `true` and the fast IRQ mask/unmask path (PMR-based instead of DAIF) is active
/// on this CPU.
///
/// Called from [`init`] (GICv2) and from `gic::init_current_cpu` (both GICv2
/// and GICv3) during per-CPU bringup.
#[inline]
pub fn mark_ready() {
    // SAFETY: `READY` is a `def_percpu` static with independent slots per CPU.
    // `write_current_raw` accesses only the current CPU's slot; no other CPU
    // writes this slot, and the write is a simple boolean store with no
    // lifetime or aliasing concerns.
    unsafe { READY.write_current_raw(true) };
}

// ── Init ────────────────────────────────────────────────────────

/// Initialise the PMR access method for this kernel instance.
///
/// # Contract
///
/// **Call at most once.** The GIC version (v2 → MMIO, v3 → `ICC_PMR_EL1`) is
/// a platform property determined at boot and never changes.  This function must
/// only be called by the GICv2 driver's `gicv2::init`; the GICv3 driver never
/// calls it — it goes through [`mark_ready`] alone during per-CPU bringup.
///
/// # Memory ordering
///
/// `ADDR.store(Release)` pairs with the `Acquire` load on the first
/// `read()` / `write()` invocation on each CPU (see [`read`]).  The subsequent
/// `mark_ready()` call writes a per-CPU flag; together with program order on
/// the same CPU (init → enable interrupts → first IRQ) this guarantees the
/// stored ADDR value is visible before the first `read()`/`write()` on that CPU.
///
/// # Parameters
///
/// - `mmio`: GICC base address (GICv2), or 0 (GICv3 no‑op path).
///   For GICv2 the PMR address (`mmio + 0x04`) is pre‑computed and stored so
///   hot‑path `read()`/`write()` calls avoid the per‑call offset addition.
#[cfg(feature = "pmr")]
#[inline]
pub fn init(mmio: usize) {
    if mmio != 0 {
        ADDR.store(mmio + 0x4, Ordering::Release);
    }
    mark_ready();
}

#[cfg(not(feature = "pmr"))]
#[inline]
pub fn init(_mmio: usize) {
    mark_ready();
}

/// PMR MMIO access reads a 32-bit register then truncates to u8 (bits [7:0]).
/// This is correct only on little‑endian systems where the least‑significant
/// byte of the register sits at the lowest address.  AArch64 big‑endian
/// (SCTLR_EL1.EE) is not supported by this kernel.
#[cfg(feature = "pmr")]
const _: () = assert!(
    cfg!(target_endian = "little"),
    "PMR MMIO access assumes little-endian"
);

// ── Read / Write ────────────────────────────────────────────────

#[cfg(feature = "pmr")]
#[inline]
pub fn read() -> u8 {
    // Fast path for GICv3: ADDR is statically 0 and never changes, so skip the
    // Acquire barrier and load ADDR entirely.
    if ADDR_IS_ZERO.load(Ordering::Relaxed) {
        let v: u64;
        // SAFETY: `ICC_PMR_EL1` is accessible from EL1 at all times (the kernel
        // runs at EL1 with system‑register access enabled).  The `nomem` option
        // tells the compiler this asm has no memory side‑effects; `nostack`
        // indicates no stack pointer modification.  Reading a system register
        // is a pure operation — it cannot produce UB.
        unsafe { core::arch::asm!("mrs {0}, ICC_PMR_EL1", out(reg) v, options(nomem, nostack)) };
        return v as u8;
    }

    // Slow path (first call on this CPU, or GICv2 every call):
    // Acquire pairs with the Release store in pmr::init() for GICv2.
    // On GICv3, ADDR is statically 0 and init() was never called, so there
    // is no paired Release — the Acquire is logically unpaired but harmless:
    // it reads the static initial value 0 which never changes.
    let a = ADDR.load(Ordering::Acquire);
    if a != 0 {
        // SAFETY: `a` is the pre-computed PMR MMIO pointer (GICC base + 0x04)
        // set by the GICv2 driver.
        unsafe { (a as *const u32).read_volatile() as u8 }
    } else {
        // ADDR == 0: GICv3 confirmed.  Cache so future calls take the fast path.
        ADDR_IS_ZERO.store(true, Ordering::Relaxed);
        let v: u64;
        // SAFETY: `ICC_PMR_EL1` is readable from EL1 at all times.  `nomem`
        // / `nostack` correctly describe this pure register read.
        unsafe { core::arch::asm!("mrs {0}, ICC_PMR_EL1", out(reg) v, options(nomem, nostack)) };
        v as u8
    }
}

#[cfg(feature = "pmr")]
#[inline]
pub fn write(v: u8) {
    // Fast path for GICv3: ADDR is statically 0 and never changes, so skip the
    // Acquire barrier and load ADDR entirely.
    if ADDR_IS_ZERO.load(Ordering::Relaxed) {
        // SAFETY: `ICC_PMR_EL1` is writable from EL1 at all times.  `nomem` /
        // `nostack` correctly describe this pure register write.
        unsafe {
            core::arch::asm!("msr ICC_PMR_EL1, {0}", in(reg) v as u64, options(nomem, nostack));
        }
        return;
    }

    // Slow path (first call on this CPU, or GICv2 every call):
    // Acquire pairs with the Release store in pmr::init() for GICv2.
    // On GICv3, ADDR is statically 0 and init() was never called, so there
    // is no paired Release — the Acquire is logically unpaired but harmless:
    // it reads the static initial value 0 which never changes.
    let a = ADDR.load(Ordering::Acquire);
    if a != 0 {
        // SAFETY: `a` is the pre-computed PMR MMIO pointer (GICC base + 0x04)
        // set by the GICv2 driver.
        unsafe { (a as *mut u32).write_volatile(v as u32) };
    } else {
        // ADDR == 0: GICv3 confirmed.  Cache so future calls take the fast path.
        ADDR_IS_ZERO.store(true, Ordering::Relaxed);
        // SAFETY: `ICC_PMR_EL1` is writable from EL1 at all times.
        unsafe {
            core::arch::asm!("msr ICC_PMR_EL1, {0}", in(reg) v as u64, options(nomem, nostack));
        }
    }
}

// ── Constants ───────────────────────────────────────────────────

pub const ALL: u8 = 0xff;
pub const NMI_ONLY: u8 = 0x80;
/// GIC default IRQ priority.  PMR values > this mean normal IRQs are unmasked.
#[cfg(feature = "pmr")]
pub(crate) const IRQ_THRESHOLD: u8 = 0xa0;
#[cfg(feature = "pmr")]
pub(crate) const TOKEN_BIT: usize = 1 << 31;
#[cfg(feature = "pmr")]
pub(crate) const TOKEN_MASK: usize = 0xFF;
