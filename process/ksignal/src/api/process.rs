// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-level signal state and delivery.
use alloc::sync::Arc;
use core::{
    array,
    ops::{Index, IndexMut},
    sync::atomic::{AtomicU64, Ordering},
};

use kerrno::{KError, KResult};
use kspin::SpinNoIrq;
use strum::IntoEnumIterator;

#[cfg(unittest)]
use crate::SignalInfo;
use crate::{
    DefaultSignalAction, MAX_SIGNALS, SigLock, SigchldChildExitSignalInfo, SignalAction,
    SignalActionFlags, SignalDisposition, SignalNotification, SignalSet, Signo,
    TimerSignalRejection, TimerSignalSendOutcome,
};

/// Container for signal actions across all supported signals.
///
/// This structure manages signal handlers for each signal number,
/// providing type-safe access through signal number indexing.
#[derive(Clone)]
pub struct SignalActions(pub(crate) [SignalAction; MAX_SIGNALS]);

impl Default for SignalActions {
    fn default() -> Self {
        Self(array::from_fn(|_| SignalAction::default()))
    }
}

impl SignalActions {
    pub(super) fn ignored_set(&self, mut pending: SignalSet) -> SignalSet {
        let mut ignored = SignalSet::default();
        while let Some(signo) = pending.dequeue(&!SignalSet::default()) {
            if signal_ignored_by(self, signo) {
                ignored.add(signo);
            }
        }
        ignored
    }

    fn reset_for_exec(&mut self) {
        for action in &mut self.0 {
            let disposition = if matches!(action.disposition, SignalDisposition::Ignore) {
                SignalDisposition::Ignore
            } else {
                SignalDisposition::Default
            };
            *action = SignalAction {
                disposition,
                ..Default::default()
            };
        }
    }
}

impl Index<Signo> for SignalActions {
    type Output = SignalAction;

    fn index(&self, signo: Signo) -> &SignalAction {
        &self.0[signo as usize - 1]
    }
}

impl IndexMut<Signo> for SignalActions {
    fn index_mut(&mut self, signo: Signo) -> &mut SignalAction {
        &mut self.0[signo as usize - 1]
    }
}

/// Signal handling configuration shared by processes using CLONE_SIGHAND.
/// The reference count tracks process ownership, not temporary query borrows.
pub(super) struct SignalHandlers {
    pub(super) actions: SpinNoIrq<SignalActions>,
    pub(super) pending_changed: kpoll::PollSet,
}

impl SignalHandlers {
    fn new(actions: SignalActions) -> Self {
        Self {
            actions: SpinNoIrq::new(actions),
            pending_changed: kpoll::PollSet::new(),
        }
    }
}

/// Manages signal handling at the process level.
///
/// This manager coordinates signal delivery between the process and its threads,
/// maintains signal actions, and handles process-wide pending signals.
pub struct ProcessSignalManager {
    /// The only critical section that answers signal deliverability: this
    /// process's directed queue together with every member thread's private
    /// queue, blocked set and saved sigmask. Holding them under one lock is
    /// what makes `pending ∩ ¬blocked` observable atomically; splitting them
    /// lets a sender and a receiver each see a consistent-but-stale half.
    siglock: SigLock,

    /// This process's current configuration; locks before SignalHandlers.actions.
    /// Serializes fork/exec ownership changes without adding query owners.
    pub(super) handlers: SpinNoIrq<Option<Arc<SignalHandlers>>>,

    /// Default signal handler restore function
    pub(crate) default_restorer: usize,

    /// Packs the fatal-signal counter (high 32 bits) and the signal number of
    /// the last accepted fatal signal (low 32 bits) into one word.
    ///
    /// `exec_de_thread` snapshots it and aborts the exec if the counter changed
    /// during the sibling sweep, so a genuine fatal signal delivered to an
    /// already-marked sweep target still terminates the process instead of being
    /// absorbed. Packing both halves means a reader can never pair an old epoch
    /// with a newer signal.
    fatal_signal: AtomicU64,
}

/// Prepared Linux-style `SIGCHLD` child-exit notification.
#[derive(Debug)]
pub struct PreparedChildExitSignal {
    sig: SigchldChildExitSignalInfo,
    autoreap: bool,
    queue_signal: bool,
}

impl PreparedChildExitSignal {
    /// Returns whether the exited child should skip the waitable zombie state.
    pub fn should_autoreap(&self) -> bool {
        self.autoreap
    }
}

/// Outcome of an immediately sent child-exit notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildExitSignalOutcome {
    autoreap: bool,
}

impl ChildExitSignalOutcome {
    /// Returns whether the exited child should skip the waitable zombie state.
    pub fn should_autoreap(&self) -> bool {
        self.autoreap
    }
}

impl ProcessSignalManager {
    /// Creates a new process signal manager.
    /// Allocates owned state; call during task-context process initialization.
    ///
    /// # Arguments
    /// * `actions` - Initial signal actions, owned by this new process
    /// * `default_restorer` - Default signal handler restore function address
    pub fn new(actions: SignalActions, default_restorer: usize) -> Self {
        Self::with_handlers(Arc::new(SignalHandlers::new(actions)), default_restorer)
    }

    fn with_handlers(handlers: Arc<SignalHandlers>, default_restorer: usize) -> Self {
        Self {
            siglock: SigLock::new(),
            handlers: SpinNoIrq::new(Some(handlers)),
            default_restorer,
            fatal_signal: AtomicU64::new(0),
        }
    }

    /// Borrows the signal-state lock shared with this process's threads.
    pub(crate) fn siglock(&self) -> &SigLock {
        &self.siglock
    }

    /// Creates a new process manager sharing this process's action configuration.
    /// Arrival notifications are shared; pending signals and membership stay separate.
    /// Call during task-context clone preparation, before child publication.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn fork_shared(&self) -> KResult<Self> {
        let handlers = self
            .handlers
            .lock()
            .as_ref()
            .ok_or(KError::NoSuchProcess)?
            .clone();
        Ok(Self::with_handlers(handlers, self.default_restorer))
    }

    /// Creates a new process manager with an independent snapshot of all actions.
    /// Pending signals, arrival notifications and thread membership start empty.
    /// Call during task-context clone preparation, before child publication.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn fork_private(&self) -> KResult<Self> {
        let actions = {
            let handlers = self.handlers.lock();
            handlers
                .as_ref()
                .ok_or(KError::NoSuchProcess)?
                .actions
                .lock()
                .clone()
        };
        Ok(Self::new(actions, self.default_restorer))
    }

    /// Creates a new process manager whose actions start cleared for
    /// `clone3(CLONE_CLEAR_SIGHAND)`.
    ///
    /// Like [`Self::fork_private`], but every handler not set to ignore is
    /// reset to its default disposition and all flags, masks and restorers
    /// are cleared (Linux `copy_sighand` + `flush_signal_handlers`). Ignored
    /// dispositions survive, matching the exec reset rule. Call during
    /// task-context clone preparation, before child publication.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn fork_cleared(&self) -> KResult<Self> {
        let mut actions = {
            let handlers = self.handlers.lock();
            handlers
                .as_ref()
                .ok_or(KError::NoSuchProcess)?
                .actions
                .lock()
                .clone()
        };
        actions.reset_for_exec();
        Ok(Self::new(actions, self.default_restorer))
    }

    /// Captures the current arrival source without retaining configuration ownership.
    /// The weak source never rebinds after exec and may expire after capture.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn pending_source(&self) -> KResult<kpoll::WeakPollSet> {
        let owner = self.handlers.lock();
        let handlers = owner.as_ref().ok_or(KError::NoSuchProcess)?;
        Ok(handlers.pending_changed.downgrade())
    }

    /// Requests a pending-state recheck on the current shared arrival source.
    /// Does not enqueue a signal or imply that any reader has matching pending data.
    /// Call outside signal locks; callbacks run synchronously and must not block.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn notify_pending(&self) -> KResult<()> {
        let source = {
            let owner = self.handlers.lock();
            let handlers = owner.as_ref().ok_or(KError::NoSuchProcess)?;
            handlers.pending_changed.clone()
        };
        source.wake();
        Ok(())
    }

    /// Releases this process's configuration ownership after its last thread exits.
    /// Idempotent; retained manager references cannot reacquire ownership.
    /// Call outside process-domain, membership and signal locks. Destruction of
    /// the shared configuration occurs after releasing the ownership lock.
    pub fn release_handlers(&self) {
        let previous = self.handlers.lock().take();
        drop(previous);
    }

    /// Resets actions for exec without changing another process's configuration.
    ///
    /// Retains ignore dispositions but clears every action's flags, mask and
    /// restorer. Pending signals and thread masks are unchanged. Call in task
    /// context: a shared configuration requires allocation outside signal locks.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn reset_actions_for_exec(&self) -> KResult<()> {
        {
            let owner = self.handlers.lock();
            let owner = owner.as_ref().ok_or(KError::NoSuchProcess)?;
            if Arc::strong_count(owner) == 1 {
                owner.actions.lock().reset_for_exec();
                return Ok(());
            }
        }

        let mut replacement = Arc::new(SignalHandlers::new(SignalActions::default()));
        let previous = {
            let mut owner = self.handlers.lock();
            let handlers = owner.as_ref().ok_or(KError::NoSuchProcess)?;
            // Fork must take this same ownership lock; a sole owner cannot
            // become shared between this check and the in-place reset.
            if Arc::strong_count(handlers) == 1 {
                handlers.actions.lock().reset_for_exec();
                return Ok(());
            }
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: replacement was created above and \
                          is not shared until owner.replace"
            )]
            let actions = Arc::get_mut(&mut replacement)
                .expect("unpublished action table has one owner")
                .actions
                .get_mut();
            *actions = handlers.actions.lock().clone();
            actions.reset_for_exec();
            owner.replace(replacement)
        };
        // A concurrent peer may have released its ownership after our count check.
        drop(previous);
        Ok(())
    }

    /// Checks if a signal is ignored by the process.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn signal_ignored(&self, signo: Signo) -> KResult<bool> {
        let handlers = self.handlers.lock();
        let handlers = handlers.as_ref().ok_or(KError::NoSuchProcess)?;
        Ok(signal_ignored_by(&handlers.actions.lock(), signo))
    }

    /// Checks if syscalls interrupted by the given signal can be restarted.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn can_restart(&self, signo: Signo) -> KResult<bool> {
        let handlers = self.handlers.lock();
        let handlers = handlers.as_ref().ok_or(KError::NoSuchProcess)?;
        Ok(handlers.actions.lock()[signo]
            .flags
            .contains(SignalActionFlags::RESTART))
    }

    /// Returns the configured action for the given signal.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn signal_action(&self, signo: Signo) -> KResult<SignalAction> {
        let handlers = self.handlers.lock();
        let handlers = handlers.as_ref().ok_or(KError::NoSuchProcess)?;
        Ok(handlers.actions.lock()[signo].clone())
    }

    /// Replaces the configured action for the given signal and returns the old one.
    /// Serializes with process/thread send_signal admission in every process
    /// sharing this action table. Prepared child-exit publication is separate.
    /// Ignoring flushes this group's queues, retaining valid periodic records;
    /// restoring a disposition requeues them without allocating. Peer groups
    /// sharing the action table are not flushed. Releases and wakes run unlocked.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn replace_signal_action(
        &self,
        signo: Signo,
        action: Option<SignalAction>,
    ) -> KResult<SignalAction> {
        let owner = self.handlers.lock();
        let handlers = owner.as_ref().ok_or(KError::NoSuchProcess)?;
        let mut actions = handlers.actions.lock();
        let mut discarded = crate::pending::DiscardedNotifications::default();
        let old = actions[signo].clone();
        let mut has_replayed = false;
        if let Some(action) = action {
            actions[signo] = action;
            has_replayed = self.siglock.transact(|txn| {
                let has_replayed = txn.change_disposition(
                    signo,
                    signal_ignored_by(&actions, signo),
                    &mut discarded,
                );
                if has_replayed
                    && signal_is_fatal(&actions, signo)
                    && txn.view().has_deliverable_signo(signo)
                {
                    self.note_fatal_signal(signo);
                }
                has_replayed
            });
        }
        let source = handlers.pending_changed.clone();
        drop(actions);
        drop(owner);
        drop(discarded);
        if has_replayed {
            crate::notification::wake_ready_records();
            source.wake();
        }
        Ok(old)
    }

    /// Replaces the configured action for the given signal.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn set_signal_action(&self, signo: Signo, action: SignalAction) -> KResult<()> {
        self.replace_signal_action(signo, Some(action))?;
        Ok(())
    }

    /// Rechecks disposition, pending state and target selection before a deferred wake.
    pub(crate) fn pending_timer_wake_tid(
        &self,
        slot: Option<crate::SlotId>,
        signo: Signo,
    ) -> Option<u32> {
        let owner = self.handlers.lock();
        let handlers = owner.as_ref()?;
        let actions = handlers.actions.lock();
        if signal_ignored_by(&actions, signo) {
            return None;
        }
        self.siglock.inspect(|view| {
            if let Some(slot) = slot {
                (view.pending(slot).has(signo) && !view.blocked_has(slot, signo))
                    .then(|| view.target_tid(slot))
                    .flatten()
            } else {
                view.shared_pending()
                    .has(signo)
                    .then(|| view.find_target(signo).0)
                    .flatten()
            }
        })
    }

    /// Sends a signal to the process.
    ///
    /// This method handles process-level signal delivery, checking if the signal
    /// should be ignored and finding an appropriate thread to handle it.
    ///
    /// # Arguments
    /// * `sig` - Signal information to send
    ///
    /// # Returns
    /// `Some(tid)` if a specific thread should be interrupted, `None` otherwise.
    /// After configuration ownership is released, drops the signal and returns `None`.
    ///
    /// Ordinary ignored signals are dropped only when no live thread currently blocks
    /// them. If a signal is blocked, it remains pending because userspace may
    /// install a handler before unblocking it. The implicitly ignored set
    /// covers default dispositions whose action is a no-op for a running
    /// process (Linux `SIG_KERNEL_IGNORE_MASK`), including default `SIGCONT`.
    ///
    /// Ordinary SignalInfo carries no timer completion authority. The kernel
    /// timer bridge may instead supply a typed SignalNotification.
    /// Ignored periodic reservations remain retained without a wake; one-shot
    /// reservations are discarded. Action or blocked-mask changes can replay
    /// retained records without allocating notification storage.
    /// Unqueued notifications are released after signal and configuration locks;
    /// this does not complete a timer receipt or change the wake hint on coalescing.
    ///
    /// # Errors
    /// Returns WouldBlock for ordinary non-SI_USER realtime owner exhaustion.
    /// Failure publishes neither a pending signal, fatal epoch nor a wake.
    pub fn send_signal(&self, sig: impl Into<SignalNotification>) -> KResult<Option<u32>> {
        let outcome = self.send_signal_inner(sig.into(), false);
        outcome.admission_result()?;
        Ok(outcome.wake_tid())
    }

    pub(crate) fn send_signal_inner(
        &self,
        sig: SignalNotification,
        require_live: bool,
    ) -> TimerSignalSendOutcome {
        self.send_signal_with_allocator(sig, require_live, |node| {
            kalloc_types::KArc::new_nowait(node).ok()
        })
    }

    pub(crate) fn send_signal_with_allocator(
        &self,
        sig: SignalNotification,
        require_live: bool,
        allocate: impl FnOnce(
            crate::notification::NotificationNode,
        )
            -> Option<kalloc_types::KArc<crate::notification::NotificationNode>>,
    ) -> TimerSignalSendOutcome {
        let signo = sig.signo();
        let owner = self.handlers.lock();
        let Some(handlers) = owner.as_ref() else {
            drop(owner);
            return TimerSignalSendOutcome::rejected(sig, TimerSignalRejection::TargetGone);
        };
        let actions = handlers.actions.lock();
        let ignored = signal_ignored_by(&actions, signo);
        let fatal = signal_is_fatal(&actions, signo);

        // Target selection and queueing are one transaction, so a target
        // cannot change its blocked set between selection and queueing, and
        // the arrival projection is derived from the queue that was just
        // extended rather than published separately.
        let (should_notify, outcome) = self.siglock.transact(|txn| {
            if sig.is_invalidated() {
                return (
                    false,
                    TimerSignalSendOutcome::rejected(sig, TimerSignalRejection::Invalidated),
                );
            }
            let (target_tid, has_blocked_thread) = txn.view().find_target(signo);
            if require_live && target_tid.is_none() && !has_blocked_thread {
                return (
                    false,
                    TimerSignalSendOutcome::rejected(sig, TimerSignalRejection::TargetGone),
                );
            }
            if ignored && !has_blocked_thread {
                return (false, txn.retain_ignored(None, sig));
            }
            let Ok(rejected) = txn.put_shared_with_allocator(sig, allocate) else {
                return (false, TimerSignalSendOutcome::QueueFull);
            };
            if fatal && target_tid.is_some() {
                self.note_fatal_signal(signo);
            }
            (
                true,
                TimerSignalSendOutcome::from_queue(
                    rejected,
                    if ignored { None } else { target_tid },
                ),
            )
        });

        let source = handlers.pending_changed.clone();
        drop(actions);
        drop(owner);
        if should_notify {
            source.wake();
        }
        outcome
    }
}

impl Drop for ProcessSignalManager {
    fn drop(&mut self) {
        self.release_handlers();
    }
}

pub(super) fn signal_ignored_by(actions: &SignalActions, signo: Signo) -> bool {
    match &actions[signo].disposition {
        SignalDisposition::Ignore => true,
        SignalDisposition::Default => {
            // Mirrors Linux's SIG_KERNEL_IGNORE_MASK {SIGCONT, SIGCHLD,
            // SIGWINCH, SIGURG}: SIGCONT classifies as `Continue` rather than
            // `Ignore`, but its default action only resumes a stopped process
            // and has no effect on a running one. Treating the default
            // disposition as ignored keeps a default SIGCONT from being queued
            // and from waking interruptible waiters; otherwise a timer- or
            // kill-sent SIGCONT spuriously aborts nanosleep- and poll-style
            // waits that Linux lets run to completion.
            matches!(
                signo.default_action(),
                DefaultSignalAction::Ignore | DefaultSignalAction::Continue
            )
        }
        _ => false,
    }
}

impl ProcessSignalManager {
    /// Prepares a child-exit `SIGCHLD` notification decision.
    ///
    /// Child-exit notification follows Linux `do_notify_parent()` semantics.
    /// The default ignored disposition of `SIGCHLD` does not suppress queuing.
    /// Explicit `SIG_IGN` suppresses the signal and requests autoreap.
    /// `SA_NOCLDWAIT` also requests autoreap, while Linux still queues
    /// `SIGCHLD` unless the disposition is explicit `SIG_IGN`.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn prepare_child_exit_signal(
        &self,
        sig: SigchldChildExitSignalInfo,
    ) -> KResult<PreparedChildExitSignal> {
        let action = self.signal_action(Signo::SIGCHLD)?;
        let explicit_ignore = matches!(action.disposition, SignalDisposition::Ignore);
        let autoreap = explicit_ignore || action.flags.contains(SignalActionFlags::NOCLDWAIT);

        if explicit_ignore {
            return Ok(PreparedChildExitSignal {
                sig,
                autoreap,
                queue_signal: false,
            });
        }

        Ok(PreparedChildExitSignal {
            sig,
            autoreap,
            queue_signal: true,
        })
    }

    /// Commits a prepared child-exit `SIGCHLD` notification and returns the
    /// current unblocked target thread, if one should be interrupted.
    /// Drops the notification if configuration ownership has been released.
    pub fn commit_child_exit_signal(&self, prepared: PreparedChildExitSignal) -> Option<u32> {
        let owner = self.handlers.lock();
        let handlers = owner.as_ref()?;
        if prepared.queue_signal {
            let (target_tid, rejected) = self.siglock.transact(|txn| {
                let rejected = txn.put_shared(prepared.sig.into_signal_info());
                (txn.view().find_target(Signo::SIGCHLD).0, rejected)
            });
            let source = handlers.pending_changed.clone();
            drop(owner);
            let _rejected = rejected;
            source.wake();
            target_tid
        } else {
            None
        }
    }

    /// Sends child-exit `SIGCHLD` notification immediately.
    ///
    /// Process-exit code that must publish exit/autoreap state before SIGCHLD
    /// becomes observable should use [`Self::prepare_child_exit_signal`] and
    /// [`Self::commit_child_exit_signal`] instead.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn send_child_exit_signal(
        &self,
        sig: SigchldChildExitSignalInfo,
    ) -> KResult<(ChildExitSignalOutcome, Option<u32>)> {
        let prepared = self.prepare_child_exit_signal(sig)?;
        let outcome = ChildExitSignalOutcome {
            autoreap: prepared.should_autoreap(),
        };
        let target_tid = self.commit_child_exit_signal(prepared);
        Ok((outcome, target_tid))
    }
}

/// Returns whether `signo`'s current disposition would terminate the process.
///
/// Mirrors Linux's `complete_signal` fatal branch: only a default
/// terminate/core disposition is fatal; an ignored or user-handled signal is
/// not.
pub(super) fn signal_is_fatal(actions: &SignalActions, signo: Signo) -> bool {
    match actions[signo].disposition {
        SignalDisposition::Ignore | SignalDisposition::Handler(_) => false,
        SignalDisposition::Default => matches!(
            signo.default_action(),
            DefaultSignalAction::Terminate | DefaultSignalAction::CoreDump
        ),
    }
}

impl ProcessSignalManager {
    /// Records that a fatal signal was accepted for delivery.
    ///
    /// The counter is bumped and `signo` stored in one atomic update, so a
    /// reader never observes the new epoch with an older signal number.
    pub fn note_fatal_signal(&self, signo: Signo) {
        let mut current = self.fatal_signal.load(Ordering::Acquire);
        loop {
            let epoch = ((current >> 32) as u32).wrapping_add(1);
            let next = (u64::from(epoch) << 32) | u64::from(signo as u32);
            match self.fatal_signal.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    /// Returns one snapshot of the fatal-signal counter and last signal number.
    ///
    /// `exec_de_thread` takes it around the sibling sweep; a counter change
    /// means a genuine fatal signal raced the sweep and must win over the exec.
    pub fn fatal_signal_snapshot(&self) -> (u32, u32) {
        let raw = self.fatal_signal.load(Ordering::Acquire);
        ((raw >> 32) as u32, raw as u32)
    }

    /// Returns the signal dispositions that are ignored and caught.
    ///
    /// The two masks are captured under one actions lock, so they describe the
    /// same disposition snapshot. Returns empty masks after configuration
    /// ownership is released, matching the `/proc` fields of a task that can no
    /// longer install handlers.
    pub fn disposition_masks(&self) -> (SignalSet, SignalSet) {
        let owner = self.handlers.lock();
        let Some(handlers) = owner.as_ref() else {
            return (SignalSet::default(), SignalSet::default());
        };
        let actions = handlers.actions.lock();
        let mut ignored = SignalSet::default();
        let mut caught = SignalSet::default();
        for signo in Signo::iter() {
            match &actions[signo].disposition {
                SignalDisposition::Ignore => {
                    ignored.add(signo);
                }
                SignalDisposition::Handler(_) => {
                    caught.add(signo);
                }
                SignalDisposition::Default => {}
            }
        }
        (ignored, caught)
    }

    /// Gets process-directed (shared) pending signals.
    pub fn pending(&self) -> SignalSet {
        self.siglock.inspect(|view| view.shared_pending())
    }
}

unittest::path_mod_test!(handler_tests, "../../ut-tests/process_handlers.rs");
