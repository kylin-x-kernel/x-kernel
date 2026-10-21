// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-level signal state and delivery.
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    array,
    ops::{Index, IndexMut},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use kerrno::{KError, KResult};
use kspin::SpinNoIrq;

use crate::{
    DefaultSignalAction, MAX_SIGNALS, PendingSignals, SigchldChildExitSignalInfo, SignalAction,
    SignalActionFlags, SignalDisposition, SignalInfo, SignalSet, Signo,
    api::{SignalDequeueAction, ThreadSignalManager, notify_signal_dequeued},
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
    /// Process-level pending signals queue
    pending: SpinNoIrq<PendingSignals>,

    /// This process's current configuration; locks before SignalHandlers.actions.
    /// Serializes fork/exec ownership changes without adding query owners.
    pub(super) handlers: SpinNoIrq<Option<Arc<SignalHandlers>>>,

    /// Default signal handler restore function
    pub(crate) default_restorer: usize,

    /// Thread signal managers for signal distribution
    pub(crate) children: SpinNoIrq<Vec<(u32, Weak<ThreadSignalManager>)>>,

    /// Fast path indicator for pending signals
    pub(crate) has_pending: AtomicBool,

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
            pending: SpinNoIrq::new(PendingSignals::default()),
            handlers: SpinNoIrq::new(Some(handlers)),
            default_restorer,
            children: SpinNoIrq::new(Vec::new()),
            has_pending: AtomicBool::new(false),
            fatal_signal: AtomicU64::new(0),
        }
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

    /// Dequeues the next pending signal that matches the given mask.
    ///
    /// # Arguments
    /// * `mask` - Signal mask to filter available signals
    ///
    /// # Returns
    /// The next available signal info, if any
    pub(crate) fn dequeue_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        loop {
            let signal = {
                let mut pending_guard = self.pending.lock();
                let signal = pending_guard.dequeue_signal(mask);
                if pending_guard.set.is_empty() {
                    self.has_pending.store(false, Ordering::Release);
                }
                signal
            };

            let sig = signal?;

            if notify_signal_dequeued(&sig) == SignalDequeueAction::Deliver {
                return Some(sig);
            }
        }
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
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn replace_signal_action(
        &self,
        signo: Signo,
        action: Option<SignalAction>,
    ) -> KResult<SignalAction> {
        let handlers = self.handlers.lock();
        let handlers = handlers.as_ref().ok_or(KError::NoSuchProcess)?;
        let mut actions = handlers.actions.lock();
        let old = actions[signo].clone();
        if let Some(action) = action {
            actions[signo] = action;
        }
        Ok(old)
    }

    /// Replaces the configured action for the given signal.
    /// Returns `NoSuchProcess` after configuration ownership is released.
    pub fn set_signal_action(&self, signo: Signo, action: SignalAction) -> KResult<()> {
        let handlers = self.handlers.lock();
        let handlers = handlers.as_ref().ok_or(KError::NoSuchProcess)?;
        handlers.actions.lock()[signo] = action;
        Ok(())
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
    /// Ignored signals are dropped only when no live thread currently blocks
    /// them. If a signal is blocked, it remains pending because userspace may
    /// install a handler before unblocking it. The implicitly ignored set
    /// covers default dispositions whose action is a no-op for a running
    /// process (Linux `SIG_KERNEL_IGNORE_MASK`), including default `SIGCONT`.
    #[must_use]
    pub fn send_signal(&self, sig: SignalInfo) -> Option<u32> {
        let signo = sig.signo();
        let owner = self.handlers.lock();
        let handlers = owner.as_ref()?;
        let (ignored, fatal, target_tid, has_blocked_thread) = {
            let actions = handlers.actions.lock();

            // Keep the disposition snapshot and target-thread scan in one
            // generation decision. This follows Linux's prepare_signal() /
            // complete_signal() structure without claiming the same global
            // sighand->siglock consistency for every signal-related field.
            let (target_tid, has_blocked_thread) = self.find_target_thread(signo);
            (
                signal_ignored_by(&actions, signo),
                signal_is_fatal(&actions, signo),
                target_tid,
                has_blocked_thread,
            )
        };

        if ignored && !has_blocked_thread {
            return None;
        }
        // A fatal group signal terminates the whole process when some thread can
        // actually take it; record it before the queue so a concurrent `exec`
        // sibling sweep cannot absorb it. A signal every thread blocks stays
        // pending (and survives `execve`) without marking the process as dying.
        if fatal && target_tid.is_some() {
            self.note_fatal_signal(signo);
        }
        self.put_pending_signal(sig);
        let source = handlers.pending_changed.clone();
        drop(owner);
        source.wake();
        if ignored { None } else { target_tid }
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
            self.put_pending_signal(prepared.sig.into_signal_info());
            let (target_tid, _) = self.find_target_thread(Signo::SIGCHLD);
            let source = handlers.pending_changed.clone();
            drop(owner);
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

    fn put_pending_signal(&self, sig: SignalInfo) {
        if self.pending.lock().put_signal(sig) {
            self.has_pending.store(true, Ordering::Release);
        }
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

    /// Finds a suitable thread and reports whether any live thread blocks it.
    fn find_target_thread(&self, signo: Signo) -> (Option<u32>, bool) {
        let mut target_tid = None;
        let mut has_blocked_thread = false;

        self.children.lock().retain(|(tid, thread_weak)| {
            if let Some(thread) = thread_weak.upgrade() {
                if thread.signal_blocked(signo) {
                    has_blocked_thread = true;
                } else if target_tid.is_none() {
                    target_tid = Some(*tid);
                }
                true // Keep this thread reference
            } else {
                false // Remove dead thread reference
            }
        });

        (target_tid, has_blocked_thread)
    }

    /// Gets currently pending signals.
    pub fn pending(&self) -> SignalSet {
        self.pending.lock().set
    }
}

unittest::path_mod_test!(handler_tests, "../../ut-tests/process_handlers.rs");
