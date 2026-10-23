// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread-level signal handling and user context setup.
use alloc::sync::Arc;
use core::{
    alloc::Layout,
    mem::offset_of,
    sync::atomic::{AtomicBool, Ordering},
};

use kcpu::userspace::{UserContext, UserRestorableContext};
use kerrno::{KResult, LinuxError};
use kspin::SpinNoIrq;
use osvm::VirtMutPtr;

use super::ProcessSignalManager;
use crate::{
    DefaultSignalAction, Dequeued, SignalAction, SignalActionFlags, SignalDisposition, SignalInfo,
    SignalOsAction, SignalSet, SignalStack, Signo, SlotId,
    api::{SignalDequeueAction, notify_signal_dequeued},
    arch::{self, UContext},
};

struct SignalFrame {
    ucontext: UContext,
    siginfo: SignalInfo,
    saved: UserRestorableContext,
}

// The x86_64 restorable context holds an FXSAVE block that `restore` feeds to
// `FXRSTOR`, which requires 16-byte alignment. Rust's field layout keeps
// `saved` naturally aligned inside the frame; this guards that invariant.
const _: () = assert!(
    core::mem::offset_of!(SignalFrame, saved) % core::mem::align_of::<UserRestorableContext>() == 0,
    "signal-frame restorable state must be naturally aligned"
);

/// Thread-level signal manager.
///
/// Owns this thread's handle into the process signal state and the alternate
/// signal stack. Pending signals, the blocked mask and the saved sigmask live
/// in the owning process's `SigLock`, because deliverability is
/// `pending ∩ ¬blocked` and must be decided under a single lock. Handler-frame
/// construction happens here on the user-return path, outside that lock, from
/// the snapshot a dequeue produced.
pub struct ThreadSignalManager {
    /// The process-level signal manager
    proc: Arc<ProcessSignalManager>,

    /// This thread's slot in the process signal state. Retiring or rebinding
    /// the slot is generation-checked, so a retained manager reference can
    /// never address a successor thread's state.
    slot: SlotId,
    /// The stack used by signal handlers
    stack: SpinNoIrq<SignalStack>,

    /// Projection of this thread's deliverability. Read lock-free on the
    /// user-return fast path; written only when the owning lock republishes
    /// state, never by this manager.
    hint: Arc<AtomicBool>,
}

impl ThreadSignalManager {
    /// Create a new thread signal manager attached to a process.
    pub fn new(tid: u32, proc: Arc<ProcessSignalManager>) -> Arc<Self> {
        let (slot, hint) = proc.siglock().register(tid);
        Arc::new(Self {
            proc,
            slot,
            stack: SpinNoIrq::new(SignalStack::default()),
            hint,
        })
    }

    /// Rebinds this thread's delivery target after an `exec` identity takeover.
    ///
    /// Routing selects threads by number, so the slot follows the moved
    /// number; other slots retain their own identities and lifecycle. The slot
    /// handle is generation-checked, so a reused TID cannot rebind a
    /// successor's slot.
    pub fn rebind_after_exec(self: &Arc<Self>, tid: u32) {
        self.proc
            .siglock()
            .transact(|txn| txn.set_tid(self.slot, tid));
    }

    /// Removes this thread from process-directed signal selection at exit.
    ///
    /// Call before retiring thread-group membership. A retained manager
    /// reference must not keep an exited thread eligible for delivery. The
    /// removal is idempotent and identity-compared, so a reused TID cannot
    /// remove a successor.
    pub fn exit(self: &Arc<Self>) {
        self.proc
            .siglock()
            .transact(|txn| txn.leave_group(self.slot));
    }

    /// Dequeues the next signal matching an explicit mask.
    ///
    /// The mask is supplied by the caller (`signalfd`, `sigtimedwait`) rather
    /// than derived from this thread, so this is the only dequeue entry point
    /// that takes one. The availability projection is recomputed from the
    /// post-dequeue state inside the same transaction.
    #[must_use]
    pub fn dequeue_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        loop {
            let dequeued = self
                .proc
                .siglock()
                .transact(|txn| txn.take_masked(self.slot, mask))?;
            if notify_signal_dequeued(&dequeued.sig) == SignalDequeueAction::Deliver {
                return Some(dequeued.sig);
            }
        }
    }

    /// Lock-free fast-path read: a signal may be deliverable to this thread,
    /// counting both queues under this thread's own blocked mask.
    ///
    /// A projection, not the state: it may over-report and never under-reports.
    fn may_have_signal(&self) -> bool {
        self.hint.load(Ordering::Acquire)
    }

    /// Returns the owning process signal manager.
    pub fn process(&self) -> &Arc<ProcessSignalManager> {
        &self.proc
    }

    /// Returns the configured alternate stack with flags computed for `sp`.
    pub fn stack_for_sp(&self, sp: usize) -> SignalStack {
        let stack = self.stack.lock().clone();
        SignalStack {
            flags: stack.flags_for_sp(sp),
            ..stack
        }
    }

    /// Returns `true` when `sp` lies on the alternate signal stack.
    pub fn is_on_signal_stack(&self, sp: usize) -> bool {
        self.stack.lock().contains_sp(sp)
    }

    /// Dispatch one dequeued signal on the user-return path.
    ///
    /// Default and ignored dispositions only report the OS action; a caught
    /// signal rewrites `uctx` to enter the user handler on a freshly built
    /// signal frame (`ucontext` + `siginfo` + restorable state) written to
    /// user memory, applies the handler mask and
    /// `SA_RESETHAND`, and returns [`SignalOsAction::Handler`].
    ///
    /// # Returns
    ///
    /// `None` when the signal needs no OS action (ignored or default-ignore),
    /// otherwise the action the caller must perform. A user-memory write
    /// failure is reported as [`SignalOsAction::CoreDump`] instead of an
    /// error so the fatal path stays uniform.
    pub fn handle_signal(
        &self,
        uctx: &mut UserContext,
        restore_blocked: SignalSet,
        sig: &SignalInfo,
        action: &SignalAction,
    ) -> Option<SignalOsAction> {
        let signo = sig.signo();
        debug!("Handle signal: {signo:?}");
        match action.disposition {
            SignalDisposition::Default => match signo.default_action() {
                DefaultSignalAction::Terminate => Some(SignalOsAction::Terminate),
                DefaultSignalAction::CoreDump => Some(SignalOsAction::CoreDump),
                DefaultSignalAction::Stop => Some(SignalOsAction::Stop),
                DefaultSignalAction::Ignore => None,
                DefaultSignalAction::Continue => Some(SignalOsAction::Continue),
            },
            SignalDisposition::Ignore => None,
            SignalDisposition::Handler(handler) => {
                prepare_syscall_restart_for_signal(uctx, action.flags);
                let layout = Layout::new::<SignalFrame>();
                let stack = self.stack.lock();
                let sp = if stack.disabled() || !action.flags.contains(SignalActionFlags::ONSTACK) {
                    uctx.sp()
                } else {
                    stack.sp + stack.size
                };
                drop(stack);

                let frame_align = layout.align().max(arch::SIGNAL_FRAME_ALIGN);
                let aligned_sp = (sp - layout.size()) & !(frame_align - 1);

                let frame_ptr = aligned_sp as *mut SignalFrame;
                if frame_ptr
                    .write_vm(SignalFrame {
                        ucontext: UContext::new(uctx, restore_blocked),
                        siginfo: sig.clone(),
                        saved: uctx.save_user_restorable(),
                    })
                    .is_err()
                {
                    return Some(SignalOsAction::CoreDump);
                }

                uctx.set_ip(handler as usize);
                uctx.set_sp(aligned_sp);
                uctx.set_arg0(signo as _);
                uctx.set_arg1(aligned_sp + offset_of!(SignalFrame, siginfo));
                uctx.set_arg2(aligned_sp + offset_of!(SignalFrame, ucontext));

                let restorer = action
                    .restorer
                    .map_or(self.proc.default_restorer, |f| f as _);
                #[cfg(target_arch = "x86_64")]
                {
                    let new_sp = uctx.sp() - 8;
                    if (new_sp as *mut usize).write_vm(restorer).is_err() {
                        return Some(SignalOsAction::CoreDump);
                    }
                    uctx.set_sp(new_sp);
                }
                #[cfg(not(target_arch = "x86_64"))]
                uctx.set_ra(restorer);

                let mut add_blocked = action.mask;
                if !action.flags.contains(SignalActionFlags::NODEFER) {
                    add_blocked.add(signo);
                }

                if action.flags.contains(SignalActionFlags::RESETHAND) {
                    self.proc
                        .set_signal_action(signo, SignalAction::default())
                        .ok()?;
                }
                self.proc
                    .siglock()
                    .transact(|txn| txn.add_blocked(self.slot, add_blocked));
                Some(SignalOsAction::Handler)
            }
        }
    }

    #[cold]
    fn check_signals_slow(
        &self,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
    ) -> Option<(SignalInfo, SignalOsAction)> {
        loop {
            let dequeued = self.take_signal()?;
            let restore_blocked = restore_blocked
                .or_else(|| self.take_saved_sigmask())
                .unwrap_or(dequeued.current_blocked);
            let action = self.proc.signal_action(dequeued.sig.signo()).ok()?;

            if let Some(os_action) =
                self.handle_signal(uctx, restore_blocked, &dequeued.sig, &action)
            {
                break Some((dequeued.sig, os_action));
            }
        }
    }

    /// Dequeues the next deliverable signal, discarding any that a dequeue
    /// observer consumes.
    ///
    /// The observer runs outside the signal lock (it may take other locks or
    /// block), so a consumed signal costs one more transaction rather than a
    /// critical section held across a callback.
    fn take_signal(&self) -> Option<Dequeued> {
        loop {
            let dequeued = self.proc.siglock().transact(|txn| txn.take(self.slot))?;
            if notify_signal_dequeued(&dequeued.sig) == SignalDequeueAction::Deliver {
                return Some(dequeued);
            }
        }
    }

    /// Checks pending signals and dispatches at most one.
    ///
    /// Uses the thread and process availability projections before taking any
    /// lock; on the slow path it dequeues the first unblocked signal, consults
    /// dequeue observers, and installs its frame.
    ///
    /// # Returns
    ///
    /// The dequeued signal and the OS action to take, or `None` when no
    /// deliverable signal is pending.
    pub fn check_signals(
        &self,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
    ) -> Option<(SignalInfo, SignalOsAction)> {
        // Fast path: projections may over-report, never under-report.
        if !self.may_have_signal() {
            return None;
        }
        self.check_signals_slow(uctx, restore_blocked)
    }

    /// Restores user context from the signal frame during `sigreturn`.
    ///
    /// The frame is read back directly from the user stack at `uctx.sp()`;
    /// this trusts that the stack still maps the frame previously written by
    /// [`Self::handle_signal`]. A user thread that corrupts that stack
    /// before `rt_sigreturn` causes the kernel-side access to fault through
    /// the normal kernel fault path.
    ///
    /// Restoring replaces the blocked mask inside a transaction, which
    /// republishes the availability projection from the restored mask, so a
    /// signal queued while the handler ran is noticed before returning to
    /// user mode. The restored mask comes from the user-writable frame, so it
    /// passes through the same write entry that drops the unblockable signals
    /// (Linux `do_sigreturn` -> `set_current_blocked`).
    pub fn restore(&self, uctx: &mut UserContext) {
        let frame_ptr = uctx.sp() as *const SignalFrame;
        // SAFETY: pointer is valid
        let frame = unsafe { &*frame_ptr };

        uctx.restore_user_restorable(frame.saved);
        frame.ucontext.mcontext.restore(uctx);

        self.proc
            .siglock()
            .transact(|txn| txn.set_blocked(self.slot, frame.ucontext.sigmask));
    }

    /// Sends a signal to the thread.
    ///
    /// Returns `true` if the caller should interrupt the task.
    /// Drops the signal and returns `false` after process configuration release.
    ///
    /// A `true` result means this generation decision found the signal neither
    /// blocked nor ignored. The caller performs the actual task interrupt.
    ///
    /// Ignored signals are still queued while blocked, matching Linux signal
    /// semantics that let userspace change the handler before unblocking.
    ///
    /// See [`ProcessSignalManager::send_signal`] for the process-level version.
    #[must_use]
    pub fn send_signal(&self, sig: SignalInfo) -> bool {
        let signo = sig.signo();
        let owner = self.proc.handlers.lock();
        let Some(handlers) = owner.as_ref() else {
            return false;
        };
        let (ignored, fatal) = {
            let actions = handlers.actions.lock();
            (
                super::process::signal_ignored_by(&actions, signo),
                super::process::signal_is_fatal(&actions, signo),
            )
        };

        // The blocked check, the fatal-signal note and the enqueue are one
        // transaction: a mask change can no longer land between the
        // "deliverable" answer and the queue it justifies, and the arrival
        // projection is published from the queue that was just extended.
        let (is_blocked, accepted) = self.proc.siglock().transact(|txn| {
            let is_blocked = txn.view().blocked_has(self.slot, signo);
            if ignored && !is_blocked {
                return (is_blocked, false);
            }
            // A fatal signal delivered to this thread terminates the whole
            // process when it is not blocked; a blocked one stays pending (and
            // survives `execve`) without marking the process as dying. Record
            // it before the queue so a concurrent `exec` sibling sweep cannot
            // absorb it.
            if fatal && !is_blocked {
                self.proc.note_fatal_signal(signo);
            }
            txn.put_thread(self.slot, sig);
            (is_blocked, true)
        });

        let source = handlers.pending_changed.clone();
        drop(owner);
        if !accepted {
            return false;
        }
        source.wake();
        !is_blocked
    }

    /// Delivers the `de_thread` sibling sweep's uncatchable kill.
    ///
    /// Unlike [`Self::send_signal`], it does not bump the fatal-signal epoch:
    /// the execing thread orchestrates the sweep and must survive it, so this
    /// kill must not abort its own exec. Returns whether the target should be
    /// woken.
    #[must_use]
    pub fn deliver_sweep_kill(&self) -> bool {
        let owner = self.proc.handlers.lock();
        let Some(handlers) = owner.as_ref() else {
            return false;
        };
        let source = handlers.pending_changed.clone();
        self.proc
            .siglock()
            .transact(|txn| txn.put_thread(self.slot, SignalInfo::new_kernel(Signo::SIGKILL)));
        drop(owner);
        source.wake();
        true
    }

    /// Captures this thread's current shared arrival source without retaining it.
    /// See [`ProcessSignalManager::pending_source`] for lifetime and error semantics.
    pub fn pending_source(&self) -> KResult<kpoll::WeakPollSet> {
        self.proc.pending_source()
    }

    /// Notifies the current shared arrival source without changing pending signals.
    /// See [`ProcessSignalManager::notify_pending`] for context and error semantics.
    pub fn notify_pending(&self) -> KResult<()> {
        self.proc.notify_pending()
    }

    /// Registers for signal-arrival notifications, including blocked signals.
    ///
    /// Registration may allocate and must run in task context. The caller must
    /// recheck its pending mask after registering to close the check/wait race.
    /// Wakers run synchronously on signal send, outside all signal locks,
    /// and must not block. Notifications are shared with CLONE_SIGHAND peers; each
    /// reader must recheck its own thread's pending signals and selection mask.
    ///
    /// # Errors
    ///
    /// Propagates poll registration allocation or identity exhaustion errors.
    /// Returns `InvalidState` if configuration ownership was already released.
    /// A source that expires after capture succeeds without rebinding.
    pub fn register_pending(
        &self,
        context: &mut kpoll::PollContext<'_>,
    ) -> Result<(), kpoll::PollRegisterError> {
        let source = self
            .pending_source()
            .map_err(|_| kpoll::PollRegisterError::InvalidState)?;
        context.register_weak(&source)
    }

    /// Returns the current blocked signal set.
    pub fn blocked(&self) -> SignalSet {
        self.proc.siglock().inspect(|view| view.blocked(self.slot))
    }

    /// Replaces the blocked set, returning the previous one.
    ///
    /// `SIGKILL` and `SIGSTOP` are always cleared because they cannot be
    /// blocked; the write entry enforces that for every caller, including the
    /// `rt_sigreturn` path. The availability projection is recomputed from the
    /// new mask before the lock is released, so a signal that became
    /// deliverable by this change is handled before the next user return.
    pub fn set_blocked(&self, set: SignalSet) -> SignalSet {
        self.proc
            .siglock()
            .transact(|txn| txn.set_blocked(self.slot, set))
    }

    /// Temporarily replaces the blocked signal set for the duration of `f`,
    /// then restores the original set — including when `f` returns an error.
    /// Used by ppoll/pselect6/epoll_pwait to atomically swap the signal mask
    /// while waiting.
    pub fn with_temp_blocked<R>(
        &self,
        blocked: Option<SignalSet>,
        f: impl FnOnce() -> KResult<R>,
    ) -> KResult<R> {
        let old_blocked = blocked.map(|set| self.set_blocked(set));
        let result = f();
        if let Some(old) = old_blocked {
            self.set_blocked(old);
        }
        result
    }

    /// Saves a blocked-mask snapshot to restore when the next caught signal
    /// frame is built. Used by `rt_sigsuspend`.
    pub fn set_saved_sigmask(&self, sigmask: SignalSet) {
        self.proc
            .siglock()
            .transact(|txn| txn.set_saved_sigmask(self.slot, sigmask));
    }

    /// Takes and clears the saved blocked-mask snapshot, if any.
    pub fn take_saved_sigmask(&self) -> Option<SignalSet> {
        self.proc
            .siglock()
            .transact(|txn| txn.take_saved_sigmask(self.slot))
    }

    /// Returns the signal handler stack configuration.
    pub fn stack(&self) -> SignalStack {
        self.stack.lock().clone()
    }

    /// Replaces the signal handler stack configuration.
    pub fn set_stack(&self, stack: SignalStack) {
        *self.stack.lock() = stack;
    }

    /// Returns pending signals for this thread and its process.
    pub fn pending(&self) -> SignalSet {
        self.proc
            .siglock()
            .inspect(|view| view.pending(self.slot) | view.shared_pending())
    }
}

impl Drop for ThreadSignalManager {
    /// Releases this thread's slot so it can be reused.
    ///
    /// [`Self::exit`] retires the slot at thread exit; this covers managers
    /// retired without it. The handle is generation-checked, so a second
    /// retirement after the slot was reused is a no-op.
    ///
    /// This acquires the process `SigLock`, which is a non-reentrant
    /// `SpinNoIrq`, so the last `Arc<ThreadSignalManager>` must not be dropped
    /// while that lock is held: the drop would wait on a lock its own CPU
    /// already owns. No path does that today — transactions only touch slot
    /// state and never a manager reference — and this is stated here so a
    /// future one cannot assume otherwise.
    fn drop(&mut self) {
        self.proc.siglock().transact(|txn| txn.retire(self.slot));
    }
}

/// If the just-returned syscall error is a Linux restart code, handle it
/// and return `true` (meaning the syscall was transparently restarted and
/// the caller should *not* set up a signal handler frame).  Returns `false`
/// when no restart took place — the caller proceeds with handler dispatch
/// as usual.
/// If the just-returned syscall error is a Linux restart code, prepare
/// the context accordingly.  The handler always runs; SA_RESTART only
/// controls whether the syscall is transparently retried *after* the
/// handler returns (via sigreturn).
fn prepare_syscall_restart_for_signal(uctx: &mut UserContext, flags: SignalActionFlags) {
    let Some(err) = uctx.syscall_restart_error() else {
        return;
    };

    match err {
        LinuxError::ERESTARTSYS if flags.contains(SignalActionFlags::RESTART) => {
            uctx.rollback_syscall();
        }
        LinuxError::ERESTARTNOINTR => {
            uctx.rollback_syscall();
        }
        LinuxError::ERESTARTSYS
        | LinuxError::ERESTARTNOHAND
        | LinuxError::ERESTART_RESTARTBLOCK => {
            // `ERESTART_RESTARTBLOCK` also armed a thread restart-block. This
            // crate does not own that slot; the user-return path clears it
            // when it publishes `EINTR`.
            uctx.set_retval(-(LinuxError::EINTR.into_raw() as isize) as usize);
        }
        _ => {}
    }
}
