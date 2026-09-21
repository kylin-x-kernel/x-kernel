// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real timer IRQ and fixed-notification integration, without manual dispatch.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcpu_id_map::LogicalCpuId;
use ktime_types::{MonotonicInstant, TimeSpan};
use ktimer_core::{Expiration, Timer, TimerControl, TimerHandler};
use unittest::{assert, assert_eq, def_test};

use crate::timers::PREPARED_TIMERS;

fn now() -> u64 {
    khal::time::monotonic_time_nanos()
}

fn instant(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

struct Probe {
    cpu: AtomicUsize,
    calls: AtomicUsize,
    is_valid: AtomicBool,
    repeats: usize,
}

impl Probe {
    fn new(cpu: LogicalCpuId, repeats: usize) -> Self {
        Self {
            cpu: AtomicUsize::new(cpu.as_usize()),
            calls: AtomicUsize::new(0),
            is_valid: AtomicBool::new(true),
            repeats,
        }
    }
}

impl TimerHandler for Probe {
    fn on_expiration(&self, control: TimerControl<'_>, event: Expiration) {
        if karch::local_irq_enabled()
            || !kirq::context::is_in_interrupt_context()
            || khal::percpu::this_cpu_id().as_usize() != self.cpu.load(Ordering::Acquire)
            || now() < event.deadline_ns
        {
            self.is_valid.store(false, Ordering::Relaxed);
        }
        let calls = self.calls.load(Ordering::Relaxed) + 1;
        if calls < self.repeats && control.rearm(instant(now() + 2_000_000)).is_err() {
            self.is_valid.store(false, Ordering::Relaxed);
        }
        self.calls.store(calls, Ordering::Release);
    }
}

fn refresh_local() {
    let _guard = kspin::NoPreemptIrqSave::new();
    crate::api::rearm_local_timer();
}

fn wait_for(probe: &Probe, count: usize) -> bool {
    let limit = now() + 2_000_000_000;
    while probe.calls.load(Ordering::Acquire) < count && now() < limit {
        core::hint::spin_loop();
    }
    probe.calls.load(Ordering::Acquire) == count
}

fn local_delivery(repeats: usize) -> bool {
    if !karch::local_irq_enabled() {
        return false;
    }
    let _pin = kspin::NoPreempt::new();
    let cpu = khal::percpu::this_cpu_id();
    let timer = crate::Timer::new(Probe::new(cpu, repeats));
    // SAFETY: this initialized CPU is pinned; only its real IRQ dispatches.
    // Storage is owned, the handler is bounded and refresh follows acceptance.
    let started = unsafe { timer.start(instant(now() + 2_000_000)) }.is_ok();
    let delivered = wait_for(timer.handler(), repeats);
    timer.cancel_sync();
    refresh_local();
    started && delivered && timer.handler().is_valid.load(Ordering::Acquire)
}

#[def_test(serial)]
fn prepared_one_shot_runs_from_real_timer_irq() {
    assert!(local_delivery(1));
}

#[def_test(serial)]
fn prepared_explicit_rearm_runs_from_real_timer_irq() {
    assert!(local_delivery(3));
}

#[def_test(serial)]
fn callback_can_start_cancel_and_replace_another_prepared_timer() {
    use alloc::sync::Arc;

    assert!(karch::local_irq_enabled());
    let _pin = kspin::NoPreempt::new();
    let cpu = khal::percpu::this_cpu_id();
    let child = Arc::new(crate::Timer::new(Probe::new(cpu, 1)));
    let is_valid = Arc::new(AtomicBool::new(true));
    let callback_child = child.clone();
    let callback_valid = is_valid.clone();
    let parent = crate::Timer::new(move |_: TimerControl<'_>, _: Expiration| {
        // SAFETY: real IRQ on the pinned initialized CPU, child ownership
        // survives the callback. No timer lock is held; adapter refresh joins
        // the IRQ batch and never dispatches the child inline.
        let first = unsafe { callback_child.start(instant(now() + 1_000_000_000)) };
        let removed = callback_child.try_cancel() == ktimer_core::CancelResult::Removed;
        // SAFETY: same owned child and local service; replace with a near
        // deadline and arrange local hardware refresh after registration.
        let second = unsafe { callback_child.start(instant(now() + 2_000_000)) };
        if first.is_err()
            || !removed
            || second.is_err()
            || callback_child.handler().calls.load(Ordering::Acquire) != 0
        {
            callback_valid.store(false, Ordering::Release);
        }
    });
    // SAFETY: owned registration on this initialized pinned CPU; refresh follows
    // acceptance, real timer IRQ is the only dispatcher and callbacks are bounded.
    let started = unsafe { parent.start(instant(now() + 2_000_000)) }.is_ok();
    let delivered = wait_for(child.handler(), 1);
    parent.cancel_sync();
    child.cancel_sync();
    refresh_local();
    assert!(started && delivered && is_valid.load(Ordering::Acquire));
    assert!(child.handler().is_valid.load(Ordering::Acquire));
}

#[cfg(feature = "ipi")]
#[def_test(serial)]
fn reprogram_endpoint_does_not_deliver_prepared_callbacks() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let cpu = khal::percpu::this_cpu_id();
    let timer = Timer::new(&PREPARED_TIMERS, Probe::new(cpu, 1));
    // SAFETY: initialized CPU, IRQs masked, static set and owned storage; the
    // endpoint below programs hardware but cannot recursively deliver callbacks.
    let started = unsafe { timer.start_on(cpu, instant(0)) }.is_ok();
    kipi::TimerReprogramIpiIf::on_timer_reprogram();
    let calls = timer.handler().calls.load(Ordering::Acquire);
    let removed = timer.cancel_sync();
    refresh_local();
    assert!(started && removed);
    assert_eq!(calls, 0);
}

#[cfg(all(feature = "smp", feature = "ipi"))]
#[def_test(serial)]
fn pending_migration_and_remote_advance_use_real_ipi() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    assert!(karch::local_irq_enabled());
    let _pin = kspin::NoPreempt::new();
    let local = khal::percpu::this_cpu_id();
    let remote: LogicalCpuId = if local.as_usize() == 0 { 1 } else { 0 }.into();
    let timer = Timer::new(&PREPARED_TIMERS, Probe::new(local, 1));
    for round in 0..8 {
        let destination = if round % 2 == 0 { remote } else { local };
        let source = if destination == local { remote } else { local };
        timer
            .handler()
            .cpu
            .store(destination.as_usize(), Ordering::Release);
        // SAFETY: both CPUs are initialized and serviced by real IRQs. The
        // owned timer remains live, and every returned owner is notified after
        // releasing its locks. The far deadline keeps the initial setting pending.
        let initial = unsafe { timer.start_on(source, instant(now() + 5_000_000_000)) };
        if let Ok((owner, _)) = initial {
            kipi::notify_timer_reprogram(owner).unwrap();
        }
        // SAFETY: same service/lifetime contract; migration precedes the far
        // deadline. Each replacement explicitly notifies the actual owner.
        let migrated = unsafe { timer.start_on(destination, instant(now() + 4_000_000_000)) };
        if let Ok((owner, _)) = migrated {
            kipi::notify_timer_reprogram(owner).unwrap();
        }
        // SAFETY: same initialized destination; advance must not wait for the
        // former multi-second hardware deadline. No notification runs callbacks.
        let advanced = unsafe { timer.start_on(destination, instant(now() + 2_000_000)) };
        if let Ok((owner, _)) = advanced {
            kipi::notify_timer_reprogram(owner).unwrap();
            kipi::notify_reschedule(owner).unwrap();
        }
        let delivered = wait_for(timer.handler(), round + 1);
        timer.cancel_sync();
        kipi::notify_timer_reprogram(local).unwrap();
        kipi::notify_timer_reprogram(remote).unwrap();
        assert_eq!(initial, Ok((source, true)));
        assert_eq!(migrated, Ok((destination, false)));
        assert_eq!(advanced, Ok((destination, false)));
        assert!(delivered && timer.handler().is_valid.load(Ordering::Acquire));
    }
}
