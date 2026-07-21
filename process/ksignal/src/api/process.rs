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
    sync::atomic::{AtomicBool, Ordering},
};

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

/// Manages signal handling at the process level.
///
/// This manager coordinates signal delivery between the process and its threads,
/// maintains signal actions, and handles process-wide pending signals.
pub struct ProcessSignalManager {
    /// Process-level pending signals queue
    pending: SpinNoIrq<PendingSignals>,

    /// Shared signal action handlers
    pub actions: Arc<SpinNoIrq<SignalActions>>,

    /// Default signal handler restore function
    pub(crate) default_restorer: usize,

    /// Thread signal managers for signal distribution
    pub(crate) children: SpinNoIrq<Vec<(u32, Weak<ThreadSignalManager>)>>,

    /// Fast path indicator for pending signals
    pub(crate) has_pending: AtomicBool,
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
    ///
    /// # Arguments
    /// * `actions` - Shared signal actions configuration
    /// * `default_restorer` - Default signal handler restore function address
    pub fn new(actions: Arc<SpinNoIrq<SignalActions>>, default_restorer: usize) -> Self {
        Self {
            pending: SpinNoIrq::new(PendingSignals::default()),
            actions,
            default_restorer,
            children: SpinNoIrq::new(Vec::new()),
            has_pending: AtomicBool::new(false),
        }
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
    pub fn signal_ignored(&self, signo: Signo) -> bool {
        signal_ignored_by(&self.actions.lock(), signo)
    }

    /// Checks if syscalls interrupted by the given signal can be restarted.
    pub fn can_restart(&self, signo: Signo) -> bool {
        self.actions.lock()[signo]
            .flags
            .contains(SignalActionFlags::RESTART)
    }

    /// Returns the configured action for the given signal.
    pub fn signal_action(&self, signo: Signo) -> SignalAction {
        self.actions.lock()[signo].clone()
    }

    /// Replaces the configured action for the given signal and returns the old one.
    pub fn replace_signal_action(
        &self,
        signo: Signo,
        action: Option<SignalAction>,
    ) -> SignalAction {
        let mut actions = self.actions.lock();
        let old = actions[signo].clone();
        if let Some(action) = action {
            actions[signo] = action;
        }
        old
    }

    /// Replaces the configured action for the given signal.
    pub fn set_signal_action(&self, signo: Signo, action: SignalAction) {
        self.actions.lock()[signo] = action;
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
    ///
    /// Ignored signals are dropped only when no live thread currently blocks
    /// them. If a signal is blocked, it remains pending because userspace may
    /// install a handler before unblocking it.
    #[must_use]
    pub fn send_signal(&self, sig: SignalInfo) -> Option<u32> {
        let signo = sig.signo();
        let (ignored, target_tid, has_blocked_thread) = {
            let actions = self.actions.lock();

            // Keep the disposition snapshot and target-thread scan in one
            // generation decision. This follows Linux's prepare_signal() /
            // complete_signal() structure without claiming the same global
            // sighand->siglock consistency for every signal-related field.
            let (target_tid, has_blocked_thread) = self.find_target_thread(signo);
            (
                signal_ignored_by(&actions, signo),
                target_tid,
                has_blocked_thread,
            )
        };

        if ignored {
            if !has_blocked_thread {
                return None;
            }

            // Linux does not drop ignored signals while they are blocked:
            // userspace may install a handler before unblocking them.
            self.put_pending_signal(sig);
            return None;
        }

        self.put_pending_signal(sig);
        target_tid
    }
}

pub(super) fn signal_ignored_by(actions: &SignalActions, signo: Signo) -> bool {
    match &actions[signo].disposition {
        SignalDisposition::Ignore => true,
        SignalDisposition::Default => {
            matches!(signo.default_action(), DefaultSignalAction::Ignore)
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
    #[must_use]
    pub fn prepare_child_exit_signal(
        &self,
        sig: SigchldChildExitSignalInfo,
    ) -> PreparedChildExitSignal {
        let action = self.signal_action(Signo::SIGCHLD);
        let explicit_ignore = matches!(action.disposition, SignalDisposition::Ignore);
        let autoreap = explicit_ignore || action.flags.contains(SignalActionFlags::NOCLDWAIT);

        if explicit_ignore {
            return PreparedChildExitSignal {
                sig,
                autoreap,
                queue_signal: false,
            };
        }

        PreparedChildExitSignal {
            sig,
            autoreap,
            queue_signal: true,
        }
    }

    /// Commits a prepared child-exit `SIGCHLD` notification and returns the
    /// current unblocked target thread, if one should be interrupted.
    pub fn commit_child_exit_signal(&self, prepared: PreparedChildExitSignal) -> Option<u32> {
        if prepared.queue_signal {
            self.put_pending_signal(prepared.sig.into_signal_info());
            let (target_tid, _) = self.find_target_thread(Signo::SIGCHLD);
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
    #[must_use]
    pub fn send_child_exit_signal(
        &self,
        sig: SigchldChildExitSignalInfo,
    ) -> (ChildExitSignalOutcome, Option<u32>) {
        let prepared = self.prepare_child_exit_signal(sig);
        let outcome = ChildExitSignalOutcome {
            autoreap: prepared.should_autoreap(),
        };
        let target_tid = self.commit_child_exit_signal(prepared);
        (outcome, target_tid)
    }

    fn put_pending_signal(&self, sig: SignalInfo) {
        if self.pending.lock().put_signal(sig) {
            self.has_pending.store(true, Ordering::Release);
        }
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
