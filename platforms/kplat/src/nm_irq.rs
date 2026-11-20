// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform NMI (or pseudo-NMI) interface.
//!
//! # Architecture
//!
//! This module defines the **NMI mode** — whether the platform supports
//! hardware NMI, falls back to pseudo-NMI, or has no NMI at all.  It is
//! deliberately **not** tied to any specific NMI source.  Source‑specific
//! configuration (counter thresholds, event numbers, handler registration)
//! belongs to the source's own subsystem, which implements [`PeriodicNmiSourceIf`]
//! behind a source‑neutral interface.
//!
//! ```text
//!   Consumer (watchdog)
//!     → khal::nmi::enable_periodic_nmi(period_ns, cb)
//!       → PeriodicNmiSourceIf provider (e.g. PMU cycle counter backend)
//!         → source-owned local delivery preparation (per CPU)
//!           → kirq::prepare_percpu_nmi(source descriptor)
//!             → local GIC NMI attribute / pseudo-NMI priority transaction
//!   pmu feature init (kruntime)
//!     → khal::pmu::register_overflow_dispatch(hwirq)
//!       → pmu-only build: kirq::try_register_startup(...)
//!       → PMU as NMI source (nmi-pmu): kirq::register_nmi_startup(...)
//!         → Line-owned RCU action (shared by true NMI and IRQ fallback)
//! ```

use kplat_macros::device_interface;

/// NMI callback type, invoked in NMI context on the current CPU.
pub type NmiCb = fn();

/// Runtime NMI capability reported by the platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NmiMode {
    /// True hardware NMI (GICv3.3 — cannot be masked by IRQ disable).
    Hardware,
    /// Pseudo-NMI implemented via high-priority IRQ + PMR masking.
    Pseudo,
    /// No NMI support.
    None,
}

/// Static descriptor for the NMI implementation.
#[derive(Clone, Copy, Debug)]
pub struct NmiSourceInfo {
    /// Human-readable name for diagnostics.
    pub name: &'static str,
}

#[device_interface]
pub trait NmiMechanismIf {
    /// System‑wide (once‑per‑boot) NMI initialization.
    ///
    /// Called during `early_driver_init`, after the interrupt controller
    /// distributor has been mapped and initialised. Typically queries
    /// controller‑wide NMI capability (for example, `GICD_TYPER.NMI` on GICv3.3);
    /// source-specific delivery attributes belong to the source backend,
    /// while per-CPU mechanism enablement belongs to `late_init`.
    fn early_init() -> bool;

    /// Per‑CPU NMI initialization.
    ///
    /// Called during `prepare_interrupts` / `prepare_local_interrupts`, after the controller
    /// CPU interface is ready.  Typically configures per‑redistributor
    /// registers and per‑CPU architecture‑level NMI enables (e.g.
    /// `SCTLR_EL1.NMI`, `PSTATE.ALLINT`, `GICR_INMIR`).
    ///
    /// Source‑specific timer / counter setup (e.g. PMU cycle counter
    /// threshold) is **not** done here — it belongs to the source's own
    /// subsystem.
    fn late_init() -> bool;

    /// Current operating NMI mode.
    ///
    /// Source backends use this to select their local delivery preparation.
    fn mode() -> NmiMode;

    /// Returns the static descriptor for this NMI implementation.
    fn info() -> NmiSourceInfo;
}

/// Source‑neutral periodic NMI provider.
///
/// Consumers such as the hardlockup watchdog ask for a periodic NMI through
/// this interface without knowing which hardware source (PMU cycle counter,
/// future sources) generates the events.  The platform implements this trait
/// in the source's own subsystem.
///
/// kiface allows exactly one provider per interface.  With a single source
/// the platform implements this trait directly in the source backend; when a
/// second source is added, the platform NMI module should become the sole
/// provider and dispatch to per‑source backends.
#[device_interface]
pub trait PeriodicNmiSourceIf {
    /// Arm a periodic NMI with period `period_ns`.
    ///
    /// Must be called on **each CPU** so every CPU arms its own source.
    /// The provider keeps setup, rollback and producer start on the calling
    /// CPU. This local exclusion does not serialize a remote source shutdown;
    /// the source owner must exclude concurrent shutdown and restart.
    /// This method never registers an IRQ-level handler: the line's
    /// overflow-dispatch handler is registered exactly once by the PMU
    /// feature init (`khal::pmu::register_overflow_dispatch`, backed by
    /// `PmuInterruptIf::register_overflow_irq`) — as a normal IRQ handler in
    /// pmu-only builds, or as an NMI handler with normal-IRQ fallback when
    /// the PMU is the compiled NMI source.  This method only arms the local
    /// CPU's source: it registers the consumer callback into the per-CPU
    /// source state, prepares and enables local line delivery (e.g. PPI priority
    /// / NMI attribute), and starts the counter. `handler` is invoked in NMI
    /// context on each overflow.  Returns `false` if no periodic NMI source
    /// is available or the local source is already armed.
    fn enable_periodic_nmi(period_ns: u64, handler: NmiCb) -> bool;

    /// Quiesce the calling CPU's periodic NMI source for a terminal stop.
    ///
    /// This is intentionally a one-way lifecycle operation: it stops a
    /// source armed by [`PeriodicNmiSourceIf::enable_periodic_nmi`] without
    /// unregistering its callback or undoing its IRQ/NMI-line configuration.
    /// It is a no-op when the local source was never armed. Callers must be
    /// pinned to the CPU whose source they are stopping and must not attempt
    /// to re-arm that source afterwards.
    fn quiesce_periodic_nmi();

    /// Stops and releases the calling CPU's periodic source for later reuse.
    ///
    /// The owner must serialize this operation against source start and other
    /// users of the same interrupt line. The provider pins the whole local
    /// transaction; task or ordinary IPI context is allowed, NMI is not.
    /// True confirms producer stop and local controller teardown before local
    /// callback storage is cleared. False retains storage for retry and does
    /// not authorize handler removal or source reuse. Unsupported mechanisms
    /// and shared source ownership are rejected before stopping production.
    /// A prepared local line still needs teardown when no periodic counter was
    /// armed; absence of a producer alone is not shutdown completion.
    ///
    /// This neither broadcasts nor removes the Line's shared handler. The owner
    /// must collect successful execution on every participant before removing
    /// that handler. A later restart must prepare and enable local delivery again.
    fn shutdown_periodic_nmi() -> bool;
}
