// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

// These fixtures simulate one CPU's strictly nested execution. They do not
// change the running kernel CPU's RCU state or claim real IRQ/NMI coverage.
pub(super) fn quiescent<R>(cpu: &CpuWatching, body_fn: impl FnOnce() -> R) -> R {
    // SAFETY: isolated fixture, no other CPU writer or outstanding interrupt;
    // the closure balances every simulated interrupt before returning.
    unsafe { cpu.enter_quiescent() };
    let result = body_fn();
    // SAFETY: matching transition on the same isolated fixture, all entries exited.
    unsafe { cpu.exit_quiescent() };
    result
}

fn interrupt<R>(cpu: &CpuWatching, body_fn: impl FnOnce() -> R) -> R {
    // SAFETY: isolated fixture with one writer and strictly nested entries.
    let entry = unsafe { cpu.enter_interrupt() };
    let result = body_fn();
    // SAFETY: nested entries finished; no simulated scheduling or migration.
    unsafe { entry.exit() };
    result
}

#[unittest::def_test]
fn idle_and_user_windows_return_to_watched_execution() {
    let cpu = CpuWatching::new();
    assert!(cpu.is_watching());
    for _ in 0..2 {
        let before = cpu.sample();
        let inside = quiescent(&cpu, || (cpu.is_watching(), cpu.sample()));
        assert_eq!(inside, (false, before + 1));
        assert!(cpu.is_watching());
        assert_eq!(cpu.sample(), before + 2);
    }
}

#[unittest::def_test]
fn nested_interrupts_preserve_watched_kernel_state() {
    let cpu = CpuWatching::new();
    let before = cpu.sample();
    let inside = interrupt(&cpu, || {
        let nested = interrupt(&cpu, || cpu.is_watching());
        (nested, cpu.is_watching())
    });
    assert_eq!(inside, (true, true));
    assert!(cpu.is_watching());
    assert_eq!(cpu.sample(), before);
}

#[unittest::def_test]
fn only_outer_activation_restores_quiescence() {
    let cpu = CpuWatching::new();
    let observed = quiescent(&cpu, || {
        let before = cpu.sample();
        let inside = interrupt(&cpu, || {
            let active = cpu.sample();
            let nested = interrupt(&cpu, || {
                let innermost = interrupt(&cpu, || cpu.is_watching());
                (innermost, cpu.is_watching())
            });
            (nested, cpu.is_watching(), cpu.sample() == active)
        });
        (inside, cpu.is_watching(), cpu.sample() == before + 2)
    });
    assert_eq!(observed, (((true, true), true, true), false, true));
    assert!(cpu.is_watching());
}

#[unittest::def_test]
fn completed_interrupt_history_survives_later_crossings() {
    let cpu = CpuWatching::new();
    let observed = quiescent(&cpu, || {
        let sampled = cpu.sample();
        // Check completed interrupt history across subsequent transitions.
        // This does not inject an interrupt inside an atomic operation or
        // distinguish RMW from a load/store pair without such an interleaving.
        let in_interrupt = interrupt(&cpu, || cpu.is_watching());
        let after_interrupt = cpu.is_watching();
        cpu.cross_boundary();
        let active = (cpu.is_watching(), cpu.sample() == sampled + 3);
        cpu.cross_boundary();
        (
            in_interrupt,
            after_interrupt,
            active,
            cpu.is_watching(),
            cpu.sample() == sampled + 4,
        )
    });
    assert_eq!(observed, (true, false, (true, true), false, true));
}

#[unittest::def_test]
fn sequence_wrap_preserves_state_without_certifying_progress() {
    let cpu = CpuWatching {
        sequence: AtomicUsize::new(usize::MAX),
        ..CpuWatching::new()
    };
    let observed = quiescent(&cpu, || {
        let before = cpu.sample();
        let during = interrupt(&cpu, || cpu.sample());
        (before, during, cpu.sample())
    });
    assert_eq!(observed, (0, 1, 2));
    assert_eq!(cpu.sample(), 3);
    assert!(cpu.is_watching());
}
