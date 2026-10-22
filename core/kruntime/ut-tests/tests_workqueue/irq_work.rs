// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live IRQ/NMI handoff probes, separate from deterministic kirq unit tests.

use alloc::{boxed::Box, sync::Arc};
use core::{
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use kcpu_id_map::KCpuMaskExt;
use kirq::irq_work::IrqWork;
use ktime_types::TimeSpan;
use kworkng::{DefaultQueue, Work};
use unittest::def_test;

struct Counters {
    completed: AtomicUsize,
    invalid_context: AtomicBool,
}

#[repr(C)]
struct Probe {
    work: IrqWork,
    next_for_nmi: *mut Probe,
    counters: Arc<Counters>,
    cpu: usize,
    worker: *const Work<fn()>,
}

static WORKER_RUNS: AtomicUsize = AtomicUsize::new(0);

fn worker_callback() {
    assert!(ktask::is_sleepable_context());
    ktask::sleep(TimeSpan::from_millis(1));
    WORKER_RUNS.fetch_add(1, Ordering::Release);
}

/// # Safety
/// work is the first field of a uniquely transferred Probe allocation. The
/// optional worker pointer remains pinned through completion and explicit flush.
unsafe fn consume(work: NonNull<IrqWork>) {
    // SAFETY: repr(C) places work first; the callback owns its detached Box.
    let probe = unsafe { Box::from_raw(work.cast::<Probe>().as_ptr()) };
    let counters = probe.counters.clone();
    if kirq::context::is_in_nmi()
        || !kirq::context::is_in_hardirq()
        || khal::percpu::this_cpu_id().as_usize() != probe.cpu
    {
        counters.invalid_context.store(true, Ordering::Relaxed);
    }
    if !probe.worker.is_null() {
        // SAFETY: the pinned task-owned Work outlives the callback and flush.
        let worker = unsafe { &*probe.worker };
        // SAFETY: task ownership keeps the Work pinned until explicit flush.
        let worker = unsafe { Pin::new_unchecked(worker) };
        // SAFETY: the same retained Work lifetime covers admission and execution.
        unsafe { DefaultQueue::Normal.queue_work(worker, None) }.unwrap();
    }
    drop(probe);
    counters.completed.fetch_add(1, Ordering::Release);
}

fn counters() -> Arc<Counters> {
    Arc::new(Counters {
        completed: AtomicUsize::new(0),
        invalid_context: AtomicBool::new(false),
    })
}

fn prepare_chain(count: usize, counters: &Arc<Counters>, worker: *const Work<fn()>) -> *mut Probe {
    let mut head = ptr::null_mut();
    for index in 0..count {
        head = Box::into_raw(Box::new(Probe {
            work: IrqWork::new(consume),
            next_for_nmi: head,
            counters: counters.clone(),
            cpu: khal::percpu::this_cpu_id().as_usize(),
            worker: if index == 0 { worker } else { ptr::null() },
        }));
    }
    head
}

/// # Safety
/// The caller owns this entire prepared chain, pins execution and retains its
/// containing fixture until completion. Nodes transfer once and self-free.
unsafe fn submit_chain(mut head: *mut Probe) -> bool {
    while let Some(pointer) = NonNull::new(head) {
        // SAFETY: this node is still privately owned; save next before publishing.
        head = unsafe { (*pointer.as_ptr()).next_for_nmi };
        // SAFETY: the prepared Box transfers to consume, which owns cleanup.
        if unsafe { kirq::irq_work::queue(pointer.cast()) }.is_err() {
            return false;
        }
    }
    true
}

fn wait_for(counters: &Counters, expected: usize) {
    let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
    while counters.completed.load(Ordering::Acquire) != expected {
        assert!(
            khal::time::monotonic_time() < deadline,
            "irq_work delivery timed out"
        );
        ktask::yield_now();
    }
    assert!(!counters.invalid_context.load(Ordering::Acquire));
}

fn on_each_cpu(action: fn()) {
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            action,
            "irq_work_probe".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
        ktask::activate_task(&task);
        assert_eq!(task.join(), 0);
    });
}

fn ordinary_probe() {
    if !kirq::irq_work::is_ready() {
        return;
    }
    let counters = counters();
    let worker = Box::pin(Work::with_callback(worker_callback as fn()));
    let before = WORKER_RUNS.load(Ordering::Acquire);
    let count = kirq::irq_work::CALLBACK_BUDGET * 3 + 1;
    let head = prepare_chain(count, &counters, worker.as_ref().get_ref());
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        // SAFETY: the fixture pins CPU and excludes IRQ until all nodes publish.
        assert!(unsafe { submit_chain(head) });
        assert_eq!(counters.completed.load(Ordering::Acquire), 0);
        kirq::irq_work::handle_irq();
        assert_eq!(counters.completed.load(Ordering::Acquire), 0);
    }
    wait_for(&counters, count);
    worker.flush().unwrap();
    assert_eq!(WORKER_RUNS.load(Ordering::Acquire), before + 1);
}

#[def_test(serial)]
fn local_irq_work_batches_self_free_and_handoff_on_each_cpu() {
    on_each_cpu(ordinary_probe);
    unittest::ktest_println!(
        "irq_work layout: node={} bytes, local callbacks/batch={}",
        core::mem::size_of::<IrqWork>(),
        kirq::irq_work::CALLBACK_BUDGET
    );
}

#[cfg(all(
    feature = "irq-work-test",
    target_arch = "aarch64",
    feature = "nmi",
    feature = "pmu",
    not(feature = "watchdog")
))]
mod hardware_nmi {
    use core::sync::atomic::AtomicPtr;

    use kfinalize::{DeferredArc, DeferredWeak};

    use super::*;

    static PENDING: [AtomicPtr<Probe>; kbuild_config::NR_CPUS] =
        [const { AtomicPtr::new(ptr::null_mut()) }; kbuild_config::NR_CPUS];
    static SUBMITTED: [AtomicBool; kbuild_config::NR_CPUS] =
        [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS];
    static FAILED: AtomicBool = AtomicBool::new(false);
    static RETIREMENT: [AtomicPtr<RetirementCase>; kbuild_config::NR_CPUS] =
        [const { AtomicPtr::new(ptr::null_mut()) }; kbuild_config::NR_CPUS];
    static RECLAIMED: [AtomicUsize; kbuild_config::NR_CPUS] =
        [const { AtomicUsize::new(0) }; kbuild_config::NR_CPUS];

    #[derive(Debug)]
    struct SleepablePayload(usize);

    impl Drop for SleepablePayload {
        fn drop(&mut self) {
            assert!(ktask::is_sleepable_context());
            ktask::sleep(TimeSpan::from_millis(1));
            RECLAIMED[self.0].fetch_add(1, Ordering::Release);
        }
    }

    struct RetirementCase {
        no_weak: Option<DeferredArc<SleepablePayload>>,
        with_weak: Option<DeferredArc<SleepablePayload>>,
        weak: Option<DeferredWeak<SleepablePayload>>,
        late_weak: Option<DeferredWeak<SleepablePayload>>,
    }

    fn wait_for_reclamation(cpu: usize, expected: usize) {
        let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
        while RECLAIMED[cpu].load(Ordering::Acquire) != expected {
            assert!(khal::time::monotonic_time() < deadline);
            ktask::yield_now();
        }
    }

    fn nmi_callback() {
        let cpu = khal::percpu::this_cpu_id().as_usize();
        let head = PENDING[cpu].swap(ptr::null_mut(), Ordering::Acquire);
        if head.is_null() {
            return;
        }
        if !kirq::context::is_in_nmi() {
            FAILED.store(true, Ordering::Release);
        }
        let retirement = RETIREMENT[cpu].load(Ordering::Acquire);
        // SAFETY: the task retains this Box until SUBMITTED's release/acquire
        // handoff. This one NMI is its exclusive accessor during that interval.
        let retirement = unsafe { &mut *retirement };
        drop(retirement.no_weak.take());
        drop(retirement.with_weak.take());
        if retirement
            .weak
            .as_ref()
            .and_then(DeferredWeak::upgrade)
            .is_some()
        {
            FAILED.store(true, Ordering::Release);
        }
        drop(retirement.weak.take());
        drop(retirement.late_weak.take());
        // SAFETY: the task release-published a private prepared chain; this CPU's
        // real NMI takes ownership once and cannot run the ordinary IRQ consumer.
        if !unsafe { submit_chain(head) } {
            FAILED.store(true, Ordering::Release);
        }
        SUBMITTED[cpu].store(true, Ordering::Release);
    }

    fn probe() {
        if khal::nmi::mode() == khal::nmi::NmiMode::None {
            unittest::ktest_println!("irq_work real NMI: skipped (NMI mechanism unavailable)");
            return;
        }
        assert!(kirq::irq_work::is_ready());
        let cpu = khal::percpu::this_cpu_id().as_usize();
        let retired = DeferredArc::new(SleepablePayload(cpu)).unwrap();
        let late_weak = DeferredArc::downgrade(&retired);
        drop(retired);
        wait_for_reclamation(cpu, 1);
        let with_weak = DeferredArc::new(SleepablePayload(cpu)).unwrap();
        let weak = DeferredArc::downgrade(&with_weak);
        let retirement = Box::into_raw(Box::new(RetirementCase {
            no_weak: Some(DeferredArc::new(SleepablePayload(cpu)).unwrap()),
            with_weak: Some(with_weak),
            weak: Some(weak),
            late_weak: Some(late_weak),
        }));
        RETIREMENT[cpu].store(retirement, Ordering::Release);
        let counters = counters();
        let worker = Box::pin(Work::with_callback(worker_callback as fn()));
        let before = WORKER_RUNS.load(Ordering::Acquire);
        let count = kirq::irq_work::CALLBACK_BUDGET * 2 + 1;
        let head = prepare_chain(count, &counters, worker.as_ref().get_ref());
        irq_driver::gic::enable(of::pmu_irq_or(kbuild_config::PMU_IRQ), false);
        assert!(khal::nmi::enable_periodic_nmi(100_000, nmi_callback));
        khal::pmu::enable_irq();
        let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
        assert!(irq_driver::gic::with_irq_work_gic_lock_for_test(|| {
            let allocation_guard = kspin::NoPreemptIrqSave::new();
            let allocated_before = kalloc::current_cpu_allocated_bytes_for_test(&allocation_guard);
            PENDING[cpu].store(head, Ordering::Release);
            while !SUBMITTED[cpu].load(Ordering::Acquire) {
                assert!(
                    khal::time::monotonic_time() < deadline,
                    "real NMI did not arrive under GIC lock"
                );
                core::hint::spin_loop();
            }
            assert_eq!(counters.completed.load(Ordering::Acquire), 0);
            assert_eq!(RECLAIMED[cpu].load(Ordering::Acquire), 1);
            assert_eq!(
                kalloc::current_cpu_allocated_bytes_for_test(&allocation_guard),
                allocated_before
            );
        }));
        irq_driver::gic::enable(of::pmu_irq_or(kbuild_config::PMU_IRQ), false);
        khal::quiesce_nmi();
        RETIREMENT[cpu].store(ptr::null_mut(), Ordering::Release);
        // SAFETY: SUBMITTED was acquired and the source is quiesced; the NMI
        // finished taking every owner, so only the empty fixture Box is freed.
        drop(unsafe { Box::from_raw(retirement) });
        wait_for(&counters, count);
        wait_for_reclamation(cpu, 3);
        worker.flush().unwrap();
        assert_eq!(WORKER_RUNS.load(Ordering::Acquire), before + 1);
        assert!(!FAILED.load(Ordering::Acquire));
        unittest::ktest_println!(
            "irq_work real NMI: cpu={cpu}, nodes={count}, source_alloc_bytes=0, \
             worker_handoffs=1, deferred_payloads=3"
        );
    }

    #[def_test(serial)]
    fn real_nmi_can_publish_while_gic_lock_is_held_on_each_cpu() {
        on_each_cpu(probe);
    }
}
