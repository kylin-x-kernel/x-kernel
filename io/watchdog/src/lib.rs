// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Watchdog subsystem for soft/hard lockup detection.
#![no_std]
extern crate alloc;

/// Optional workqueue warning provider, called only from sleepable task context.
/// Enabling `workqueue-stall-check` requires exactly one linked implementation.
/// The per-CPU watchdog task invokes it between timestamp updates and sleep.
/// It is separate from fatal
/// IRQ/NMI checks and must not drain or recover queues. Busy or uninitialized
/// services may skip inspection without reporting.
#[cfg(feature = "workqueue-stall-check")]
#[kiface::interface]
pub trait WorkqueueStallInspectorIf {
    /// Inspects this CPU's queues using a monotonic sample and warning threshold.
    /// Requires sleepable task context with no caller-held spinlocks.
    fn inspect(
        cpu: kcpu_id_map::LogicalCpuId,
        now: ktime_types::MonotonicInstant,
        threshold: ktime_types::TimeSpan,
    );
}

pub mod init;
pub mod lockup_detection;
pub mod rendezvous;
pub mod watchdog_task;
pub use crate::{
    init::{init_primary, init_secondary},
    lockup_detection::{
        check_softlockup, register_hardlockup_detection_task, timer_tick, touch_softlockup,
    },
    watchdog_task::register_watchdog_task,
};
