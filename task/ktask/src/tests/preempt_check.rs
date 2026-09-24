// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-owned injection of renewed requests at preemption-check guard release.

use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::TaskInner;

pub(super) struct Probe {
    remaining: AtomicUsize,
    first_stack_address: AtomicUsize,
    max_stack_growth: AtomicUsize,
}

impl Probe {
    pub(super) const fn new() -> Self {
        Self {
            remaining: AtomicUsize::new(0),
            first_stack_address: AtomicUsize::new(0),
            max_stack_growth: AtomicUsize::new(0),
        }
    }

    pub(super) fn rearm(&self, task: &TaskInner) {
        // Only this task exercises its probe; no cross-task publication occurs.
        let remaining = self.remaining.load(Ordering::Relaxed);
        if remaining == 0 {
            return;
        }
        let marker = 0u8;
        let address = core::hint::black_box(&marker as *const u8 as usize);
        let first = self.first_stack_address.load(Ordering::Relaxed);
        if first == 0 {
            self.first_stack_address.store(address, Ordering::Relaxed);
        } else {
            self.max_stack_growth
                .fetch_max(first.abs_diff(address), Ordering::Relaxed);
        }
        self.remaining.store(remaining - 1, Ordering::Relaxed);
        task.set_preempt_pending(true);
    }
}

struct ArmedProbe<'a>(&'a Probe);

impl Drop for ArmedProbe<'_> {
    fn drop(&mut self) {
        self.0.remaining.store(0, Ordering::Relaxed);
        self.0.first_stack_address.store(0, Ordering::Relaxed);
        self.0.max_stack_growth.store(0, Ordering::Relaxed);
    }
}

#[def_test(serial)]
fn renewed_preemption_requests_do_not_accumulate_stack_frames() {
    let task = crate::current();
    let probe = &task.preempt_check_probe;
    let _fixture = ArmedProbe(probe);
    // Measure only the injected guard-release requests. An unrelated IRQ tail
    // can enter the same check with an exception frame and inflate the address
    // difference without recursive guard release. This guard leaves preemption
    // enabled so an actual guard-drop recursion remains observable.
    let _irq = kspin::IrqSave::new();
    assert!(task.can_preempt(0));
    {
        let _guard = kspin::NoPreempt::new();
        probe.remaining.store(256, Ordering::Relaxed);
        task.set_preempt_pending(true);
    }
    assert_eq!(probe.remaining.load(Ordering::Relaxed), 0);
    assert!(probe.first_stack_address.load(Ordering::Relaxed) != 0);
    assert!(
        probe.max_stack_growth.load(Ordering::Relaxed) < 1024,
        "renewed requests accumulated preemption-check stack frames"
    );
    assert!(task.can_preempt(0));
}
