// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wake selection across lock/writeback bits and colliding folio keys.

use alloc::sync::Arc;
use core::sync::atomic::AtomicUsize;

use ksync::{Mutex, static_lock};
use ktask::WaitQueue;
use unittest::{assert, assert_eq, def_test};

use super::*;

struct WakeCount(AtomicUsize);

// SAFETY: notification only increments an atomic counter, with no blocking or retention.
unsafe impl SyncWake for WakeCount {
    fn notify(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn waiter(address: usize, bit: FolioWaitBit, count: &WakeCount) -> FolioWaiter {
    FolioWaiter::new(address, bit, count)
}

#[def_test]
fn wake_matches_folio_and_bit_with_exclusive_lock_waiters() {
    let folio = Folio::new_zeroed(0).unwrap();
    let address = core::ptr::from_ref(&folio) as usize;
    // These key bits cancel in the hash. The address is never dereferenced,
    // so a synthetic second identity deterministically exercises a collision.
    let collision = address ^ (1 << 3) ^ (1 << 11);
    assert!(core::ptr::eq(wait_queue(address), wait_queue(collision)));
    let locks = Arc::new(WakeCount(AtomicUsize::new(0)));
    let writebacks = Arc::new(WakeCount(AtomicUsize::new(0)));
    let unrelated = Arc::new(WakeCount(AtomicUsize::new(0)));
    let first = pin!(waiter(address, FolioWaitBit::Locked, &locks));
    let second = pin!(waiter(address, FolioWaitBit::Locked, &locks));
    let wb_first = pin!(waiter(address, FolioWaitBit::Writeback, &writebacks));
    let wb_second = pin!(waiter(address, FolioWaitBit::Writeback, &writebacks));
    let other = pin!(waiter(collision, FolioWaitBit::Locked, &unrelated));
    let registrations = [
        FolioWaitRegistration(first.as_ref()),
        FolioWaitRegistration(wb_first.as_ref()),
        FolioWaitRegistration(other.as_ref()),
        FolioWaitRegistration(second.as_ref()),
        FolioWaitRegistration(wb_second.as_ref()),
    ];
    {
        let mut entries = wait_queue(address).entries.lock();
        folio
            .flags
            .store(LOCKED | WRITEBACK | WAITERS, Ordering::Relaxed);
        for registration in &registrations {
            // SAFETY: all entries are pinned and their scoped registrations
            // unlink them before destruction. Both keys select this bucket.
            let entry = unsafe { UnsafeRef::from_raw(&*registration.0) };
            entries.push_back(entry);
        }
    }

    folio.clear_bit_and_wake(FolioWaitBit::Writeback);
    assert_eq!(writebacks.0.load(Ordering::Relaxed), 2);
    assert_eq!(locks.0.load(Ordering::Relaxed), 0);
    assert_eq!(unrelated.0.load(Ordering::Relaxed), 0);
    assert!(folio.flags.load(Ordering::Relaxed) & WAITERS != 0);

    folio.clear_bit_and_wake(FolioWaitBit::Locked);
    assert_eq!(locks.0.load(Ordering::Relaxed), 1);
    assert!(folio.flags.load(Ordering::Relaxed) & WAITERS != 0);
    assert!(folio.acquire_lock());
    // A new locker can win before an earlier unlock reaches the bucket.
    // That wake must preserve the next waiter's position and notification.
    second.progress.store(HANDOFF, Ordering::Relaxed);
    folio.wake_waiters(FolioWaitBit::Locked);
    assert_eq!(locks.0.load(Ordering::Relaxed), 1);
    assert!(second.link.is_linked());
    assert_eq!(second.progress.load(Ordering::Relaxed), HANDOFF);
    folio.unlock();
    assert_eq!(locks.0.load(Ordering::Relaxed), 2);
    assert_eq!(writebacks.0.load(Ordering::Relaxed), 2);
    assert_eq!(unrelated.0.load(Ordering::Relaxed), 0);
    // Exclusive wake stops at the selected entry, retaining the hint even
    // though only an unrelated identity remains. A later unlock clears it.
    assert_eq!(
        second.progress.load(Ordering::Acquire),
        HANDOFF | WOKEN | GRANTED
    );
    assert_eq!(folio.flags.load(Ordering::Relaxed), LOCKED | WAITERS);
    assert!(!folio.acquire_lock(), "handoff already reserved ownership");
    folio.unlock();
    assert_eq!(folio.flags.load(Ordering::Relaxed), 0);
}

#[def_test]
fn handoff_exposes_data_written_after_waiter_enrollment() {
    use kcompletion::Completion;

    let folio = Arc::new(Folio::new_zeroed(0).unwrap());
    let start = Arc::new(Completion::new());
    let writer_folio = folio.clone();
    let writer_start = start.clone();
    let writer = ktask::spawn(move || {
        CompletionWait::prepare().unwrap().wait(&writer_start);
        let mut owner = writer_folio.try_lock().unwrap();
        owner.data().fill(0x6d);
        // Unlock is the only publication to the waiting reader after the write.
        drop(owner);
    });
    let wait = CompletionWait::prepare().unwrap();
    let address = Arc::as_ptr(&folio) as usize;
    let waiter = pin!(FolioWaiter::new(
        address,
        FolioWaitBit::Locked,
        wait.notifier()
    ));
    let _registration = FolioWaitRegistration(waiter.as_ref());
    waiter.progress.store(HANDOFF, Ordering::Relaxed);
    {
        let mut entries = wait_queue(address).entries.lock();
        folio.flags.fetch_or(WAITERS, Ordering::Relaxed);
        // SAFETY: the pinned waiter and borrowed notifier outlive registration;
        // the bucket serializes all access and cleanup unlinks before drop.
        entries.push_back(unsafe { UnsafeRef::from_raw(&*waiter) });
    }
    start.complete();
    while waiter.progress.load(Ordering::Acquire) & WOKEN == 0 {
        wait.park_lock(address, || {});
    }
    assert_eq!(
        waiter.progress.load(Ordering::Acquire),
        HANDOFF | WOKEN | GRANTED
    );
    let mut owned = folio.locked_guard();
    assert!(owned.data().iter().all(|&byte| byte == 0x6d));
    drop(owned);
    assert_eq!(writer.join(), 0);
}

// Pause only this test's reader after notification, before it can acquire or
// accept ownership. This lets the controller force each lost race without
// relying on timing, scheduler priority, or a loop polling task state.
struct HandoffObservation {
    folio: Arc<Folio>,
    enrolled: AtomicUsize,
    notified: AtomicUsize,
    resume: AtomicUsize,
    events: WaitQueue,
}

static_lock! {
    static HANDOFF_OBSERVATION: Mutex<Option<Arc<HandoffObservation>>> = Mutex::new(None);
}

struct HandoffRegistration;
impl Drop for HandoffRegistration {
    fn drop(&mut self) {
        HANDOFF_OBSERVATION.lock().take();
    }
}

fn observation(folio: &Folio) -> Option<Arc<HandoffObservation>> {
    HANDOFF_OBSERVATION
        .lock()
        .as_ref()
        .filter(|observed| core::ptr::eq(Arc::as_ptr(&observed.folio), folio))
        .cloned()
}

pub(super) fn observe_enrollment(folio: &Folio) {
    if let Some(observed) = observation(folio) {
        observed.enrolled.fetch_add(1, Ordering::Release);
        observed.events.notify_all(false);
    }
}

pub(super) fn observe_notification(folio: &Folio) {
    if let Some(observed) = observation(folio) {
        let round = observed.notified.fetch_add(1, Ordering::AcqRel) + 1;
        observed.events.notify_all(false);
        observed
            .events
            .wait_until(|| observed.resume.load(Ordering::Acquire) >= round);
    }
}

#[def_test(serial)]
fn repeated_lost_acquisitions_transfer_ownership_to_the_waiter() {
    let folio = Arc::new(Folio::new_zeroed(0).unwrap());
    let observed = Arc::new(HandoffObservation {
        folio: folio.clone(),
        enrolled: AtomicUsize::new(0),
        notified: AtomicUsize::new(0),
        resume: AtomicUsize::new(0),
        events: WaitQueue::new(),
    });
    assert!(HANDOFF_OBSERVATION.lock().is_none());
    *HANDOFF_OBSERVATION.lock() = Some(observed.clone());
    let _registration = HandoffRegistration;
    let mut owner = Some(folio.lock().unwrap());
    owner.as_mut().unwrap().data()[0] = 0xa5;
    let reader_folio = folio.clone();
    let reader = ktask::spawn(move || {
        let mut locked = reader_folio.lock().unwrap();
        // This test checks reservation after bounded contention. Data
        // publication without a post-write test rendezvous is checked separately.
        core::assert_eq!(locked.data()[0], 0xa5);
    });

    for round in 1..=LOCK_RETRY_BUDGET {
        observed
            .events
            .wait_until(|| observed.enrolled.load(Ordering::Acquire) >= round);
        drop(owner.take());
        observed
            .events
            .wait_until(|| observed.notified.load(Ordering::Acquire) >= round);
        owner = folio.try_lock();
        core::assert!(
            owner.is_some(),
            "ordinary notification still permits barging"
        );
        observed.resume.store(round, Ordering::Release);
        observed.events.notify_all(false);
    }

    let handoff_round = LOCK_RETRY_BUDGET + 1;
    observed
        .events
        .wait_until(|| observed.enrolled.load(Ordering::Acquire) >= handoff_round);
    drop(owner.take());
    observed
        .events
        .wait_until(|| observed.notified.load(Ordering::Acquire) >= handoff_round);
    let competitor = folio.try_lock();
    let was_granted = competitor.is_none();
    drop(competitor);
    observed.resume.store(handoff_round, Ordering::Release);
    observed.events.notify_all(false);
    assert_eq!(reader.join(), 0);
    assert!(was_granted, "a notified handoff must already own the lock");
    assert!(folio.try_lock().is_some());
}

#[def_test]
fn contended_folio_wait_rejects_irq_context_without_losing_the_owner() {
    let folio = Folio::new_zeroed(0).unwrap();
    let owner = folio.lock().unwrap();
    {
        let _irq = kirq::context::test_support::ScopedHardIrqContext::enter();
        core::assert!(matches!(folio.lock(), Err(kerrno::KError::InvalidInput)));
        core::assert!(matches!(
            folio.lock_killable(),
            Err(kerrno::KError::InvalidInput)
        ));
        core::assert!(folio.try_lock().is_none());
    }
    drop(owner);
    core::assert!(folio.try_lock().is_some());
}

#[def_test(serial)]
fn killed_reader_leaves_initialization_owned_by_the_completion_token() {
    use kalloc_types::KBox;
    use kcompletion::Completion;
    use kerrno::KError;

    struct SignalRuntime {
        pending: Arc<SpinNoIrq<bool>>,
        parked: Arc<Completion>,
    }
    impl ktask::UserTaskRuntime for SignalRuntime {
        fn fatal_signal_pending(&self) -> bool {
            let pending = *self.pending.lock();
            if ktask::current().state() == ktask::TaskState::BlockedKillable {
                self.parked.complete();
            }
            pending
        }

        fn as_any(&self) -> &dyn core::any::Any {
            self
        }
    }

    let cache = Arc::new(crate::MappingPages::new());
    let mut pending_read = None;
    cache
        .readahead_folios(0..1, |folios| pending_read = folios.next())
        .unwrap();
    let resident = cache.state.lock().entries[&0].clone();
    let signal = Arc::new(SpinNoIrq::new(false));
    let parked = Arc::new(Completion::new());
    let reader_cache = cache.clone();
    let reader = ktask::prepare_task(ktask::TaskInner::new_user(
        ktask::PreparedTaskEntry::new(move || {
            core::assert!(matches!(
                reader_cache
                    .pin_folio_or_create(0, |_| panic!("initialization still has an owner")),
                Err(KError::Interrupted)
            ));
        })
        .unwrap(),
        "killable-folio-read".into(),
        kidentity::allocate_root_pid_handle().unwrap(),
        KBox::new(SignalRuntime {
            pending: signal.clone(),
            parked: parked.clone(),
        })
        .unwrap(),
    ));
    ktask::activate_task(&reader);
    CompletionWait::prepare().unwrap().wait(&parked);
    *signal.lock() = true;
    ktask::signal_task(&reader);
    assert_eq!(reader.join(), 0);
    assert!(resident.try_lock().is_none());
    assert!(!resident.is_uptodate());
    assert_eq!(cache.nrpages(), 1);
    let mut read = pending_read.unwrap();
    read.data().fill(0x7b);
    read.complete(Ok(())).unwrap();
    cache
        .with_folio(0, |folio| {
            core::assert_eq!(folio.unwrap().data()[0], 0x7b);
        })
        .unwrap();
}
