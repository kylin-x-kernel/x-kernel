// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Folio-and-bit keyed waits, with no task owner stored in the folio.
//!
//! Initialization can finish in another context, so there is no task owner to
//! inspect for mutex-style spinning or recursive locking. Contenders retry
//! under the bucket lock before parking; holding a token forbids reentry.

use core::{
    marker::PhantomPinned,
    pin::{Pin, pin},
    ptr::NonNull,
    sync::atomic::{AtomicU8, Ordering},
};

use intrusive_collections::{LinkedList, LinkedListAtomicLink, UnsafeRef, intrusive_adapter};
use kcompletion::SyncWake;
use ksync::spin::SpinNoIrq;
use ktask::completion::CompletionWait;

use super::{Folio, LOCKED, WRITEBACK};

const WAITERS: u8 = 4;

// Per-wait progress, separate from the folio's state and the task wake latch.
const WOKEN: u8 = 1;
const GRANTED: u8 = 2;
const HANDOFF: u8 = 4;
// Match the initial unfairness budget of the page-lock wait protocol.
const LOCK_RETRY_BUDGET: usize = 5;

#[repr(u8)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum FolioWaitBit {
    Locked    = LOCKED,
    Writeback = WRITEBACK,
}

impl FolioWaitBit {
    fn try_complete(self, folio: &Folio) -> bool {
        match self {
            Self::Locked => folio.acquire_lock(),
            Self::Writeback => !folio.is_under_writeback(),
        }
    }
}

struct FolioWaitQueue {
    entries: SpinNoIrq<LinkedList<FolioWaitAdapter>>,
}

impl FolioWaitQueue {
    const fn new() -> Self {
        Self {
            #[cfg(not(feature = "stats"))]
            entries: SpinNoIrq::new(LinkedList::new(FolioWaitAdapter::NEW)),
            #[cfg(feature = "stats")]
            entries: SpinNoIrq::new_with_stats(
                LinkedList::new(FolioWaitAdapter::NEW),
                &BUCKET_STATS,
            ),
        }
    }
}

#[cfg(feature = "stats")]
static BUCKET_STATS: ksync::LockClassStats =
    ksync::LockClassStats::new("folio wait buckets", "SpinNoIrq");
#[cfg(feature = "stats")]
#[linkme::distributed_slice(ksync::LOCK_CLASSES)]
static BUCKET_CLASS: &ksync::LockClassStats = &BUCKET_STATS;
#[cfg(feature = "stats")]
pub(super) static FOLIO_STATS: ksync::LockClassStats =
    ksync::LockClassStats::new("folio lock", "Folio");
#[cfg(feature = "stats")]
#[linkme::distributed_slice(ksync::LOCK_CLASSES)]
static FOLIO_CLASS: &ksync::LockClassStats = &FOLIO_STATS;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum WaitMode {
    Uninterruptible,
    Killable,
}

static FOLIO_WAITERS: [FolioWaitQueue; 256] = [const { FolioWaitQueue::new() }; 256];

fn wait_queue(address: usize) -> &'static FolioWaitQueue {
    &FOLIO_WAITERS[((address >> 3) ^ (address >> 11)) % FOLIO_WAITERS.len()]
}

// Like a page wait entry, this exists only on the waiting task's stack. The
// address is an equality key, never dereferenced. The wait call borrows the
// folio until removal; pinning and Drop keep the intrusive link valid.
struct FolioWaiter {
    link: LinkedListAtomicLink,
    address: usize,
    bit: FolioWaitBit,
    progress: AtomicU8,
    wake: NonNull<()>,
    notify: unsafe fn(NonNull<()>),
    _pin: PhantomPinned,
}

// SAFETY: the bucket serializes link access and notification against removal.
// The private registration retains the pinned node and its borrowed SyncWake
// until removal. Progress is atomic; all other fields are immutable.
unsafe impl Send for FolioWaiter {}
// SAFETY: shared access follows the same bucket and atomic-progress protocol.
unsafe impl Sync for FolioWaiter {}

impl FolioWaiter {
    fn new<W: SyncWake>(address: usize, bit: FolioWaitBit, wake: &W) -> Self {
        Self {
            link: LinkedListAtomicLink::new(),
            address,
            bit,
            progress: AtomicU8::new(0),
            wake: NonNull::from(wake).cast(),
            notify: notify_borrowed::<W>,
            _pin: PhantomPinned,
        }
    }
}

/// # Safety
/// `data` points to a live shared `W`. The caller holds the bucket lock,
/// excluding registration cleanup and keeping that wake borrow alive.
unsafe fn notify_borrowed<W: SyncWake>(data: NonNull<()>) {
    // SAFETY: new pairs the pointer and trampoline, and registration teardown
    // takes the same bucket lock before the wake owner can be destroyed.
    unsafe { data.cast::<W>().as_ref() }.notify();
}

intrusive_adapter!(FolioWaitAdapter = UnsafeRef<FolioWaiter>: FolioWaiter {
    link => LinkedListAtomicLink
});

// Unlink before Rust drops the pinned node itself: the queue may still be
// borrowing that node while this registration starts its destruction.
struct FolioWaitRegistration<'a>(Pin<&'a FolioWaiter>);

impl Drop for FolioWaitRegistration<'_> {
    fn drop(&mut self) {
        let mut entries = wait_queue(self.0.address).entries.lock();
        if !self.0.link.is_linked() {
            return;
        }
        // SAFETY: this pinned entry is inserted only into its address's bucket,
        // and that lock serializes removal against wakeup. It remains alive
        // through removal, including early return or unwinding from the wait.
        unsafe { entries.cursor_mut_from_ptr(&*self.0) }.remove();
    }
}

impl Folio {
    pub(super) fn wait_for_bit(
        &self,
        bit: FolioWaitBit,
        task_wait: &CompletionWait,
        mode: WaitMode,
    ) -> bool {
        if bit.try_complete(self) {
            return true;
        }
        let address = core::ptr::from_ref(self) as usize;
        let waiter = pin!(FolioWaiter::new(address, bit, task_wait.notifier()));
        let registration = FolioWaitRegistration(waiter.as_ref());
        let mut retries = LOCK_RETRY_BUDGET;
        #[cfg(feature = "stats")]
        let mut counted_contention = false;
        #[cfg(feature = "stats")]
        let mut record_contention = || {
            if !counted_contention && bit == FolioWaitBit::Locked {
                FOLIO_STATS.record_contentions(1);
                counted_contention = true;
            }
        };
        loop {
            let mut entries = wait_queue(address).entries.lock();
            debug_assert!(!waiter.link.is_linked());
            let progress = if bit == FolioWaitBit::Locked && retries == 0 {
                HANDOFF
            } else {
                0
            };
            waiter.progress.store(progress, Ordering::Relaxed);
            // Serialize enrollment with wakeup and recheck after setting the
            // hint. A completion either sees WAITERS or precedes this recheck.
            self.flags.fetch_or(WAITERS, Ordering::Relaxed);
            if bit.try_complete(self) {
                return true;
            }
            if mode == WaitMode::Killable && task_wait.is_killed() {
                return false;
            }
            // SAFETY: pin! keeps this shared node stable; the registration
            // unlinks it before destruction. This bucket lock serializes all
            // link access; a notified entry was removed before reenrollment.
            let entry = unsafe { UnsafeRef::from_raw(&*waiter) };
            entries.push_back(entry);
            drop(entries);
            #[cfg(unittest)]
            match bit {
                FolioWaitBit::Locked => {
                    crate::pagecache_unittest::observe_read_wait(self);
                    wait_unittest::observe_enrollment(self);
                }
                FolioWaitBit::Writeback => crate::pagecache_unittest::observe_writeback_wait(self),
            }
            loop {
                // A scheduler wake alone neither completes this wait nor
                // consumes its budget. The folio waker publishes progress.
                let progress = waiter.progress.load(Ordering::Acquire);
                if progress & WOKEN == 0 {
                    if mode == WaitMode::Killable && task_wait.is_killed() {
                        // Grant and cancellation are serialized by the bucket.
                        // A granted lock belongs to this waiter even if a signal raced.
                        drop(registration);
                        return waiter.progress.load(Ordering::Acquire) & GRANTED != 0;
                    }
                    #[cfg(feature = "stats")]
                    let on_block = &mut record_contention;
                    #[cfg(not(feature = "stats"))]
                    let on_block = || {};
                    match mode {
                        WaitMode::Uninterruptible => task_wait.park_lock(address, on_block),
                        WaitMode::Killable => task_wait.park_lock_killable(address, on_block),
                    };
                    continue;
                }
                #[cfg(unittest)]
                if bit == FolioWaitBit::Locked {
                    wait_unittest::observe_notification(self);
                }
                if bit == FolioWaitBit::Writeback || progress & GRANTED != 0 || self.acquire_lock()
                {
                    return true;
                }
                // Only a notified lock waiter which lost the acquisition
                // retries. Granted ownership must never be acquired again.
                retries = retries.saturating_sub(1);
                break;
            }
        }
    }

    pub(super) fn clear_bit_and_wake(&self, bit: FolioWaitBit) {
        if self.clear_bit(bit) {
            self.wake_waiters(bit);
        }
    }

    // Return the hint from the clearing RMW: enrollment either precedes it,
    // or sees the cleared bit in its predicate recheck under the bucket lock.
    pub(super) fn clear_bit(&self, bit: FolioWaitBit) -> bool {
        self.flags.fetch_and(!(bit as u8), Ordering::Release) & WAITERS != 0
    }

    pub(super) fn wake_waiters(&self, bit: FolioWaitBit) {
        let address = core::ptr::from_ref(self) as usize;
        let mut entries = wait_queue(address).entries.lock();
        let mut cursor = entries.front_mut();
        let mut has_waiters = false;
        while let Some(waiter) = cursor.get() {
            if waiter.address != address {
                cursor.move_next();
                continue;
            }
            if waiter.bit != bit {
                has_waiters = true;
                cursor.move_next();
                continue;
            }
            let mut progress = waiter.progress.load(Ordering::Relaxed);
            if bit == FolioWaitBit::Locked {
                // Fast-path lockers do not take the bucket lock. If one won,
                // keep this waiter's position and let its unlock try again.
                if progress & HANDOFF != 0 {
                    if !self.acquire_lock() {
                        return;
                    }
                    progress |= GRANTED;
                } else if self.flags.load(Ordering::Relaxed) & LOCKED != 0 {
                    return;
                }
            }
            // A handoff acquires the previous owner's Release before this
            // Release publishes ownership to the waiter's Acquire load.
            waiter.progress.store(progress | WOKEN, Ordering::Release);
            let wake = waiter.wake;
            let notify = waiter.notify;
            cursor.remove();
            // SAFETY: the scoped registration cannot retire its wake borrow
            // until it acquires this bucket. SyncWake requires IRQ-safe,
            // nonblocking notification, and parking holds no bucket lock.
            unsafe { notify(wake) };
            if bit == FolioWaitBit::Locked {
                // WAITERS is a conservative hint. Stop after one exclusive
                // wake instead of scanning the tail to make the hint exact.
                return;
            }
        }
        if !has_waiters {
            self.flags.fetch_and(!WAITERS, Ordering::Relaxed);
        }
    }
}

unittest::path_mod_test!(wait_unittest, "../../ut-tests/folio_wait_unittest.rs");
