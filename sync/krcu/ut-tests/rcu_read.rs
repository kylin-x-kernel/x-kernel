// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal host borrowing and cleanup-demand policy.

use unittest::{assert, assert_eq, def_test};

use crate::host::{self, RcuExecutionIf};

#[def_test]
fn host_borrow_preserves_current_reader_without_entering_a_scope() {
    let (calls, observed, result, is_reading_after_borrow) =
        host::with_current_reader(|expected| {
            let mut calls = 0;
            let (observed, result) = host::with_current_reader(|current| {
                calls += 1;
                let is_current = core::ptr::eq(current.reader(), expected.reader());
                let before = current.reader().is_reading();
                let during = crate::with_read(|_| current.reader().is_reading());
                let after = current.reader().is_reading();
                ((is_current, before, during, after), Err::<(), _>(17))
            });
            (calls, observed, result, expected.reader().is_reading())
        });
    assert_eq!(calls, 1);
    assert_eq!(observed, (true, false, true, false));
    assert_eq!(result, Err(17));
    assert!(!is_reading_after_borrow);
}

#[def_test]
fn cleanup_without_notification_still_unlinks_finished_membership() {
    let mut observed = None;
    RcuExecutionIf::with_exception_snapshot(&mut |_| {
        host::with_preempt_check_deferred(|| {
            host::with_current_reader(|current| {
                let reader = current.reader();
                let cpu = RcuExecutionIf::current_cpu();
                // SAFETY: retained current at a watched task boundary under
                // IRQ/preemption exclusion; no node lock or active scope.
                unsafe { crate::context::finish_reader_cleanup(reader) };
                let initially_clean = !reader.is_enrolled()
                    && !crate::context::needs_reader_cleanup(cpu);
                let enroll_fn = |_: &krcu_core::ReadGuard<'_>| {
                    // SAFETY: exclusively driven current under host exclusion;
                    // its permanent node and retained storage outlive unlinking.
                    unsafe { crate::context::reader_node(cpu).enroll(reader) };
                };
                // SAFETY: retained current, excluded nonblocking callback;
                // explicit cleanup below removes the deferred membership.
                unsafe { reader.with_read_deferred(enroll_fn) };
                let pending_without_hint = !reader.is_reading()
                    && reader.is_enrolled()
                    && !crate::context::needs_reader_cleanup(cpu);
                // SAFETY: the scope ended, with the same retained current and
                // IRQ/preemption exclusion; membership alone requires cleanup.
                unsafe { crate::context::finish_reader_cleanup(reader) };
                let cleaned = !reader.is_enrolled()
                    && !crate::context::needs_reader_cleanup(cpu);
                // Restore the fixture even if the provider incorrectly skipped
                // unlinking. This also preserves any resulting progress hint.
                // SAFETY: same excluded retained current after scope exit.
                let outcome = unsafe { reader.finish_deferred_cleanup() };
                if outcome.needs_progress {
                    krcu_core::RcuContextIf::request_completion_pass();
                }
                observed = Some((initially_clean, pending_without_hint, cleaned));
            });
        });
    });
    assert_eq!(observed, Some((true, true, true)));
}

#[def_test]
fn cleanup_demand_survives_reentry_and_is_consumed_locally() {
    let mut observed = None;
    RcuExecutionIf::with_exception_snapshot(&mut |_| {
        host::with_preempt_check_deferred(|| {
            host::with_current_reader(|current| {
                let reader = current.reader();
                let cpu = RcuExecutionIf::current_cpu();
                let enroll_fn = |_: &krcu_core::ReadGuard<'_>| {
                    // SAFETY: excluded active current reader; no node lock is
                    // held and its permanent owner outlives explicit cleanup.
                    unsafe { crate::context::reader_node(cpu).enroll(reader) };
                };
                // SAFETY: host retains current with IRQs/preemption excluded.
                // No scheduler request is needed to construct this deferred state.
                unsafe { reader.with_read_deferred(enroll_fn) };
                crate::context::request_local_cleanup_for_tests();
                let requested = !reader.is_reading()
                    && reader.is_enrolled()
                    && crate::context::needs_reader_cleanup(cpu);
                let reenter_fn = |_: &krcu_core::ReadGuard<'_>| {
                    // SAFETY: retained current at an excluded watched task
                    // boundary; active nesting must prevent unlinking.
                    unsafe { crate::context::finish_reader_cleanup(reader) };
                    crate::context::needs_reader_cleanup(cpu) && reader.is_enrolled()
                };
                // SAFETY: same retained current under host exclusion; this
                // nested scope ends before explicit local cleanup below.
                let retained = unsafe { reader.with_read_deferred(reenter_fn) };
                // SAFETY: scopes ended; current remains watched and excluded.
                unsafe { crate::context::finish_reader_cleanup(reader) };
                let consumed = !crate::context::needs_reader_cleanup(cpu) && !reader.is_enrolled();
                // A later notification must survive even without an owner.
                crate::context::request_local_cleanup_for_tests();
                let republished = crate::context::needs_reader_cleanup(cpu);
                // SAFETY: same safe boundary; stale demand needs no enrollment.
                unsafe { crate::context::finish_reader_cleanup(reader) };
                let stale_consumed = !crate::context::needs_reader_cleanup(cpu);
                observed = Some((requested, retained, consumed, republished, stale_consumed));
            });
        });
    });
    assert_eq!(observed, Some((true, true, true, true, true)));
}
