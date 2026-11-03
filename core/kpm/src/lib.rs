// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System power-management coordination, independent of runtime bring-up.
//!
//! Currently provides a single-present-CPU PM time transaction, not a userspace
//! suspend entry. See `docs/design.md` and `docs/security.md` for its boundaries.
//!
//! The coordinator freezes all admitted tasks, deferring new scheduler activation;
//! a platform provider supplies device/NMI exclusion and firmware entry.
//! It orders RTC accounting, monotonic freeze and
//! local clock events and optional ordered device callbacks, and unwinds completed
//! preparation on ordinary errors.
//! SMP rendezvous and concrete platform implementations are separate work.

#![no_std]

extern crate alloc;

mod device;

use core::sync::atomic::{AtomicBool, Ordering};

use device::DeviceSet;
pub use device::{DeviceSuspendError, DeviceSuspendOps};
use kerrno::{KError, KResult};
use khal::time::{MonotonicSuspendError, MonotonicSuspendToken};
use kiface_time_api::PersistentClock;
use kspin::NoPreemptIrqSave;
use ktask::{
    freezer::{FreezeCancellation, FreezeError, SystemFrozenTasks},
    timer_suspend::{LocalTimerSuspend, TimerSuspendError},
};
use ktime::{RtcSleepAccounting, RtcSleepError, RtcSleepSession};
use ktime_types::TimeSpan;

/// Platform-owned exclusion and hardware recovery around one PM transaction.
///
/// No current platform implements this contract. In particular, the legacy
/// `khal::power::suspend_to_ram` stub alone does not satisfy it.
///
/// # Safety
/// The coordinator closes scheduler admission and freezes every non-exempt
/// admitted task after preparation. SuspendTasks may exempt only explicitly
/// identified kernel infrastructure needed for PM progress; the provider must
/// keep those tasks' accesses safe. Kernel workers must provide safe checkpoints
/// or have their producers/work drained before being exempted. Preparation must exclude timer producers,
/// idle/fixup dispatch and NMI/watchdog clock reads until finish, while allowing
/// the PM task and resources needed for RTC reads to progress. Counter, event
/// and RTC access must remain valid until enter. No CPU may be brought online.
/// Entry and restoration run on the sole present CPU with IRQs masked and
/// MONOTONIC frozen: never sleep, allocate, enable IRQs, wait on logical time,
/// dispatch repair or unwind there. Restoration must re-establish counter and
/// controller access/routing at unchanged counter frequency, even if enter
/// failed or only partially powered down hardware. All methods must return
/// normally without unwinding; finish must release preparation without failure.
/// Before devices become unavailable, cancel/quiesce callbacks that could
/// access them. By the time restore returns, resources reachable by enabled
/// IRQs, clock-change observers or resumed timer callbacks must be usable, or
/// their consumers must remain separately suppressed until ready. Event rearm
/// precedes finish; finish cannot defer this interrupt-safety requirement.
/// With ordered device participants, preparation must also exclude exempt
/// accesses; it must not duplicate their suspend/resume callbacks. Devices are
/// resumed before finish, while platform exclusions and frozen tasks remain held.
/// Prepare and finish must preserve the enabled local IRQ state. These
/// obligations include any device mappings needed by the existing HAL.
pub unsafe trait PlatformSuspendOps {
    /// Stops producers and prepares device/clock quiescence with IRQs enabled.
    /// Ordinary tasks still run until the following system freezer step, which
    /// closes scheduler admission and takes a complete task snapshot.
    /// Keep task-accessible resources usable until enter, including after a
    /// task reaches its checkpoint: failed freezing thaws partial acknowledgments
    /// before finish is called. Prepare may arrange exclusion, not power off
    /// devices that thawed tasks could access.
    /// May sleep. On error, undo this method's own partial preparation; finish
    /// is called only after preparation succeeded.
    ///
    /// # Errors
    /// Returns unsupported entry or a preparation failure before time is frozen.
    fn prepare(&mut self) -> KResult<()>;

    /// Enters the platform sleep state and returns on wake or failed entry.
    /// Runs with IRQs masked and time frozen. Restoration is called regardless
    /// of the result, before any further counter access.
    ///
    /// # Errors
    /// Reports refused or interrupted entry; partial hardware changes are allowed
    /// only when restore can unconditionally repair them.
    fn enter(&mut self) -> KResult<()>;

    /// Restores counter/controller/RTC accessibility, keeping exclusions active.
    /// Must succeed on both wake and entry error, with IRQs still masked.
    /// Do not release NMI or ordinary execution yet.
    fn restore(&mut self);

    /// Releases successful preparation after clocks/events are usable again.
    /// Runs with the original enabled IRQ state, may sleep, and must also handle
    /// early aborts that never entered platform sleep. Must not unwind or fail.
    fn finish(&mut self);
}

/// Failure returned only after ordinary rollback or resume cleanup completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuspendError {
    /// A sleepable initialized task with enabled IRQs is required.
    InvalidContext,
    /// This first coordinator cannot stop and restart other present CPUs.
    MultipleCpus,
    /// Another PM transaction is active, including preparation or cleanup.
    Busy,
    /// Platform preparation failed and undid its own partial changes.
    Prepare(KError),
    /// Cooperative task freezing failed; requests were cancelled and preparation released.
    TaskFreeze(FreezeError),
    /// Initial RTC accounting failed; platform preparation was released.
    RtcPrepare(RtcSleepError),
    /// Device preparation failed; successful predecessors were resumed in reverse order.
    DeviceSuspend(DeviceSuspendError),
    /// Local events could not be paused; pending accounting was cancelled.
    TimerPause(TimerSuspendError),
    /// The clock could not be frozen; events were restored and re-arbitrated.
    ClockFreeze(MonotonicSuspendError),
    /// The accounting freeze marker failed; the owned clock/events were restored.
    /// No platform entry occurred; the short excluded interval was not settled.
    MarkFrozen(RtcSleepError),
    /// Wake/entry cleanup completed, but entry or sleep compensation failed.
    /// A successful sleep_time was already committed exactly once, even when
    /// entry_error is present. A failed sleep_time was not committed and its
    /// accounting was cancelled; missing elapsed time remains uncompensated.
    Resume {
        /// Error from platform entry, if any.
        entry_error: Option<KError>,
        /// Result of settling the interval actually excluded from MONOTONIC.
        sleep_time: Result<TimeSpan, RtcSleepError>,
    },
}

static TRANSACTION_ACTIVE: AtomicBool = AtomicBool::new(false);

struct Admission<'a>(&'a AtomicBool);
impl<'a> Admission<'a> {
    fn acquire(active: &'a AtomicBool) -> Result<Self, SuspendError> {
        active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| SuspendError::Busy)?;
        Ok(Self(active))
    }
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn validate_context(
    is_sleepable: bool,
    present_cpus: impl FnOnce() -> usize,
) -> Result<(), SuspendError> {
    if !is_sleepable {
        return Err(SuspendError::InvalidContext);
    }
    if present_cpus() != 1 {
        return Err(SuspendError::MultipleCpus);
    }
    Ok(())
}

/// Explicit kernel-infrastructure exemptions and cancellation policy for PM.
/// ktask collects all admitted tasks and defers new activation until recovery
/// completes. The platform must keep exempt infrastructure safe and available.
pub struct SuspendTasks<'a> {
    /// Already admitted kernel infrastructure required for recovery progress.
    /// User tasks cannot be exempted; PM itself and idle are excluded automatically.
    pub exempt: &'a [ktask::KtaskRef],
    /// Maximum MONOTONIC duration to wait before clocks/events are paused.
    pub timeout: TimeSpan,
    /// One-shot cancellation notification, prepared before platform preparation.
    pub cancellation: &'a FreezeCancellation,
}

/// Coordinates one single-CPU suspend attempt using a prepared RTC source.
///
/// Source discovery/mapping and its trust/latency policy must already be set.
/// Both RTC reads run with IRQs enabled, outside spinlocks, with MONOTONIC
/// running. The resume read occurs while local events remain paused: it must
/// not depend on that timer for progress (including timeout or scheduling wake
/// events). The selected platform must retain PM-task/source progress while
/// excluding ordinary execution. This function neither discovers nor writes
/// the RTC and cannot authenticate its cross-host-sleep continuity.
///
/// Only one present CPU is admitted, even in an SMP-capable build. No resource
/// reclamation or reversible CPU parking is inferred from the terminal SMP-stop
/// interface. No syscall, platform stub, or real sleep path is wired by this API.
///
/// # Errors
/// See SuspendError. Every ordinary error releases successful preparation and
/// cancels pending accounting before events/ordinary execution resume. A failed
/// RTC estimate after freeze is reported as uncompensated, not hidden as zero.
/// Entry errors still settle the actual frozen interval, since it was excluded
/// even when firmware refused to sleep. Success returns the committed duration.
///
/// # Panics
/// Invariant failure in owned HAL/event restoration is fatal: returning while
/// time remains frozen or hardware inaccessible would release unsafe execution.
/// Such failures imply violation of PlatformSuspendOps's unsafe contract or broken
/// internal credential ownership; ordinary entry/source errors do not panic.
pub fn suspend_uniprocessor<P: PlatformSuspendOps, C: PersistentClock>(
    platform: &mut P,
    source: &mut RtcSleepAccounting<C>,
    tasks: SuspendTasks<'_>,
) -> Result<TimeSpan, SuspendError> {
    suspend_uniprocessor_with_devices(platform, source, tasks, &mut [])
}

/// Coordinates a single-CPU suspend attempt with ordered device callbacks.
///
/// Devices suspend in supplied order after task freezing, with IRQs enabled.
/// Every successfully suspended device resumes in reverse order on both abort
/// and wake, after time/events recover and before platform finish and task thaw.
/// The failing device must undo its own partial suspension. Supply consumers
/// before providers; no topology discovery, sorting or allocation is performed.
/// The mutable list borrows prevent caller-side mutation during the transaction.
///
/// PM-critical resources (including the selected RTC source and resources needed
/// to restore time/events) must remain available; their platform protocol is
/// separate. Device callbacks must suppress asynchronous consumers until their
/// resources are usable, since IRQs/events resume before device callbacks do.
/// All context, topology and platform requirements of [`suspend_uniprocessor`]
/// apply. An empty list retains that function's behavior.
///
/// # Errors
/// Returns [`SuspendError::DeviceSuspend`] after reversing completed device
/// suspension and releasing platform preparation. Other errors follow
/// [`suspend_uniprocessor`], with devices restored before frozen tasks are thawed.
///
/// # Panics
/// Owned time/event restoration invariant failures are fatal, as documented by
/// [`suspend_uniprocessor`]. Device and platform callbacks must not unwind.
pub fn suspend_uniprocessor_with_devices<P: PlatformSuspendOps, C: PersistentClock>(
    platform: &mut P,
    source: &mut RtcSleepAccounting<C>,
    tasks: SuspendTasks<'_>,
    devices: &mut [&mut dyn DeviceSuspendOps],
) -> Result<TimeSpan, SuspendError> {
    validate_context(ktask::is_sleepable_context(), kcpu_id_map::nr_cpus)?;
    let _admission = Admission::acquire(&TRANSACTION_ACTIVE)?;
    run(
        &mut SystemOps { platform, tasks },
        source,
        &mut DeviceSet::new(devices),
    )
}

// Separate adapters let deterministic fixtures exercise the actual executor
// without freezing the running test kernel or changing global sleep offsets.
trait SleepSession {
    fn mark_frozen(&mut self) -> Result<(), RtcSleepError>;
    fn settle(&mut self) -> Result<TimeSpan, RtcSleepError>;
    // Drop cancels any still-pending accounting without notifying consumers.
}
trait SleepSource {
    type Session<'a>: SleepSession
    where
        Self: 'a;
    fn begin(&mut self) -> Result<Self::Session<'_>, RtcSleepError>;
}
impl<C: PersistentClock> SleepSource for RtcSleepAccounting<C> {
    type Session<'a>
        = RtcSleepSession<'a, C>
    where
        Self: 'a;

    fn begin(&mut self) -> Result<Self::Session<'_>, RtcSleepError> {
        RtcSleepAccounting::begin(self)
    }
}
impl<C: PersistentClock> SleepSession for RtcSleepSession<'_, C> {
    fn mark_frozen(&mut self) -> Result<(), RtcSleepError> {
        self.mark_suspended()
    }

    fn settle(&mut self) -> Result<TimeSpan, RtcSleepError> {
        self.resume_from_rtc()
    }
}

trait SuspendOps {
    type Events;
    type Clock;
    type Critical;
    type Tasks;
    fn freeze_tasks(&mut self) -> Result<Self::Tasks, FreezeError>;
    fn prepare(&mut self) -> KResult<()>;
    fn finish(&mut self);
    fn mask_irqs(&mut self) -> Self::Critical;
    fn pause_events(&mut self) -> Result<Self::Events, TimerSuspendError>;
    fn freeze(&mut self) -> Result<Self::Clock, MonotonicSuspendError>;
    fn enter(&mut self) -> KResult<()>;
    fn restore_platform(&mut self);
    fn resume_clock(&mut self, token: &Self::Clock);
    fn restore_events(&mut self, token: &mut Self::Events);
    fn resume_events(&mut self, token: &mut Self::Events);
}

fn run<O: SuspendOps, S: SleepSource>(
    ops: &mut O,
    source: &mut S,
    devices: &mut DeviceSet<'_, '_>,
) -> Result<TimeSpan, SuspendError> {
    ops.prepare().map_err(SuspendError::Prepare)?;
    // Keep the guard through finish: no listed task can race hardware recovery.
    let _tasks = match ops.freeze_tasks() {
        Ok(tasks) => tasks,
        Err(error) => {
            finish(ops, devices);
            return Err(SuspendError::TaskFreeze(error));
        }
    };
    if let Err(error) = devices.suspend() {
        finish(ops, devices);
        return Err(SuspendError::DeviceSuspend(error));
    }
    let mut accounting = match source.begin() {
        Ok(session) => session,
        Err(error) => {
            finish(ops, devices);
            return Err(SuspendError::RtcPrepare(error));
        }
    };
    let critical = ops.mask_irqs();
    let mut events = match ops.pause_events() {
        Ok(events) => events,
        Err(error) => {
            drop(critical);
            drop(accounting);
            finish(ops, devices);
            return Err(SuspendError::TimerPause(error));
        }
    };
    let clock = match ops.freeze() {
        Ok(clock) => clock,
        Err(error) => {
            ops.restore_events(&mut events);
            drop(critical);
            drop(accounting);
            ops.resume_events(&mut events);
            finish(ops, devices);
            return Err(SuspendError::ClockFreeze(error));
        }
    };
    if let Err(error) = accounting.mark_frozen() {
        ops.resume_clock(&clock);
        ops.restore_events(&mut events);
        drop(critical);
        drop(accounting);
        ops.resume_events(&mut events);
        finish(ops, devices);
        return Err(SuspendError::MarkFrozen(error));
    }
    let entry = ops.enter();
    ops.restore_platform();
    ops.resume_clock(&clock);
    ops.restore_events(&mut events);
    drop(critical);
    let sleep_time = accounting.settle();
    // On failure Drop cancels the still-active interval and calibration before
    // producers can run. On success the same Drop is inert.
    drop(accounting);
    ops.resume_events(&mut events);
    finish(ops, devices);
    match (entry, sleep_time) {
        (Ok(()), Ok(delta)) => Ok(delta),
        (entry, sleep_time) => Err(SuspendError::Resume {
            entry_error: entry.err(),
            sleep_time,
        }),
    }
}

fn finish<O: SuspendOps>(ops: &mut O, devices: &mut DeviceSet<'_, '_>) {
    devices.resume();
    ops.finish();
}

struct SystemOps<'a, P> {
    platform: &'a mut P,
    tasks: SuspendTasks<'a>,
}
impl<P: PlatformSuspendOps> SuspendOps for SystemOps<'_, P> {
    type Clock = MonotonicSuspendToken;
    type Critical = NoPreemptIrqSave;
    type Events = LocalTimerSuspend;
    type Tasks = SystemFrozenTasks;

    fn freeze_tasks(&mut self) -> Result<Self::Tasks, FreezeError> {
        ktask::freezer::freeze_system_tasks(
            self.tasks.exempt,
            self.tasks.timeout,
            self.tasks.cancellation,
        )
    }

    fn prepare(&mut self) -> KResult<()> {
        self.platform.prepare()
    }

    fn finish(&mut self) {
        self.platform.finish();
    }

    fn mask_irqs(&mut self) -> Self::Critical {
        NoPreemptIrqSave::new()
    }

    fn pause_events(&mut self) -> Result<Self::Events, TimerSuspendError> {
        ktask::timer_suspend::suspend_local_timer()
    }

    fn freeze(&mut self) -> Result<Self::Clock, MonotonicSuspendError> {
        khal::time::suspend_monotonic_clock()
    }

    fn enter(&mut self) -> KResult<()> {
        self.platform.enter()
    }

    fn restore_platform(&mut self) {
        self.platform.restore();
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: private executor owns the live freeze \
                  token; UP, IRQ exclusion and platform-required NMI/repair quiescence exclude \
                  other writers, and freeze reserved resume identity capacity"
    )]
    fn resume_clock(&mut self, token: &Self::Clock) {
        khal::time::resume_monotonic_clock(token).expect("PM lost its owned monotonic freeze");
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: private executor owns a fresh event token \
                  on its sole CPU with IRQs masked; platform restored controller access and no \
                  competing transition is allowed"
    )]
    fn restore_events(&mut self, token: &mut Self::Events) {
        token
            .restore_hardware()
            .expect("PM lost its owned paused event");
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: executor has restored this owned event \
                  exactly once; runtime admission allows only one present CPU, so RTC waits \
                  cannot migrate the owner; the local guard excludes IRQ/preemption during \
                  arbitration"
    )]
    fn resume_events(&mut self, token: &mut Self::Events) {
        let _guard = NoPreemptIrqSave::new();
        token.resume().expect("PM lost its restored local event");
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/suspend.rs");
