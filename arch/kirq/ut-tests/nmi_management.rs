// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::{Handler, IRQ_STATE, IrqDesc, register_nmi, register_nmi_startup, unregister_nmi};
use crate::{
    IrqActionToken, IrqDescError, IrqEvent, IrqRef,
    action::IrqAction,
    runtime::rcu::{with_nmi_claim, with_nmi_context},
    state::IrqLine,
};

struct Fixture {
    desc: IrqDesc,
    line: Arc<IrqLine>,
}

impl Fixture {
    fn new(virq: usize) -> Self {
        Self::mapped(IrqDesc::from_virq(virq))
    }

    fn mapped(desc: IrqDesc) -> Self {
        let virq = crate::try_map(desc).unwrap();
        let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
        Self { desc, line }
    }

    fn install(&self, handler: Handler) -> bool {
        super::register_nmi_with_mode(self.desc, handler, super::ManagementMode::Task, |_| Ok(()))
    }

    fn remove(&self) -> bool {
        super::unregister_nmi_with(self.desc, |_| {})
    }

    fn has_owner(&self) -> bool {
        krcu::with_read(|scope| self.line.nmi_action.read(scope).is_some())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Synthetic keys have no hardware producer. Tests finish every scoped
        // borrow before fixture cleanup; no asynchronous readers use these keys.
        let removed = IRQ_STATE
            .lock()
            .remove_test_line(self.desc.logical_irq().unwrap());
        drop(removed);
    }
}

static LOCAL_NMI_CHIP: crate::PerCpuNmiChip = crate::PerCpuNmiChip {
    prepare: |_| Ok(()),
    teardown: |_| Ok(()),
};

/// Synthetic NMI source for real task/IPI lifecycle tests; no hardware access.
pub struct NmiShutdownProbe(Fixture);

impl NmiShutdownProbe {
    /// Publishes a test-owned NMI action on an otherwise unused logical IRQ.
    pub fn new(virq: usize, handler: Handler) -> Self {
        let fixture = Fixture::new(virq);
        core::assert!(fixture.install(handler));
        fixture.line.lock().flow = crate::IrqFlow::PerCpu;
        Self(fixture)
    }

    /// Prepares/enables only the calling CPU's synthetic local slot.
    pub fn prepare_local(&self) -> bool {
        let _pin = kspin::NoPreemptIrqSave::new();
        let mut entry = self.0.line.lock();
        if entry.is_nmi_closing() || entry.prepare_local_nmi(&LOCAL_NMI_CHIP).is_err() {
            return false;
        }
        let plan = entry.prepare_legacy_enable();
        entry.commit_platform_plan(plan);
        true
    }

    /// Disables locally and optionally models a failed controller acknowledgement.
    pub fn stop_local(&self, has_teardown_ack: bool) -> bool {
        let _pin = kspin::NoPreemptIrqSave::new();
        let mut entry = self.0.line.lock();
        if entry.can_run_local_actions() {
            let plan = entry.prepare_disable_irq_nosync();
            entry.commit_platform_plan(plan);
        }
        has_teardown_ack && entry.teardown_local_nmi().is_ok()
    }

    /// Starts the real retained-Line shutdown transaction.
    pub fn begin(&self) -> super::NmiShutdown {
        super::begin_nmi_shutdown(self.0.desc).unwrap()
    }

    /// Attempts ordinary removal, which must reject an outstanding claim.
    pub fn remove_without_claim(&self) -> bool {
        self.0.remove()
    }

    /// Consumes the real shutdown claim without touching a physical controller.
    pub fn finish(&self, shutdown: super::NmiShutdown) -> Result<(), super::NmiShutdown> {
        core::assert_eq!(
            shutdown.descriptor().logical_irq(),
            self.0.desc.logical_irq()
        );
        shutdown.finish_with(|_| {})
    }
}

#[def_test(serial)]
fn shutdown_claim_retains_action_until_unique_owner_finishes() {
    let fixture = Fixture::new(0x5fb7);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(fixture.install(handler.clone()));
    fixture.line.lock().flow = crate::IrqFlow::PerCpu;
    let shutdown = super::begin_nmi_shutdown(fixture.desc).unwrap();
    assert!((0..kbuild_config::NR_CPUS).all(|cpu| !shutdown.is_participant(cpu)));
    assert!(!shutdown.is_participant(usize::MAX));
    assert!(matches!(
        super::begin_nmi_shutdown(fixture.desc),
        Err(IrqDescError::TeardownInProgress { .. })
    ));
    assert!(!fixture.remove());
    assert!(matches!(
        crate::try_enable(fixture.desc, true),
        Err(IrqDescError::TeardownInProgress { .. })
    ));
    let shutdown = Arc::new(shutdown);
    let outstanding = shutdown.clone();
    let shutdown = Arc::try_unwrap(shutdown).err().unwrap();
    assert!(fixture.has_owner());
    drop(outstanding);
    let shutdown = Arc::try_unwrap(shutdown).ok().unwrap();
    assert!(shutdown.finish_with(|_| {}).is_ok());
    assert_eq!(Arc::strong_count(&handler), 1);
    assert!(!fixture.line.lock().is_nmi_closing());
    assert!(fixture.install(handler.clone()));
    assert!(fixture.remove());
}

#[def_test(serial)]
fn abandoned_shutdown_keeps_admission_closed_and_allows_retry() {
    let fixture = Fixture::new(0x5fb8);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(fixture.install(handler.clone()));
    fixture.line.lock().flow = crate::IrqFlow::PerCpu;
    let shutdown = super::begin_nmi_shutdown(fixture.desc).unwrap();
    drop(shutdown);
    assert!(fixture.line.lock().is_nmi_closing());
    assert!(fixture.has_owner());
    assert_eq!(Arc::strong_count(&handler), 2);
    let shutdown = super::begin_nmi_shutdown(fixture.desc).unwrap();
    // Context rejection must return the same claim without withdrawing its owner.
    let irq = kspin::IrqSave::new();
    let shutdown = shutdown.finish_with(|_| {}).err().unwrap();
    drop(irq);
    assert!(fixture.line.lock().has_nmi_shutdown());
    assert!(fixture.has_owner());
    assert!(shutdown.finish_with(|_| {}).is_ok());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn shutdown_snapshots_prepared_cpu_and_rejects_incomplete_teardown() {
    let fixture = Fixture::new(0x5fb9);
    assert!(fixture.install(Arc::new(|_| IrqEvent::HANDLED)));
    let pin = kspin::NoPreemptIrqSave::new();
    let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize();
    {
        let mut entry = fixture.line.lock();
        entry.flow = crate::IrqFlow::PerCpu;
        assert!(entry.prepare_local_nmi(&LOCAL_NMI_CHIP).is_ok());
    }
    drop(pin);
    let shutdown = super::begin_nmi_shutdown(fixture.desc).unwrap();
    for candidate in 0..kbuild_config::NR_CPUS {
        assert_eq!(shutdown.is_participant(candidate), candidate == cpu);
    }
    let shutdown = shutdown.finish_with(|_| {}).err().unwrap();
    assert!(fixture.has_owner());
    assert!(fixture.line.lock().has_local_nmi_preparation());
    assert!(fixture.line.lock().has_nmi_shutdown());
    drop(shutdown);
    // Fixture destruction removes only a synthetic source, without hardware or
    // asynchronous readers. It need not execute teardown on the prepared CPU.
}

static HAS_TEARDOWN_FAILURE: AtomicBool = AtomicBool::new(false);
static RETRY_NMI_CHIP: crate::PerCpuNmiChip = crate::PerCpuNmiChip {
    prepare: |_| Ok(()),
    teardown: |desc| {
        if HAS_TEARDOWN_FAILURE.load(Ordering::Relaxed) {
            Err(IrqDescError::ConfigurationFailed { hwirq: desc.hwirq })
        } else {
            Ok(())
        }
    },
};

#[def_test(serial)]
fn failed_local_teardown_preserves_owner_until_retry_and_retirement() {
    let fixture = Fixture::new(0x5fb6);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(fixture.install(handler.clone()));
    // Keep both backend attempts on the prepared CPU; sleeping retirement
    // follows only after the local transaction has released this guard.
    let pin = kspin::NoPreemptIrqSave::new();
    {
        let mut entry = fixture.line.lock();
        entry.flow = crate::IrqFlow::PerCpu;
        assert!(entry.prepare_local_nmi(&RETRY_NMI_CHIP).is_ok());
        entry.begin_nmi_close();
    }
    HAS_TEARDOWN_FAILURE.store(true, Ordering::Relaxed);
    // SAFETY: no hardware producer or claim exists for this synthetic key.
    // The backend models an incomplete local controller teardown.
    let failed = unsafe { super::teardown_percpu_nmi(fixture.desc) };
    HAS_TEARDOWN_FAILURE.store(false, Ordering::Relaxed);
    assert!(failed.is_err());
    assert!(fixture.line.lock().has_local_nmi_preparation());
    assert!(fixture.has_owner());
    assert_eq!(Arc::strong_count(&handler), 2);
    assert_eq!(fixture.line.lock().action_count(), 1);
    assert!(matches!(
        crate::try_enable(fixture.desc, true),
        Err(IrqDescError::TeardownInProgress { .. })
    ));

    // SAFETY: the same synthetic source remains stopped; retry now confirms
    // completion on the original prepared CPU and Line.
    let completed = unsafe { super::teardown_percpu_nmi(fixture.desc) };
    assert!(completed.is_ok());
    assert!(!fixture.line.lock().has_local_nmi_preparation());
    drop(pin);
    assert!(fixture.remove());
    assert_eq!(Arc::strong_count(&handler), 1);
    assert!(!fixture.has_owner());
    assert!(!fixture.line.lock().is_nmi_closing());
    assert!(fixture.install(handler.clone()));
    assert!(fixture.remove());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn closing_nmi_requires_local_teardown_before_owner_withdrawal() {
    let fixture = Fixture::new(0x5fb5);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(fixture.install(handler.clone()));
    fixture.line.lock().flow = crate::IrqFlow::PerCpu;
    assert!(
        fixture
            .line
            .lock()
            .prepare_local_nmi(&LOCAL_NMI_CHIP)
            .is_ok()
    );
    assert!(!fixture.remove());
    assert_eq!(Arc::strong_count(&handler), 2);
    assert!(matches!(
        super::prepare_percpu_nmi(fixture.desc),
        Err(IrqDescError::TeardownInProgress { .. })
    ));
    // SAFETY: the synthetic source has no producer or outstanding hardware
    // claims; its stateless backend acknowledges only this fixture's teardown.
    let torn_down = unsafe { super::teardown_percpu_nmi(fixture.desc) };
    assert!(torn_down.is_ok());
    assert!(!fixture.line.lock().has_local_nmi_preparation());
    assert!(fixture.remove());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn incomplete_local_nmi_close_retains_owner_and_rejects_reenable() {
    let fixture = Fixture::new(0x5fb1);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(fixture.install(handler.clone()));
    {
        let mut entry = fixture.line.lock();
        entry.flow = crate::IrqFlow::PerCpu;
        let plan = entry.prepare_legacy_enable();
        entry.commit_platform_plan(plan);
    }
    let mut has_masked = false;
    assert!(!super::unregister_nmi_with(fixture.desc, |_| has_masked = true));
    assert!(!has_masked);
    assert!(fixture.line.lock().is_nmi_closing());
    assert_eq!(fixture.line.lock().action_count(), 1);
    assert!(fixture.has_owner());
    assert_eq!(Arc::strong_count(&handler), 2);
    assert!(matches!(
        crate::try_enable(fixture.desc, true),
        Err(IrqDescError::TeardownInProgress { .. })
    ));
    assert!(matches!(
        crate::try_enable_irq(fixture.desc),
        Err(IrqDescError::TeardownInProgress { .. })
    ));
    {
        let mut entry = fixture.line.lock();
        let plan = entry.prepare_disable_irq_nosync();
        assert_eq!(plan.enable, Some(false));
        entry.commit_platform_plan(plan);
    }
    assert!(super::unregister_nmi_with(fixture.desc, |_| has_masked = true));
    assert!(has_masked);
    assert!(!fixture.line.lock().is_nmi_closing());
    assert_eq!(fixture.line.lock().action_count(), 0);
    assert!(!fixture.has_owner());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn startup_nmi_registration_reuses_prepared_mapping() {
    let desc = IrqDesc::from_hwirq(0x5f7b)
        .with_virq(crate::DYNAMIC_VIRQ_BASE + 0x5f7b)
        .with_domain(crate::domain::TEST_DOMAIN);
    let fixture = Fixture::mapped(desc);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    {
        let _irq = kspin::IrqSave::new();
        assert!(super::register_nmi_with_mode(
            desc,
            handler.clone(),
            super::ManagementMode::Startup,
            |_| Ok(()),
        ));
        assert!(!karch::local_irq_enabled());
    }
    let installed = IRQ_STATE
        .lock()
        .descs
        .get(&desc.logical_irq().unwrap())
        .unwrap();
    assert!(Arc::ptr_eq(&fixture.line, &installed));
    assert_eq!(installed.lock().action_count(), 1);
    assert!(fixture.has_owner());
    assert_eq!(Arc::strong_count(&handler), 2);
    assert!(fixture.remove());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn task_and_startup_nmi_reject_missing_mapping_before_configuration() {
    let desc = IrqDesc::from_virq(0x5fac);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    for mode in [super::ManagementMode::Task, super::ManagementMode::Startup] {
        let mut has_configured = false;
        assert!(!super::register_nmi_with_mode(
            desc,
            handler.clone(),
            mode,
            |_| {
                has_configured = true;
                Ok(())
            }
        ));
        assert!(!has_configured);
        assert!(crate::descriptor(desc.hwirq).is_none());
        assert_eq!(Arc::strong_count(&handler), 1);
    }
}

#[def_test(serial)]
fn failed_nmi_configuration_publishes_no_owner_and_preserves_irq_mask() {
    let fixture = Fixture::new(0x5f7a);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    for is_masked in [false, true] {
        let irq_guard = is_masked.then(kspin::IrqSave::new);
        let expected_irq = karch::local_irq_enabled();
        let mode = if is_masked {
            super::ManagementMode::Startup
        } else {
            super::ManagementMode::Task
        };
        let mut observed = None;
        assert!(!super::register_nmi_with_mode(
            fixture.desc,
            handler.clone(),
            mode,
            |_| {
                observed = Some((Arc::strong_count(&handler), karch::local_irq_enabled()));
                Err(IrqDescError::ConfigurationFailed {
                    hwirq: fixture.desc.hwirq,
                })
            }
        ));
        assert_eq!(observed, Some((2, false)));
        assert_eq!(karch::local_irq_enabled(), expected_irq);
        drop(irq_guard);
        assert!(!fixture.has_owner());
        assert_eq!(fixture.line.lock().action_count(), 0);
        assert_eq!(fixture.line.lock().configured_generation_for_tests(), None);
        assert_eq!(Arc::strong_count(&handler), 1);
    }
    assert!(fixture.install(handler.clone()));
    assert_eq!(Arc::strong_count(&handler), 2);
    assert!(!fixture.line.lock().prepare_reconfigure_if_stale().configure);
    assert!(fixture.remove());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn nmi_controller_updates_hold_original_line_exclusion() -> unittest::TestResult {
    let fixture = Fixture::new(0x5f49);
    let has_atomic_exclusion = {
        let _guard = fixture.line.lock();
        fixture.line.is_state_locked_for_tests()
    };
    if !has_atomic_exclusion {
        return unittest::TestResult::Ignored;
    }
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let mut has_configured = false;
    assert!(super::register_nmi_with_mode(
        fixture.desc,
        handler.clone(),
        super::ManagementMode::Task,
        |stored| {
            has_configured = !karch::local_irq_enabled()
                && fixture.line.is_state_locked_for_tests()
                && Arc::strong_count(&handler) == 2
                && stored.flags.contains(crate::IrqFlags::PER_CPU);
            Ok(())
        },
    ));
    let mut has_masked = false;
    assert!(super::unregister_nmi_with(fixture.desc, |stored| {
        has_masked = !karch::local_irq_enabled()
            && fixture.line.is_state_locked_for_tests()
            && stored.logical_irq() == fixture.desc.logical_irq()
            && Arc::strong_count(&handler) == 2;
    }));
    assert!(has_configured && has_masked);
    assert_eq!(fixture.line.lock().action_count(), 0);
    assert!(!fixture.line.lock().is_teardown_in_progress());
    assert_eq!(Arc::strong_count(&handler), 1);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn regular_action_rejects_nmi_without_merging_richer_metadata() {
    let fixture = Fixture::new(0x5f46);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let offered: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(
        fixture
            .line
            .lock()
            .install_regular_action(IrqAction::regular(handler.clone()))
    );
    let before = fixture.line.lock().desc;
    assert!(!register_nmi(
        fixture.desc.with_flags(crate::IrqFlags::WAKEUP_SOURCE),
        offered.clone()
    ));
    assert!(!register_nmi_startup(fixture.desc, offered.clone()));
    assert_eq!(fixture.line.lock().desc, before);
    assert!(!fixture.has_owner());
    assert!(!fixture.remove());
    assert_eq!(fixture.line.lock().action_count(), 1);
    drop(fixture);
    assert_eq!(Arc::strong_count(&handler), 1);
    assert_eq!(Arc::strong_count(&offered), 1);
}

fn is_invalid_context<T>(result: Result<T, IrqDescError>) -> bool {
    matches!(result, Err(IrqDescError::InvalidContext { .. }))
}

#[def_test(serial)]
fn nmi_context_rejects_management_before_publication() {
    let virq = 0x5f40;
    let desc = IrqDesc::from_virq(virq);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let observed = {
        let _pin = kspin::NoPreemptIrqSave::new();
        // SAFETY: balanced synthetic NMI; only rejection paths, no hardware,
        // blocking operations, or publication are permitted here.
        unsafe {
            with_nmi_context(|| {
                [
                    is_invalid_context(crate::try_map(desc)),
                    is_invalid_context(crate::try_enable(virq, true)),
                    is_invalid_context(crate::try_enable_irq(virq)),
                    is_invalid_context(crate::try_disable_irq_nosync(virq)),
                    is_invalid_context(crate::try_disable_irq(virq)),
                    is_invalid_context(crate::try_synchronize_irq(virq)),
                    is_invalid_context(crate::try_register(virq, handler.clone())),
                    is_invalid_context(crate::try_register_disabled(virq, handler.clone())),
                    is_invalid_context(crate::try_register_shared(virq, handler.clone())),
                    is_invalid_context(crate::try_free_irq(virq)),
                    is_invalid_context(crate::try_free_irq_action(virq, IrqActionToken::new(0))),
                    !register_nmi(desc, handler.clone()),
                    !unregister_nmi(desc),
                    !register_nmi_startup(desc, handler.clone()),
                    !crate::register(virq, handler.clone()),
                    crate::free_irq(virq).is_none(),
                    crate::alloc_msix(crate::IrqAffinity::Any).is_none(),
                    !crate::free_msix(virq),
                ]
            })
        }
    };
    assert!(observed.into_iter().all(|is_rejected| is_rejected));
    assert!(crate::descriptor(virq).is_none());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn ordinary_free_cannot_withdraw_nmi_owner() {
    let fixture = Fixture::new(0x5f41);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(fixture.install(handler.clone()));
    assert!(crate::try_free_irq(fixture.desc).unwrap().is_none());
    assert!(
        crate::try_free_irq_action(fixture.desc, IrqActionToken::new(0))
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.line.lock().action_count(), 1);
    assert_eq!(fixture.line.lock().teardown_depth_for_tests(), 0);
    assert!(fixture.has_owner());
    assert!(fixture.remove());
    assert_eq!(Arc::strong_count(&handler), 1);
}

struct DropProbe(Arc<AtomicBool>, alloc::sync::Weak<IrqLine>);

impl crate::IrqHandler for DropProbe {
    fn handle(&self, _irq: usize) -> IrqEvent {
        IrqEvent::HANDLED
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        let is_safe = karch::local_irq_enabled() && !krcu::is_reading();
        let is_safe = is_safe
            && self
                .1
                .upgrade()
                .is_none_or(|line| line.management.try_lock().is_some());
        if is_safe {
            drop(IRQ_STATE.lock());
        }
        self.0.store(is_safe, Ordering::Release);
    }
}

#[def_test]
fn nmi_drop_probe_rejects_masked_context_without_reentry() {
    let has_unlocked_drop = Arc::new(AtomicBool::new(true));
    let _irq = kspin::IrqSave::new();
    drop(DropProbe(
        has_unlocked_drop.clone(),
        alloc::sync::Weak::new(),
    ));
    assert!(!has_unlocked_drop.load(Ordering::Acquire));
}

#[def_test(serial)]
fn nmi_removal_drops_client_after_state_unlock_and_grace_period() {
    let fixture = Fixture::new(0x5f43);
    let has_unlocked_drop = Arc::new(AtomicBool::new(false));
    assert!(fixture.install(Arc::new(DropProbe(
        has_unlocked_drop.clone(),
        Arc::downgrade(&fixture.line),
    ))));
    assert!(fixture.remove());
    assert!(has_unlocked_drop.load(Ordering::Acquire));
    assert!(!fixture.has_owner());
    assert!(crate::descriptor(fixture.desc.logical_irq().unwrap()).is_some());
}

#[def_test(serial)]
fn nmi_dispatch_borrows_one_owner_even_while_writer_locks_are_held() {
    let desc = IrqDesc::from_hwirq(0x5fa0)
        .with_virq(crate::DYNAMIC_VIRQ_BASE + 0x5fa0)
        .with_domain(crate::domain::TEST_DOMAIN);
    let fixture = Fixture::mapped(desc);
    let received = Arc::new(AtomicUsize::new(0));
    let observer = received.clone();
    let handler: Handler = Arc::new(move |irq| {
        observer.store(irq, Ordering::Relaxed);
        IrqEvent::HANDLED
    });
    assert!(fixture.install(handler.clone()));
    let mut completed = false;
    {
        let _context = kspin::NoPreemptIrqSave::new();
        let _registry = IRQ_STATE.lock();
        let _entry = fixture.line.lock();
        // SAFETY: no hardware source uses this key; claim and completion stay
        // on this pinned stack. Neither reader callback takes a writer lock.
        unsafe {
            with_nmi_claim(
                |scope| {
                    super::dispatch_nmi_handler(
                        IrqRef::Domain(crate::domain::TEST_DOMAIN, desc.hwirq),
                        scope,
                    )
                },
                || completed = krcu::is_reading() && Arc::strong_count(&handler) == 2,
            );
        }
    }
    assert!(completed);
    assert_eq!(received.load(Ordering::Relaxed), desc.hwirq);
    assert!(fixture.remove());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn withdrawn_nmi_owner_survives_the_original_completion_scope() {
    let fixture = Fixture::new(0x5fa1);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(fixture.install(handler.clone()));
    let mut completed = false;
    let mut borrowed_survived = false;
    let owner = fixture
        .line
        .nmi_action
        .with_prepared_take(|take| {
            let mut retired = None;
            {
                let _pin = kspin::NoPreemptIrqSave::new();
                // SAFETY: synthetic strictly nested read/withdraw interleaving;
                // no waiting occurs until this NMI scope and CPU guard have ended.
                unsafe {
                    with_nmi_claim(
                        |scope| {
                            let borrowed = fixture.line.nmi_action.read(scope).unwrap();
                            retired = Some(take.withdraw());
                            borrowed_survived = fixture.line.nmi_action.read(scope).is_none()
                                && borrowed.handle(0).handled();
                        },
                        || completed = krcu::is_reading() && Arc::strong_count(&handler) == 2,
                    );
                }
            }
            // SAFETY: restored watched preparing task, no IRQ/reader/Line guard;
            // the permanent RCU service is live throughout this test.
            unsafe { retired.unwrap().reclaim() }
        })
        .unwrap();
    assert!(completed);
    assert!(borrowed_survived);
    assert!(owner.is_some());
    assert_eq!(Arc::strong_count(&handler), 2);
    {
        let mut entry = fixture.line.lock();
        entry.begin_nmi_close();
        entry.finish_nmi_retirement();
    }
    drop(owner);
    assert_eq!(Arc::strong_count(&handler), 1);
    assert!(!fixture.has_owner());
}

#[def_test(serial)]
fn fallback_uses_the_same_owner_and_preserves_logical_irq() {
    let desc = IrqDesc::from_hwirq(0x5fa2)
        .with_virq(crate::DYNAMIC_VIRQ_BASE + 0x5fa2)
        .with_domain(crate::domain::TEST_DOMAIN);
    let fixture = Fixture::mapped(desc);
    let received = Arc::new(AtomicUsize::new(0));
    let observer = received.clone();
    let handler: Handler = Arc::new(move |irq| {
        observer.store(irq, Ordering::Relaxed);
        IrqEvent::HANDLED
    });
    assert!(fixture.install(handler.clone()));
    let mut completed = false;
    let resolved = {
        let _pin = kspin::NoPreemptIrqSave::new();
        {
            let mut entry = fixture.line.lock();
            entry.flow = crate::IrqFlow::PerCpu;
            let plan = entry.prepare_legacy_enable();
            entry.commit_platform_plan(plan);
        }
        let _irq = crate::context::HardIrqContextGuard::enter();
        super::super::dispatch::dispatch_claim(
            IrqRef::Domain(crate::domain::TEST_DOMAIN, desc.hwirq),
            || completed = krcu::is_reading() && Arc::strong_count(&handler) == 2,
        )
    };
    assert!(completed);
    assert_eq!(resolved, desc.logical_irq());
    assert_eq!(
        received.load(Ordering::Relaxed),
        desc.logical_irq().unwrap()
    );
    assert_eq!(fixture.line.lock().in_flight(), 0);
    {
        let mut entry = fixture.line.lock();
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
    }
    assert!(fixture.remove());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn duplicate_and_shared_registration_cannot_replace_nmi_owner() {
    let fixture = Fixture::new(0x5fa3);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let offered: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(fixture.install(handler.clone()));
    let mut configured = false;
    assert!(!super::register_nmi_with_mode(
        fixture.desc,
        offered.clone(),
        super::ManagementMode::Task,
        |_| {
            configured = true;
            Ok(())
        },
    ));
    assert!(!configured);
    assert!(
        crate::try_register_shared(fixture.desc, offered.clone())
            .unwrap()
            .is_none()
    );
    assert!(!crate::try_register_disabled(fixture.desc, offered.clone()).unwrap());
    assert_eq!(Arc::strong_count(&handler), 2);
    assert_eq!(Arc::strong_count(&offered), 1);
    assert!(fixture.remove());
    assert!(fixture.install(offered.clone()));
    assert_eq!(Arc::strong_count(&handler), 1);
    assert_eq!(Arc::strong_count(&offered), 2);
    assert!(fixture.remove());
    assert_eq!(Arc::strong_count(&offered), 1);
}

#[def_test(serial)]
fn typed_nmi_domains_do_not_alias_equal_hardware_numbers() {
    let hwirq = 0x5fa4;
    let first = Fixture::mapped(
        IrqDesc::from_hwirq(hwirq)
            .with_domain(crate::domain::TEST_DOMAIN)
            .with_virq(0x16fa4),
    );
    let second = Fixture::mapped(
        IrqDesc::from_hwirq(hwirq)
            .with_domain(crate::domain::SECOND_TEST_DOMAIN)
            .with_virq(0x17fa4),
    );
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let count = first_calls.clone();
    assert!(first.install(Arc::new(move |_| {
        count.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    })));
    let count = second_calls.clone();
    assert!(second.install(Arc::new(move |_| {
        count.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    })));
    {
        let _pin = kspin::NoPreemptIrqSave::new();
        // SAFETY: synthetic Domain claims on this pinned CPU, no controller
        // operations; each read and completion finishes on this stack.
        unsafe {
            with_nmi_claim(
                |scope| {
                    super::dispatch_nmi_handler(
                        IrqRef::Domain(crate::domain::TEST_DOMAIN, hwirq),
                        scope,
                    );
                    super::dispatch_nmi_handler(
                        IrqRef::Domain(crate::domain::SECOND_TEST_DOMAIN, hwirq),
                        scope,
                    );
                },
                || {},
            );
        }
    }
    assert_eq!(first_calls.load(Ordering::Relaxed), 1);
    assert_eq!(second_calls.load(Ordering::Relaxed), 1);
    assert!(first.remove());
    let mut completed = false;
    {
        let _pin = kspin::NoPreemptIrqSave::new();
        // SAFETY: the old synthetic source is stopped. This empty software
        // dispatch exercises completion, not a live hardware reuse protocol.
        unsafe {
            with_nmi_claim(
                |scope| {
                    super::dispatch_nmi_handler(
                        IrqRef::Domain(crate::domain::TEST_DOMAIN, hwirq),
                        scope,
                    )
                },
                || completed = krcu::is_reading(),
            );
        }
    }
    assert!(completed);
    assert_eq!(first_calls.load(Ordering::Relaxed), 1);
    assert_eq!(second_calls.load(Ordering::Relaxed), 1);
    assert!(second.remove());
}
