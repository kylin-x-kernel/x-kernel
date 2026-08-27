// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! vCPU runtime state and profiling counters.

use core::sync::atomic::AtomicU64;

/// Coarse vCPU execution state.
///
/// Published by the vCPU run loop and consulted by
/// [`crate::vm::VmShared::inject_irq`] to decide whether an injected virtual IRQ
/// needs to actively wake or kick the target vCPU. Also useful for diagnostics.
///
/// This is deliberately coarse: the real interrupt-injection substrate (pending
/// bitmap + list-register programming) and the cross-pCPU kick path are built on
/// top of this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VcpuRunState {
    /// vCPU not yet started, or has exited.
    Offline          = 0,
    /// Trapped out of the guest; the host is handling the exit.
    HostHandlingExit = 1,
    /// Executing guest code at EL1/VS/non-root mode.
    RunningGuest     = 2,
    /// Parked in the VMM WFI path in an interruptible sleep.
    WfiSleeping      = 3,
}

impl VcpuRunState {
    pub(crate) fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::HostHandlingExit,
            2 => Self::RunningGuest,
            3 => Self::WfiSleeping,
            _ => Self::Offline,
        }
    }

    /// Short human-readable label for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::HostHandlingExit => "host",
            Self::RunningGuest => "guest",
            Self::WfiSleeping => "wfi",
        }
    }
}

/// Per-vCPU profiling counters stored in `VmShared`.
///
/// Each vCPU thread writes its own slot (no contention); the procfs reader loads
/// with `Relaxed` ordering because slightly stale diagnostics are acceptable.
pub struct VcpuStats {
    pub guest_ticks: AtomicU64,
    pub exit_ticks: AtomicU64,
    pub exit_count: AtomicU64,
    pub exits_halt: AtomicU64,
    pub exits_hypercall: AtomicU64,
    pub exits_mmio: AtomicU64,
    pub exits_interrupt: AtomicU64,
    pub exits_other: AtomicU64,
}

impl VcpuStats {
    pub(crate) const fn new() -> Self {
        Self {
            guest_ticks: AtomicU64::new(0),
            exit_ticks: AtomicU64::new(0),
            exit_count: AtomicU64::new(0),
            exits_halt: AtomicU64::new(0),
            exits_hypercall: AtomicU64::new(0),
            exits_mmio: AtomicU64::new(0),
            exits_interrupt: AtomicU64::new(0),
            exits_other: AtomicU64::new(0),
        }
    }
}

/// Exit reason categories for profiling.
pub const EXIT_CAT_HALT: u8 = 0;
pub const EXIT_CAT_HYPERCALL: u8 = 1;
pub const EXIT_CAT_MMIO: u8 = 2;
pub const EXIT_CAT_INTERRUPT: u8 = 3;
pub const EXIT_CAT_OTHER: u8 = 4;
