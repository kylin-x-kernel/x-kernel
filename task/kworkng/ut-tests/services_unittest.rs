// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kirq::softirq::test_support::{ScopedSoftirqAction, begin_softirq_test};
use kspin::{SpinNoIrq, static_lock};
use unittest::def_test;

use super::*;

fn budget() -> NonZeroUsize {
    NonZeroUsize::new(2).unwrap()
}

fn existing_action() {}

fn prepare() -> Services {
    Services::prepare(budget(), TimeSpan::from_millis(10), budget()).unwrap()
}

static_lock! {
    static TEST_SERVICES: SpinNoIrq<Option<Arc<Once<Services>>>> = SpinNoIrq::new(None);
}

pub(super) fn registry() -> Option<Arc<Once<Services>>> {
    TEST_SERVICES.lock().clone()
}

fn is_ready() -> bool {
    with_services(|services| services.is_some())
}

fn default_weak() -> alloc::sync::Weak<Queue> {
    with_services(|services| Arc::downgrade(&services.unwrap().defaults[0]))
}

fn initialize() -> Result<(), InitError> {
    crate::runtime::init(budget(), TimeSpan::from_millis(10), budget())
}

// Declared before action guards so handlers are restored before storage retires.
// These serial tests publish no queues, remote kicks or asynchronous producers.
struct ScopedServices;
impl ScopedServices {
    fn new() -> Self {
        assert!(SERVICES.get().is_none());
        let registry = Arc::new(Once::new());
        let mut slot = TEST_SERVICES.lock();
        assert!(slot.is_none());
        *slot = Some(registry);
        Self
    }
}
impl Drop for ScopedServices {
    fn drop(&mut self) {
        let retired = TEST_SERVICES.lock().take();
        if let Some(services) = retired.as_ref().and_then(|registry| registry.get()) {
            for queue in &services.defaults {
                queue.close_and_drain().unwrap();
            }
            services.normal.stop().unwrap();
        }
        drop(retired);
    }
}

// Field drop order restores actions before joining the unpublished services,
// then re-enables daemon wakeups. All users are serial kernel tests.
pub(crate) struct ScopedInstalledServices {
    _normal: ScopedSoftirqAction,
    _high: ScopedSoftirqAction,
    _storage: ScopedServices,
    _wake_gate: kirq::softirq::test_support::ScopedDaemonWakeGate,
}

pub(crate) fn install_for_test() -> ScopedInstalledServices {
    let wake_gate = begin_softirq_test();
    let storage = ScopedServices::new();
    let normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let high = ScopedSoftirqAction::clear(SoftirqVec::High);
    initialize().unwrap();
    ScopedInstalledServices {
        _normal: normal,
        _high: high,
        _storage: storage,
        _wake_gate: wake_gate,
    }
}

/// Both real softirq entries see complete services; repeat init cannot replace them.
///
/// # Panics
/// Panics on partial publication, wrong handler, invalid execution or replacement.
#[def_test(serial)]
fn bh_registration_publishes_both_entries_and_rejects_replacement() {
    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);
    initialize().unwrap();
    let original = default_weak();
    let candidate = prepare();
    let rejected = Arc::downgrade(&candidate.defaults[0]);
    assert_eq!(install(candidate), Err(InitError::AlreadyInitialized));
    assert!(rejected.upgrade().is_none());
    assert_eq!(
        initialize(),
        Err(crate::runtime::InitError::AlreadyInitialized)
    );
    assert!(alloc::sync::Weak::ptr_eq(&original, &default_weak()));
    assert!(softirq::softirq_action_matches_for_tests(
        SoftirqVec::Tasklet,
        run_normal
    ));
    assert!(softirq::softirq_action_matches_for_tests(
        SoftirqVec::High,
        run_high
    ));
    let _irq = kspin::NoPreemptIrqSave::new();
    softirq::raise_softirq(SoftirqVec::Tasklet);
    softirq::raise_softirq(SoftirqVec::High);
    assert_eq!(
        softirq::run_pending_softirqs(),
        softirq::SoftirqRunResult::Ran
    );
    assert_eq!(softirq::local_softirq_pending(), 0);
}

/// Conflict at either entry rolls back storage and leaves both action slots unchanged.
///
/// # Panics
/// Panics on partial installation, retained failed storage or overwriting an owner.
#[def_test(serial)]
fn bh_registration_conflict_releases_preparation_without_overwriting_actions() {
    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);
    for (occupied, vacant) in [
        (SoftirqVec::Tasklet, SoftirqVec::High),
        (SoftirqVec::High, SoftirqVec::Tasklet),
    ] {
        let _existing = ScopedSoftirqAction::install(occupied, existing_action);
        let services = prepare();
        let weak = Arc::downgrade(&services.defaults[0]);
        assert_eq!(install(services), Err(InitError::ActionsOccupied));
        assert!(weak.upgrade().is_none());
        assert!(!is_ready());
        assert!(softirq::softirq_action_matches_for_tests(
            occupied,
            existing_action
        ));
        assert!(!softirq::is_softirq_open(vacant));
    }
    initialize().unwrap();
    assert!(is_ready());
}

/// Default lookup borrows the published queue instead of retaining another Arc.
/// # Panics
/// Panics if lookup changes ownership, selects another queue or loses readiness.
#[def_test(serial)]
fn default_lookup_does_not_clone_queue() {
    let _services = install_for_test();
    with_services(|services| {
        let services = services.unwrap();
        for (index, kind) in [
            crate::DefaultQueue::Normal,
            crate::DefaultQueue::BottomHalf,
            crate::DefaultQueue::HighBottomHalf,
        ]
        .into_iter()
        .enumerate()
        {
            let expected = &services.defaults[index];
            let count = Arc::strong_count(expected);
            with_default_queue(kind, |queue| {
                let queue = queue.unwrap();
                assert!(core::ptr::eq(queue, expected));
                assert_eq!(Arc::strong_count(queue), count);
            });
        }
    });
}

/// Invalid preparation context rejects before publishing services or IRQ actions.
///
/// # Panics
/// Panics on accepting IRQ-masked initialization or any publication side effect.
#[def_test(serial)]
fn bh_registration_rejects_invalid_context_before_publication() {
    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);
    let _irq = kspin::NoPreemptIrqSave::new();
    assert_eq!(
        initialize(),
        Err(InitError::BottomHalf(
            BottomHalfPrepareError::InvalidContext
        ))
    );
    assert!(!is_ready());
    assert!(!softirq::is_softirq_open(SoftirqVec::Tasklet));
    assert!(!softirq::is_softirq_open(SoftirqVec::High));
}

/// Normal preparation errors leave the registry and both BH actions untouched.
///
/// # Panics
/// Panics if an invalid idle interval publishes any execution provider.
#[def_test(serial)]
fn registration_rejects_invalid_normal_configuration() {
    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);
    assert_eq!(
        crate::runtime::init(budget(), TimeSpan::ZERO, budget()),
        Err(InitError::Normal(NormalRuntimeError::InvalidIdleInterval))
    );
    assert!(!is_ready());
    assert!(!softirq::is_softirq_open(SoftirqVec::Tasklet));
    assert!(!softirq::is_softirq_open(SoftirqVec::High));
}

/// Registered providers execute real Work on each requested CPU and category.
///
/// # Panics
/// Panics on wrong placement/context, lost work or incomplete queue detachment.
#[def_test(serial)]
fn registered_services_bind_and_drain_real_queues() {
    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);
    initialize().unwrap();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
        for kind in [
            ExecutionKind::Normal,
            ExecutionKind::BottomHalf(BottomHalfPriority::Normal),
            ExecutionKind::BottomHalf(BottomHalfPriority::High),
        ] {
            let config = QueueConfig::new(&[cpu], kworkqueue_ng::CpuPolicy::CurrentCpu, 1).unwrap();
            let queue = prepare_queue("bound-test", config, kind).unwrap();
            let weak = Arc::downgrade(&queue);
            let calls = Arc::new(core::sync::atomic::AtomicUsize::new(0));
            let observed = calls.clone();
            let work = core::pin::pin!(crate::Work::with_callback(move || {
                assert_eq!(khal::percpu::this_cpu_id(), cpu);
                assert_eq!(
                    ktask::is_sleepable_context(),
                    matches!(kind, ExecutionKind::Normal)
                );
                observed.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }));
            // SAFETY: this sole producer retains pinned storage through queue
            // drain; the callback above satisfies its selected execution context.
            unsafe { queue.queue_work(work.as_ref(), Some(cpu)) }.unwrap();
            {
                let _irq = kspin::NoPreemptIrqSave::new();
                softirq::run_pending_softirqs();
            }
            queue.close_and_drain().unwrap();
            assert_eq!(calls.load(core::sync::atomic::Ordering::Relaxed), 1);
            drop(queue);
            assert!(weak.upgrade().is_none());
        }
    });
}

/// Queue preparation rejects unavailable providers or forbidden execution context.
///
/// # Panics
/// Panics if rejection creates a queue or service side effect.
#[def_test(serial)]
fn queue_binding_rejects_before_construction() {
    let _storage = ScopedServices::new();
    let config = || {
        QueueConfig::new(
            &[khal::percpu::this_cpu_id()],
            kworkqueue_ng::CpuPolicy::CurrentCpu,
            1,
        )
        .unwrap()
    };
    assert!(matches!(
        crate::create_queue("unavailable", config(), ExecutionKind::Normal),
        Err(QueueCreateError::ServicesUnavailable)
    ));
    let prepared = config();
    let _irq = kspin::NoPreemptIrqSave::new();
    assert!(matches!(
        crate::create_queue("invalid-context", prepared, ExecutionKind::Normal),
        Err(QueueCreateError::InvalidContext)
    ));
    assert!(!is_ready());
}

/// Caller-facing objects use the same real normal/BH providers without resource tokens.
/// # Panics
/// Panics on wrong admission, callback count, delayed modification or drain behavior.
#[def_test(serial)]
fn public_work_operations_use_bound_services() {
    use core::{
        pin::pin,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::{DelayedWork, SubmitError, Work};

    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);
    initialize().unwrap();
    let cpu = khal::percpu::this_cpu_id();
    for kind in [
        ExecutionKind::Normal,
        ExecutionKind::BottomHalf(BottomHalfPriority::Normal),
        ExecutionKind::BottomHalf(BottomHalfPriority::High),
    ] {
        let config = QueueConfig::new(&[cpu], crate::CpuPolicy::CurrentCpu, 1).unwrap();
        let queue = prepare_queue("public-api", config, kind).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let work = pin!(Work::with_callback(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }));
        let (disabled, revoked) = work.disable();
        assert!(!revoked);
        assert_eq!(
            // SAFETY: pinned Work and captures remain retained until final synchronous
            // cancellation; the callback is nonblocking in every execution category.
            unsafe { queue.queue_work(work.as_ref(), Some(cpu)) },
            Err(SubmitError::Disabled)
        );
        drop(disabled);
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            assert_eq!(
                // SAFETY: same retained storage and initialized bound providers;
                // this exercises submission with IRQs masked, not a sleepable shortcut.
                unsafe { queue.queue_work(work.as_ref(), Some(cpu)) }.unwrap(),
                cpu
            );
            softirq::run_pending_softirqs();
        }
        work.flush().unwrap();
        work.cancel_sync().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let observed = calls.clone();
        let delayed = pin!(DelayedWork::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }));
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            // SAFETY: the pinned pair and captures outlive both the timer and
            // ordinary drains below; the timer service was initialized at boot.
            unsafe {
                queue
                    .queue_delayed_work(delayed.as_ref(), TimeSpan::from_secs(3600), Some(cpu))
                    .unwrap();
                queue
                    .modify_delayed_work(delayed.as_ref(), TimeSpan::ZERO, Some(cpu))
                    .unwrap();
            }
            softirq::run_pending_softirqs();
        }
        delayed.flush().unwrap();
        delayed.cancel_sync().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        queue.close_and_drain().unwrap();
    }
}

/// The watchdog skips unpublished services and reports real published backlog.
/// # Panics
/// Panics on missing/wrong reports, premature execution or incomplete cancellation.
#[cfg(feature = "stall-check")]
#[def_test(serial)]
fn watchdog_observes_only_published_services() {
    {
        let _storage = ScopedServices::new();
        let cpu = khal::percpu::this_cpu_id();
        let now = khal::time::monotonic_time();
        watchdog::WorkqueueStallCheck::inspect(cpu, now, TimeSpan::from_secs(20));
        assert!(!inspect_stalls(
            cpu,
            now,
            TimeSpan::from_secs(20),
            |_, _, _| panic!("unavailable service")
        ));
    }
    let _services = install_for_test();
    let cpu = khal::percpu::this_cpu_id();
    let threshold = TimeSpan::from_secs(20);
    // An unserviced source retains real backlog without blocking a worker or
    // disabling IRQs/preemption while inspecting the sleepable directory.
    let queue = with_services(|services| {
        service::bind_queue_with_diagnostics(
            QueueConfig::new(&[cpu], CpuPolicy::CurrentCpu, 1).unwrap(),
            "diagnostic-backlog",
            &services.unwrap().diagnostics,
            |_| {
                (
                    kworkerpool_ng::PartitionDispatch::new(),
                    Arc::new(core::task::Waker::noop().clone()),
                )
            },
        )
    });
    let work = core::pin::pin!(crate::Work::with_callback(|| panic!("unserviced work ran")));
    // SAFETY: the pinned work remains retained through cancellation and queue
    // drain below; no execution provider claims this diagnostic-only source.
    unsafe { queue.queue_work(work.as_ref(), Some(cpu)) }.unwrap();
    let now = khal::time::monotonic_time() + threshold;
    let mut reports = 0;
    assert!(inspect_stalls(
        cpu,
        now,
        threshold,
        |name, actual_cpu, elapsed| {
            assert_eq!(name, "diagnostic-backlog");
            assert_eq!(actual_cpu, cpu);
            assert!(elapsed >= threshold);
            reports += 1;
        }
    ));
    assert_eq!(reports, 1);
    assert!(work.cancel_pending());
    assert!(inspect_stalls(
        cpu,
        now + threshold,
        threshold,
        |_, _, _| panic!("canceled backlog reported")
    ));
    queue.close_and_drain().unwrap();
}

/// Installed actions can run before publication without exposing a candidate.
/// # Panics
/// Panics on premature readiness, spinning dispatch, or lost post-publication work.
#[def_test(serial)]
fn bh_dispatch_before_publication_is_nonblocking_and_unready() {
    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);
    let candidate = prepare();
    let calls = core::sync::atomic::AtomicUsize::new(0);
    let work = core::pin::pin!(crate::Work::with_callback(|| {
        calls.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }));
    with_registry(|registry| {
        install_with_actions(registry, candidate, || {
            assert!(softirq::open_softirqs(&[
                (SoftirqVec::Tasklet, run_normal),
                (SoftirqVec::High, run_high),
            ]));
            assert!(!is_ready());
            let _irq = kspin::NoPreemptIrqSave::new();
            assert_eq!(
                // SAFETY: the pinned Work remains live through final cancellation below;
                // unready admission must not publish it.
                unsafe { crate::DefaultQueue::BottomHalf.queue_work(work.as_ref(), None) },
                Err(crate::ServiceError::NotReady)
            );
            softirq::raise_softirq(SoftirqVec::Tasklet);
            softirq::raise_softirq(SoftirqVec::High);
            assert_eq!(
                softirq::run_pending_softirqs(),
                softirq::SoftirqRunResult::Ran
            );
            assert_eq!(softirq::local_softirq_pending(), 0);
            true
        })
    })
    .unwrap();
    assert!(is_ready());
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        // SAFETY: now published providers execute a nonblocking callback; this
        // pinned Work and its capture remain live through the final drain.
        unsafe { crate::DefaultQueue::BottomHalf.queue_work(work.as_ref(), None) }.unwrap();
        softirq::run_pending_softirqs();
    }
    work.cancel_sync().unwrap();
    assert_eq!(calls.load(core::sync::atomic::Ordering::Relaxed), 1);
}

/// Readiness rejection never queues Work or programs delayed timers.
/// # Panics
/// Panics on acceptance before readiness or any retained occurrence after failure.
#[def_test(serial)]
fn defaults_reject_before_readiness_and_after_failed_init() {
    use core::pin::pin;

    use crate::{DefaultQueue, DelayedWork, ServiceError, Work};
    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _occupied = ScopedSoftirqAction::install(SoftirqVec::Tasklet, existing_action);
    let work = pin!(Work::with_callback(|| panic!("unready work ran")));
    let delayed = pin!(DelayedWork::new(|| panic!("unready delayed work ran")));
    for failed_init in [false, true] {
        if failed_init {
            assert_eq!(initialize(), Err(InitError::ActionsOccupied));
        }
        assert!(matches!(
            crate::WorkHandle::new(|| {}),
            Err(crate::HandleCreateError::ServicesUnavailable)
        ));
        assert!(matches!(
            crate::DelayedWorkHandle::new(|| {}),
            Err(crate::HandleCreateError::ServicesUnavailable)
        ));
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            assert!(matches!(
                crate::WorkHandle::new(|| {}),
                Err(crate::HandleCreateError::InvalidContext)
            ));
            assert!(matches!(
                crate::DelayedWorkHandle::new(|| {}),
                Err(crate::HandleCreateError::InvalidContext)
            ));
        }
        for queue in [
            DefaultQueue::Normal,
            DefaultQueue::BottomHalf,
            DefaultQueue::HighBottomHalf,
        ] {
            // SAFETY: storage remains pinned through final cancellation, with no
            // other producer; rejected admission must publish no borrowed address.
            unsafe {
                assert_eq!(
                    queue.queue_work(work.as_ref(), None),
                    Err(ServiceError::NotReady)
                );
                assert_eq!(
                    queue.queue_delayed_work(delayed.as_ref(), TimeSpan::ZERO, None),
                    Err(ServiceError::NotReady)
                );
                assert_eq!(
                    queue.modify_delayed_work(delayed.as_ref(), TimeSpan::MAX, None),
                    Err(ServiceError::NotReady)
                );
            }
        }
        assert!(!work.cancel_sync().unwrap());
        assert!(!delayed.cancel_sync().unwrap());
        assert!(!is_ready());
    }
}

/// Invalid limits cannot prepare services; valid defaults cover exactly boot CPUs.
/// # Panics
/// Panics on zero acceptance, mismatched coverage/policy/limits or failed cleanup.
#[def_test(serial)]
fn defaults_validate_limits_and_boot_cpu_coverage() {
    let _storage = ScopedServices::new();
    for (normal, bh) in [(0, 1), (1, 0)] {
        assert!(matches!(
            Services::prepare_with_limits(
                budget(),
                TimeSpan::from_millis(10),
                budget(),
                normal,
                bh
            ),
            Err(InitError::Configuration(QueueConfigError::ZeroMaxActive))
        ));
    }
    let services =
        Services::prepare_with_limits(budget(), TimeSpan::from_millis(10), budget(), 2, 3).unwrap();
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    cpus.sort_unstable();
    for (queue, max_active) in services.defaults.iter().zip([2, 3, 3]) {
        assert_eq!(queue.config().cpus(), cpus.as_slice());
        assert_eq!(queue.config().policy(), CpuPolicy::CurrentCpu);
        assert_eq!(queue.config().max_active(), max_active);
        queue.close_and_drain().unwrap();
    }
    services.normal.stop().unwrap();
}

/// Stopping one user's Work does not close the shared default service.
/// # Panics
/// Panics on wrong placement, lost callbacks, delayed modification or drain failure.
#[def_test(serial)]
fn ready_defaults_execute_on_every_boot_cpu_and_remain_shared() {
    use core::{
        pin::pin,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::{DefaultQueue, DelayedWork, Work};
    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);
    initialize().unwrap();
    for queue in [
        DefaultQueue::Normal,
        DefaultQueue::BottomHalf,
        DefaultQueue::HighBottomHalf,
    ] {
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
            let calls = AtomicUsize::new(0);
            let work = pin!(Work::with_callback(|| {
                assert_eq!(khal::percpu::this_cpu_id(), cpu);
                calls.fetch_add(1, Ordering::Relaxed);
            }));
            {
                let _irq = kspin::NoPreemptIrqSave::new();
                assert_eq!(
                    // SAFETY: pinned Work and borrowed captures outlive joined completion;
                    // all services are installed and this callback never sleeps.
                    unsafe { queue.queue_work(work.as_ref(), Some(cpu)) }.unwrap(),
                    cpu
                );
                softirq::run_pending_softirqs();
            }
            work.flush().unwrap();
            work.cancel_sync().unwrap();
            let delayed = pin!(DelayedWork::new(|| {
                calls.fetch_add(1, Ordering::Relaxed);
            }));
            {
                let _irq = kspin::NoPreemptIrqSave::new();
                let current_cpu = khal::percpu::this_cpu_id();
                // SAFETY: this independent caller retains the pinned pair/captures
                // through final drain; implicit routing is sampled while CPU-pinned.
                unsafe {
                    assert_eq!(
                        queue
                            .queue_delayed_work(delayed.as_ref(), TimeSpan::from_secs(3600), None)
                            .unwrap(),
                        current_cpu
                    );
                    assert_eq!(
                        queue
                            .modify_delayed_work(delayed.as_ref(), TimeSpan::ZERO, None)
                            .unwrap(),
                        current_cpu
                    );
                }
                softirq::run_pending_softirqs();
            }
            delayed.flush().unwrap();
            delayed.cancel_sync().unwrap();
            assert_eq!(calls.load(Ordering::Relaxed), 2);
        });
    }
}

/// Independent queue shutdown preserves peer/default services in every category.
/// # Panics
/// Panics on wrong scope, lost callbacks, acceptance after closure or failed drain.
#[def_test(serial)]
fn owned_queue_flush_and_shutdown_preserve_shared_services() {
    use core::{
        pin::pin,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::{DefaultQueue, DelayedWork, SubmitError, Work, create_queue};
    let _wake_gate = begin_softirq_test();
    let _storage = ScopedServices::new();
    let _normal = ScopedSoftirqAction::clear(SoftirqVec::Tasklet);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);
    initialize().unwrap();
    let cpu = khal::percpu::this_cpu_id();
    for (kind, default) in [
        (ExecutionKind::Normal, DefaultQueue::Normal),
        (
            ExecutionKind::BottomHalf(BottomHalfPriority::Normal),
            DefaultQueue::BottomHalf,
        ),
        (
            ExecutionKind::BottomHalf(BottomHalfPriority::High),
            DefaultQueue::HighBottomHalf,
        ),
    ] {
        let config = || QueueConfig::new(&[cpu], CpuPolicy::CurrentCpu, 1).unwrap();
        let queue = create_queue("owned", config(), kind).unwrap();
        let peer = create_queue("peer", config(), kind).unwrap();
        let weak = Arc::downgrade(&queue);
        let calls = AtomicUsize::new(0);
        let work = pin!(Work::with_callback(|| {
            calls.fetch_add(1, Ordering::Relaxed);
        }));
        let delayed = pin!(DelayedWork::new(|| panic!("canceled timer ran")));
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            // SAFETY: both pinned objects/captures outlive the ordinary and physical
            // drains below. Providers are initialized and callbacks cannot sleep.
            unsafe {
                queue.queue_work(work.as_ref(), Some(cpu)).unwrap();
                queue
                    .queue_delayed_work(delayed.as_ref(), TimeSpan::from_secs(3600), Some(cpu))
                    .unwrap();
            }
            softirq::run_pending_softirqs();
        }
        queue.flush().unwrap(); // Does not wait for the timer-only occurrence.
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(delayed.cancel_sync().unwrap());
        work.cancel_sync().unwrap();
        queue.close_and_drain().unwrap();
        queue.close_and_drain().unwrap();
        assert_eq!(
            // SAFETY: rejected submission still uses retained pinned storage.
            unsafe { queue.queue_work(work.as_ref(), Some(cpu)) },
            Err(SubmitError::Closed)
        );
        drop(queue);
        assert!(weak.upgrade().is_none());
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            // SAFETY: the old occurrence was drained; storage remains pinned and
            // retained through the independent peer's flush and final cancellation.
            unsafe { peer.queue_work(work.as_ref(), Some(cpu)) }.unwrap();
            softirq::run_pending_softirqs();
        }
        peer.flush().unwrap();
        work.cancel_sync().unwrap();
        peer.close_and_drain().unwrap();
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            // SAFETY: the same retained Work is idle after the peer's joined drain;
            // closing either private queue must not stop shared default execution.
            unsafe { default.queue_work(work.as_ref(), Some(cpu)) }.unwrap();
            softirq::run_pending_softirqs();
        }
        work.flush().unwrap();
        work.cancel_sync().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }
}
