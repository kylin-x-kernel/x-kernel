// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real-context finalization and caller-CPU allocation probes.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kalloc::current_cpu_allocated_bytes_for_test as allocated_bytes;
use kfinalize::{DeferredArc, DeferredWeak};
use ktime_types::TimeSpan;
use unittest::def_test;

static BOOT_DROPS: AtomicUsize = AtomicUsize::new(0);
static BOOT_CHILD_DROPS: AtomicUsize = AtomicUsize::new(0);
static BOOT_PREPARED: AtomicUsize = AtomicUsize::new(0);
const BOOT_BACKLOG: usize = 193;

struct BootPayload;

struct BootChild;

impl Drop for BootChild {
    fn drop(&mut self) {
        assert!(ktask::is_sleepable_context());
        BOOT_CHILD_DROPS.fetch_add(1, Ordering::Release);
    }
}

impl Drop for BootPayload {
    fn drop(&mut self) {
        assert!(ktask::is_sleepable_context());
        ktask::sleep(TimeSpan::from_millis(1));
        if BOOT_DROPS.fetch_add(1, Ordering::Release) == 0 {
            let child =
                DeferredArc::new(BootChild).unwrap_or_else(|_| panic!("boot provider unready"));
            drop(child);
        }
    }
}

pub(crate) fn prepare_boot_probe() {
    prepare_boot_objects(BOOT_BACKLOG);
}

pub(crate) fn prepare_local_boot_probe() {
    prepare_boot_objects(1);
}

fn prepare_boot_objects(count: usize) {
    assert!(matches!(
        DeferredArc::new(0usize),
        Err((kfinalize::CreateError::NotReady, 0))
    ));
    for _ in 0..count {
        // SAFETY: runtime boot call sites follow local per-CPU/trap/mapping and
        // global allocator setup. They are not IRQ/NMI handlers and hold no
        // allocator lock. The provider was published before AP startup.
        let owner = unsafe { kfinalize::service::new_integration(BootPayload) }
            .unwrap_or_else(|_| panic!("boot preparation unavailable"));
        let weak = DeferredArc::downgrade(&owner);
        drop(owner);
        assert!(weak.upgrade().is_none());
    }
    BOOT_PREPARED.fetch_add(count, Ordering::Release);
    assert_eq!(BOOT_DROPS.load(Ordering::Acquire), 0);
}

#[def_test(serial)]
fn early_retirement_survives_until_normal_services_bind() {
    let expected = BOOT_PREPARED.load(Ordering::Acquire);
    let secondary_count = if cfg!(feature = "smp") {
        kcpu_id_map::nr_cpus() - 1
    } else {
        0
    };
    assert_eq!(expected, BOOT_BACKLOG + 1 + 2 * secondary_count);
    let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
    while BOOT_DROPS.load(Ordering::Acquire) != expected
        || BOOT_CHILD_DROPS.load(Ordering::Acquire) != 1
    {
        assert!(khal::time::monotonic_time() < deadline);
        ktask::sleep(TimeSpan::from_millis(1));
    }
    assert_eq!(BOOT_DROPS.load(Ordering::Acquire), expected);
}

#[derive(Clone, Copy, Debug)]
enum Source {
    Task,
    HardIrq,
    SoftIrq,
}

#[derive(Debug)]
struct Payload {
    drops: Arc<AtomicUsize>,
    submitting_cpu: Arc<AtomicUsize>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        assert_ne!(
            self.submitting_cpu.load(Ordering::Acquire),
            khal::percpu::this_cpu_id().as_usize() + 1
        );
        assert!(!kirq::context::is_in_hardirq());
        assert!(!kirq::context::is_serving_softirq());
        assert!(ktask::is_sleepable_context());
        ktask::sleep(TimeSpan::from_millis(1));
        self.drops.fetch_add(1, Ordering::Release);
    }
}

struct PreparedRelease {
    normal: DeferredArc<Payload>,
    normal_weak: DeferredWeak<Payload>,
    submitting_cpu: Arc<AtomicUsize>,
}

impl PreparedRelease {
    fn new(normal_drops: &Arc<AtomicUsize>) -> Self {
        let submitting_cpu = Arc::new(AtomicUsize::new(0));
        let normal = DeferredArc::new(Payload {
            drops: normal_drops.clone(),
            submitting_cpu: submitting_cpu.clone(),
        })
        .unwrap();
        let normal_weak = DeferredArc::downgrade(&normal);
        Self {
            normal,
            normal_weak,
            submitting_cpu,
        }
    }

    fn release(self, source: Source) {
        let irq = kspin::NoPreemptIrqSave::new();
        match source {
            Source::Task => assert!(!kirq::context::is_in_interrupt_context()),
            Source::HardIrq => assert!(kirq::context::is_in_hardirq()),
            Source::SoftIrq => {
                assert!(kirq::context::is_serving_softirq());
                assert!(!kirq::context::is_in_hardirq());
            }
        }
        self.submitting_cpu.store(
            khal::percpu::this_cpu_id().as_usize() + 1,
            Ordering::Release,
        );
        let before = allocated_bytes(&irq);
        drop(self.normal);
        assert!(self.normal_weak.upgrade().is_none());
        drop(self.normal_weak);
        assert_eq!(allocated_bytes(&irq), before);
        self.submitting_cpu.store(0, Ordering::Release);
    }
}

fn wait_for_release(done: &AtomicBool, normal: &Arc<AtomicUsize>) {
    let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
    while khal::time::monotonic_time() < deadline {
        if done.load(Ordering::Acquire)
            && normal.load(Ordering::Acquire) == 1
            && Arc::strong_count(normal) == 1
        {
            break;
        }
        ktask::sleep(TimeSpan::from_millis(1));
    }
}

/// Normal finalization accepts real task, timer IRQ and softirq submission.
///
/// # Panics
/// Panics on wrong source/target context, local allocation, inline Drop, Weak
/// resurrection, duplicate destruction or missing bounded completion.
#[def_test(serial)]
fn deferred_last_references_cross_real_contexts_without_allocating() {
    for source in [Source::Task, Source::HardIrq, Source::SoftIrq] {
        let normal_drops = Arc::new(AtomicUsize::new(0));
        let prepared = PreparedRelease::new(&normal_drops);
        let slot = kspin::SpinNoIrq::new(Some(prepared));
        let done = Arc::new(AtomicBool::new(false));
        let producer_done = done.clone();
        match source {
            Source::Task => {
                let prepared = slot.lock().take().unwrap();
                prepared.release(source);
                done.store(true, Ordering::Release);
                wait_for_release(&done, &normal_drops);
            }
            Source::HardIrq => {
                let timer =
                    ktask::Timer::new(move |_: ktask::TimerControl<'_>, _: ktask::Expiration| {
                        let prepared = slot.lock().take().unwrap();
                        prepared.release(source);
                        producer_done.store(true, Ordering::Release);
                    });
                // SAFETY: boot timers are ready; this bounded, non-sleeping
                // callback owns its prepared. The test drains delivery before dropping
                // timer/captures, even if the bounded completion wait expires.
                unsafe { timer.start(khal::time::monotonic_time() + TimeSpan::from_millis(1)) }
                    .unwrap();
                wait_for_release(&done, &normal_drops);
                timer.cancel_sync();
            }
            Source::SoftIrq => {
                let work = core::pin::pin!(kworkng::Work::with_callback(move || {
                    let prepared = slot.lock().take().unwrap();
                    prepared.release(source);
                    producer_done.store(true, Ordering::Release);
                }));
                // SAFETY: pinned Work and captures remain live through cancel_sync.
                // The sole callback is non-sleeping and no producer can resubmit it.
                unsafe { kworkng::DefaultQueue::BottomHalf.queue_work(work.as_ref(), None) }
                    .unwrap();
                wait_for_release(&done, &normal_drops);
                work.cancel_sync().unwrap();
            }
        }
        assert!(done.load(Ordering::Acquire));
        assert_eq!(normal_drops.load(Ordering::Acquire), 1);
        assert_eq!(Arc::strong_count(&normal_drops), 1);
        unittest::ktest_println!("deferred source={source:?}: submit_alloc_bytes=0 normal_drops=1");
    }
}
