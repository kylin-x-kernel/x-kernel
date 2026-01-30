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
    DefaultSignalAction, MAX_SIGNALS, PendingSignals, SignalAction, SignalActionFlags,
    SignalDisposition, SignalInfo, SignalSet, Signo, api::ThreadSignalManager,
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
        let mut pending_guard = self.pending.lock();
        let signal = pending_guard.dequeue_signal(mask);

        // Update fast path indicator
        if pending_guard.set.is_empty() {
            self.has_pending.store(false, Ordering::Release);
        }

        signal
    }

    /// Checks if a signal is ignored by the process.
    pub fn signal_ignored(&self, signo: Signo) -> bool {
        match &self.actions.lock()[signo].disposition {
            SignalDisposition::Ignore => true,
            SignalDisposition::Default => {
                matches!(signo.default_action(), DefaultSignalAction::Ignore)
            }
            _ => false,
        }
    }

    /// Checks if syscalls interrupted by the given signal can be restarted.
    pub fn can_restart(&self, signo: Signo) -> bool {
        self.actions.lock()[signo]
            .flags
            .contains(SignalActionFlags::RESTART)
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
    /// `Some(tid)` if a specific thread should handle the signal, `None` otherwise
    #[must_use]
    pub fn send_signal(&self, sig: SignalInfo) -> Option<u32> {
        let signo = sig.signo();

        // Check if signal should be ignored
        if self.signal_ignored(signo) {
            return None;
        }

        // Add to pending signals
        if self.pending.lock().put_signal(sig) {
            self.has_pending.store(true, Ordering::Release);
        }

        // Find a thread that can handle this signal
        self.find_target_thread(signo)
    }

    /// Finds a suitable thread to handle the given signal.
    fn find_target_thread(&self, signo: Signo) -> Option<u32> {
        let mut target_tid = None;

        self.children.lock().retain(|(tid, thread_weak)| {
            if let Some(thread) = thread_weak.upgrade() {
                if target_tid.is_none() && !thread.signal_blocked(signo) {
                    target_tid = Some(*tid);
                }
                true // Keep this thread reference
            } else {
                false // Remove dead thread reference
            }
        });

        target_tid
    }

    /// Gets currently pending signals.
    pub fn pending(&self) -> SignalSet {
        self.pending.lock().set
    }
}
