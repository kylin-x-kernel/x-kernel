// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

#[unittest::def_test]
fn report_deferred_carrier_layout() {
    let (size, alignment) = super::finalize::carrier_layout_for_test();
    unittest::ktest_println!(
        "deferred layout: carrier_bytes={} carrier_align={} work_bytes={}",
        size,
        alignment,
        core::mem::size_of::<crate::Work<fn()>>()
    );
}
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kfinalize::{CreateError, DeferredArc};
use ktime_types::TimeSpan;
use unittest::def_test;

struct PublicationProbe {
    is_active: AtomicBool,
    observed: AtomicBool,
    drops: Arc<AtomicUsize>,
}

static PUBLICATION_PROBE: klazy::Once<PublicationProbe> = klazy::Once::new();

static SUBMITTING_CPU: AtomicUsize = AtomicUsize::new(0);

struct SubmissionGuard {
    _irq: kspin::NoPreemptIrqSave,
}

impl SubmissionGuard {
    fn new() -> Self {
        let guard = Self {
            _irq: kspin::NoPreemptIrqSave::new(),
        };
        SUBMITTING_CPU.store(
            khal::percpu::this_cpu_id().as_usize() + 1,
            Ordering::Release,
        );
        guard
    }
}

impl Drop for SubmissionGuard {
    fn drop(&mut self) {
        SUBMITTING_CPU.store(0, Ordering::Release);
    }
}

fn assert_not_inline() {
    assert_ne!(
        SUBMITTING_CPU.load(Ordering::Acquire),
        khal::percpu::this_cpu_id().as_usize() + 1
    );
}

static EMPTY_ARMED: AtomicBool = AtomicBool::new(false);
static EMPTY_REACHED: AtomicBool = AtomicBool::new(false);
static EMPTY_RELEASED: AtomicBool = AtomicBool::new(false);

struct EmptyRelease;

impl Drop for EmptyRelease {
    fn drop(&mut self) {
        EMPTY_ARMED.store(false, Ordering::Release);
        EMPTY_RELEASED.store(true, Ordering::Release);
    }
}

pub(super) fn before_empty_return() {
    if ktask::is_sleepable_context() && EMPTY_ARMED.swap(false, Ordering::AcqRel) {
        EMPTY_REACHED.store(true, Ordering::Release);
        while !EMPTY_RELEASED.load(Ordering::Acquire) {
            ktask::sleep(TimeSpan::from_millis(1));
        }
    }
}

#[def_test(serial)]
fn shared_finalization_accepts_enqueue_after_empty_observation() {
    let drops = Arc::new(AtomicUsize::new(0));
    let first = DeferredArc::new(Payload {
        drops: drops.clone(),
        value: 1,
    })
    .unwrap();
    let second = DeferredArc::new(Payload {
        drops: drops.clone(),
        value: 2,
    })
    .unwrap();
    EMPTY_REACHED.store(false, Ordering::Release);
    EMPTY_RELEASED.store(false, Ordering::Release);
    let release = EmptyRelease;
    EMPTY_ARMED.store(true, Ordering::Release);
    drop(first);
    wait_until(|| EMPTY_REACHED.load(Ordering::Acquire));
    let before = drops.load(Ordering::Acquire);
    drop(second);
    assert_eq!(drops.load(Ordering::Acquire), before);
    drop(release);
    wait_until(|| drops.load(Ordering::Acquire) == 2 && Arc::strong_count(&drops) == 1);
}

#[def_test(serial)]
fn shared_destructor_can_enqueue_another_finalization() {
    let drops = Arc::new(AtomicUsize::new(0));
    let child = DeferredArc::new(Payload {
        drops: drops.clone(),
        value: 3,
    })
    .unwrap();
    let weak = DeferredArc::downgrade(&child);
    let parent = DeferredArc::new(child).map_err(|(error, _)| error).unwrap();
    drop(parent);
    wait_until(|| drops.load(Ordering::Acquire) == 1 && Arc::strong_count(&drops) == 1);
    assert!(weak.upgrade().is_none());
}

pub(super) fn publication_guard() -> Option<kspin::NoPreemptIrqSave> {
    PUBLICATION_PROBE
        .get()
        .filter(|probe| probe.is_active.load(Ordering::Acquire))
        .map(|_| kspin::NoPreemptIrqSave::new())
}

pub(super) fn after_publish(carrier_owners: impl Fn() -> usize) {
    let Some(probe) = PUBLICATION_PROBE.get() else {
        return;
    };
    if !probe.is_active.load(Ordering::Acquire) {
        return;
    }
    assert!(ktask::is_sleepable_context());
    wait_until(|| {
        probe.drops.load(Ordering::Acquire) == 1
            && Arc::strong_count(&probe.drops) == 1
            && carrier_owners() == 1
    });
    probe.observed.store(true, Ordering::Release);
}

#[derive(Debug)]
struct Payload {
    drops: Arc<AtomicUsize>,
    value: usize,
}

impl Drop for Payload {
    fn drop(&mut self) {
        assert_not_inline();
        assert!(ktask::is_sleepable_context());
        ktask::sleep(TimeSpan::from_millis(1));
        self.drops.fetch_add(1, Ordering::Release);
    }
}

fn wait_until(mut done: impl FnMut() -> bool) {
    let limit = khal::time::monotonic_time() + TimeSpan::from_secs(5);
    while !done() && khal::time::monotonic_time() < limit {
        ktask::sleep(TimeSpan::from_millis(1));
    }
    assert!(done(), "deferred finalization did not make progress");
}

/// Last strong Drop from an IRQ-masked caller hands sleeping Drop to a worker.
///
/// # Panics
/// Panics on inline destruction, wrong context, resurrection or lost destruction.
#[def_test(serial)]
fn deferred_payload_sleeps_only_on_normal_worker() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = DeferredArc::new(Payload {
        drops: drops.clone(),
        value: 42,
    })
    .unwrap();
    let weak = DeferredArc::downgrade(&owner);
    {
        let _submission = SubmissionGuard::new();
        drop(owner);
        assert!(weak.upgrade().is_none());
    }
    wait_until(|| drops.load(Ordering::Acquire) == 1 && Arc::strong_count(&drops) == 1);
    drop(weak);
}

/// Invalid creation context returns the original payload without destroying it.
///
/// # Panics
/// Panics if creation succeeds under IRQ masking or loses the input value.
#[def_test(serial)]
fn deferred_creation_rejects_atomic_context() {
    let drops = Arc::new(AtomicUsize::new(0));
    let result = {
        let _irq = kspin::NoPreemptIrqSave::new();
        DeferredArc::new(Payload {
            drops: drops.clone(),
            value: 73,
        })
    };
    let Err((error, value)) = result else {
        panic!("atomic creation succeeded")
    };
    assert_eq!(error, CreateError::InvalidContext);
    assert_eq!(value.value, 73);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

/// Concurrent upgrade and final strong Drop cannot resurrect or free a borrowed T.
///
/// # Panics
/// Panics on stale payload access, failed join or missing/excess destruction.
#[def_test(serial)]
fn deferred_upgrade_races_last_owner() {
    for _ in 0..32 {
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = DeferredArc::new(Payload {
            drops: drops.clone(),
            value: 97,
        })
        .unwrap();
        let weak = DeferredArc::downgrade(&owner);
        let reader_weak = weak.clone();
        let ready = Arc::new(AtomicBool::new(false));
        let reader_ready = ready.clone();
        let reader = ktask::spawn(move || {
            reader_ready.store(true, Ordering::Release);
            for _ in 0..128 {
                let Some(strong) = reader_weak.upgrade() else {
                    break;
                };
                assert_eq!(strong.value, 97);
                assert_eq!(strong.drops.load(Ordering::Acquire), 0);
                ktask::yield_now();
            }
        });
        wait_until(|| ready.load(Ordering::Acquire));
        drop(owner);
        assert_eq!(reader.join(), 0);
        assert!(weak.upgrade().is_none());
        wait_until(|| drops.load(Ordering::Acquire) == 1 && Arc::strong_count(&drops) == 1);
    }
}

/// The executor finishes while the publishing call still retains its carrier.
///
/// # Panics
/// Panics if the worker cannot finish before publication returns or loses storage.
#[def_test(serial)]
fn deferred_worker_finishes_before_publisher_returns() {
    let probe = PUBLICATION_PROBE.call_once(|| PublicationProbe {
        is_active: AtomicBool::new(false),
        observed: AtomicBool::new(false),
        drops: Arc::new(AtomicUsize::new(0)),
    });
    probe.drops.store(0, Ordering::Release);
    probe.observed.store(false, Ordering::Release);
    let owner = DeferredArc::new(Payload {
        drops: probe.drops.clone(),
        value: 42,
    })
    .unwrap();
    let weak = DeferredArc::downgrade(&owner);
    probe.is_active.store(true, Ordering::Release);
    drop(owner);
    probe.is_active.store(false, Ordering::Release);
    assert!(weak.upgrade().is_none());
    assert!(probe.observed.load(Ordering::Acquire));
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
}

/// A backlog larger than the finalizer batch survives continuation passes.
///
/// # Panics
/// Panics on inline destruction, wrong route, resurrection or lost/duplicate work.
#[def_test(serial)]
fn deferred_finalization_preserves_backlog_and_weak_policy() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut owners = alloc::vec::Vec::new();
    let mut weaks = alloc::vec::Vec::new();
    for _ in 0..193 {
        let owner = DeferredArc::new(Payload {
            drops: drops.clone(),
            value: 42,
        })
        .unwrap();
        let weak = DeferredArc::downgrade(&owner);
        let other = owner.clone();
        drop(owner);
        let upgraded = weak.clone().upgrade().unwrap();
        drop(other);
        owners.push(upgraded);
        weaks.push(weak);
    }
    {
        let _submission = SubmissionGuard::new();
        drop(owners);
        assert!(weaks.iter().all(|weak| weak.upgrade().is_none()));
    }
    wait_until(|| drops.load(Ordering::Acquire) == 193 && Arc::strong_count(&drops) == 1);
}

/// Busy normal workers delay but do not cancel finalization.
///
/// # Panics
/// Panics on wrong worker capacity, lost finalization, resurrection or failed drain.
#[def_test(serial)]
fn deferred_obligations_survive_busy_normal_pool() {
    use kcpu_id_map::KCpuMaskExt;

    let original_mask = ktask::current().cpumask();
    let target = khal::percpu::this_cpu_id();
    assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(target)
    ));
    let entered = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let mut blockers = alloc::vec::Vec::new();
    for _ in 0..kbuild_config::WORKQUEUE_WORKERS_PER_POOL {
        let entered = entered.clone();
        let release = release.clone();
        let blocker = crate::WorkHandle::new(move || {
            entered.fetch_add(1, Ordering::Release);
            while !release.load(Ordering::Acquire) {
                ktask::sleep(TimeSpan::from_millis(1));
            }
        })
        .unwrap();
        blocker.queue_default(Some(target)).unwrap();
        blockers.push(blocker);
    }
    wait_until(|| entered.load(Ordering::Acquire) == kbuild_config::WORKQUEUE_WORKERS_PER_POOL);
    let normal_drops = Arc::new(AtomicUsize::new(0));
    let normal = DeferredArc::new(Payload {
        drops: normal_drops.clone(),
        value: 42,
    })
    .unwrap();
    let normal_weak = DeferredArc::downgrade(&normal);
    let ordinary_calls = Arc::new(AtomicUsize::new(0));
    let calls = ordinary_calls.clone();
    let ordinary = crate::WorkHandle::new(move || {
        calls.fetch_add(1, Ordering::Release);
    })
    .unwrap();
    ordinary.queue_default(Some(target)).unwrap();
    drop(normal);
    assert!(normal_weak.upgrade().is_none());
    assert_eq!(normal_drops.load(Ordering::Acquire), 0);
    assert_eq!(ordinary_calls.load(Ordering::Acquire), 0);
    release.store(true, Ordering::Release);
    for blocker in &blockers {
        blocker.shutdown().unwrap();
    }
    ordinary.flush().unwrap();
    ordinary.shutdown().unwrap();
    wait_until(|| {
        normal_drops.load(Ordering::Acquire) == 1 && Arc::strong_count(&normal_drops) == 1
    });
    assert_eq!(ordinary_calls.load(Ordering::Acquire), 1);
    drop(blockers);
    drop(ordinary);
    wait_until(|| {
        Arc::strong_count(&entered) == 1
            && Arc::strong_count(&release) == 1
            && Arc::strong_count(&ordinary_calls) == 1
    });
    assert!(ktask::set_current_affinity(original_mask));
}
