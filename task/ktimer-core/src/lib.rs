// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared timer core, independent of task scheduling.
//!
//! TimerSet exposes platform delivery and locked hardware arbitration. Timer
//! owns stable node/handler storage and drains delivery before releasing it.
//! Starting a timer still requires the platform placement and IRQ guarantees.
//!
//! External API: [`Timer`] owns a timer, [`TimerHandler`] receives [`Expiration`],
//! and [`TimerControl`] changes its successor from the callback. [`TimerSet`]
//! provides platform construction, IRQ dispatch and hardware arbitration only.
//! Nodes, indexes, deadline validation, CPU state and raw registration stay private.
//!
//! # Configuration and context
//! Enable `smp` for cross-CPU concurrency and `preempt` when the kernel supports
//! preemption. The number of [`TimerSet`] slots does not enable these features.
//! Preparation requires an available allocator in an allocation-capable context;
//! later operations require initialized IRQ/preemption guards, not a process thread.
//! The allocation-free guarantee excludes preparation, final storage destruction
//! and resource operations performed by the caller's callback.
//!
//! # Examples
//! The [owned lifecycle example] prepares a static set and an atomic handler,
//! starts a one-shot, moves its owner, drives modeled CPU service, and checks
//! callback counts, repeated cancellation and exactly-once destruction.
//! The [future-only start example] demonstrates rejecting an elapsed deadline
//! without replacing an existing future event. The [callback rearm example]
//! shows explicit one-shot continuation followed by stopping, and the [programming
//! example] shows locked deadline observation across replacements and cancellation.
//!
//! These are executable kernel unit tests, not standalone userspace snippets.
//! Configure `platforms/kplat-aarch64/qemu_defconfig` with `make defconfig`, then
//! run `make unittest UNITTEST_CRATE=ktimer_core,ktask::tests_timer_routing`.
//! The tests require initialized allocator/guard support and explicitly model CPU
//! service; they do not demonstrate production hardware or IPI integration.
//!
//! [owned lifecycle example]: ../src/ktimer_core/timer.rs.html#241-261
//! [future-only start example]: ../src/ktimer_core/timer.rs.html#263-279
//! [callback rearm example]: ../src/ktimer_core/dispatch.rs.html#68-91
//! [programming example]: ../src/ktimer_core/set.rs.html#357-404

#![no_std]

extern crate alloc;

mod control;

#[cfg_attr(
    all(not(unittest), not(doc)),
    expect(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: timer API integration follows"
    )
)]
mod cpu;
mod deadline;
mod dispatch;
mod index;
mod set;
mod timer;

pub use control::TimerControl;
pub use deadline::Expiration;
pub use index::TimerHandler;
pub use set::TimerSet;
pub use timer::{CancelResult, Timer};

/// Test-only scenarios driven by an upper-layer kernel task harness.
#[cfg(unittest)]
#[path = "../ut-tests/test_support.rs"]
pub mod test_support;
