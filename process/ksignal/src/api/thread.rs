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
    DefaultSignalAction, PendingSignals, SignalAction, SignalActionFlags, SignalDisposition,
    SignalInfo, SignalOsAction, SignalSet, SignalStack, Signo,
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
/// Owns the per-thread pending queue, blocked set, saved sigmask, and
/// alternate stack. Handler-frame construction happens here on the
/// user-return path; queueing decisions consult the owning
/// [`ProcessSignalManager`] for shared actions.
pub struct ThreadSignalManager {
    /// The process-level signal manager
    proc: Arc<ProcessSignalManager>,

    /// The pending signals
    pending: SpinNoIrq<PendingSignals>,
    /// The set of signals currently blocked from delivery.
    blocked: SpinNoIrq<SignalSet>,
    /// Previous blocked mask to restore when the next caught signal frame is
    /// installed, matching Linux `saved_sigmask` semantics.
    saved_sigmask: SpinNoIrq<Option<SignalSet>>,
    /// The stack used by signal handlers
    stack: SpinNoIrq<SignalStack>,

    possibly_has_signal: AtomicBool,
}

impl ThreadSignalManager {
    /// Create a new thread signal manager attached to a process.
    pub fn new(tid: u32, proc: Arc<ProcessSignalManager>) -> Arc<Self> {
        let this = Arc::new(Self {
            proc: proc.clone(),

            pending: SpinNoIrq::new(PendingSignals::default()),
            blocked: SpinNoIrq::new(SignalSet::default()),
            saved_sigmask: SpinNoIrq::new(None),
            stack: SpinNoIrq::new(SignalStack::default()),

            possibly_has_signal: AtomicBool::new(false),
        });
        proc.children.lock().push((tid, Arc::downgrade(&this)));
        this
    }

    /// Rebinds this thread's delivery target after an `exec` identity takeover.
    ///
    /// Routing selects threads by number, so the registry entry follows the
    /// moved number; other registered managers retain their own identities and
    /// lifecycle. The lookup compares object identity, so a reused TID cannot
    /// rebind a successor's entry.
    pub fn rebind_after_exec(self: &Arc<Self>, tid: u32) {
        let this = Arc::downgrade(self);
        let mut children = self.proc.children.lock();
        if let Some((number, _)) = children
            .iter_mut()
            .find(|(_, child)| alloc::sync::Weak::ptr_eq(child, &this))
        {
            *number = tid;
        }
    }

    /// Removes this thread from process-directed signal selection at exit.
    ///
    /// Call before retiring thread-group membership. A retained manager
    /// reference must not keep an exited thread eligible for delivery. The
    /// removal is idempotent and identity-compared, so a reused TID cannot
    /// remove a successor.
    pub fn exit(self: &Arc<Self>) {
        let this = Arc::downgrade(self);
        self.proc
            .children
            .lock()
            .retain(|(_, child)| !alloc::sync::Weak::ptr_eq(child, &this));
    }

    /// Dequeues a signal from the thread's pending signals.
    #[must_use]
    pub fn dequeue_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        loop {
            let signal = {
                let mut pending = self.pending.lock();
                let signal = pending.dequeue_signal(mask);
                if signal.is_none() {
                    // Serialize the no-deliverable-signal observation and hint clear with enqueue.
                    // Clearing after unlocking can overwrite a sender's true hint.
                    self.possibly_has_signal.store(false, Ordering::Release);
                }
                signal
            };
            if let Some(sig) = signal {
                if notify_signal_dequeued(&sig) == SignalDequeueAction::Deliver {
                    return Some(sig);
                }
                continue;
            }
            break;
        }

        self.proc.dequeue_signal(mask)
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
                *self.blocked.lock() |= add_blocked;
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
        let blocked = self.blocked.lock();
        let mask = !*blocked;
        let current_blocked = *blocked;
        drop(blocked);

        loop {
            let sig = self.dequeue_signal(&mask)?;
            let restore_blocked = restore_blocked
                .or_else(|| self.take_saved_sigmask())
                .unwrap_or(current_blocked);
            let action = self.proc.signal_action(sig.signo()).ok()?;

            if let Some(os_action) = self.handle_signal(uctx, restore_blocked, &sig, &action) {
                break Some((sig, os_action));
            }
        }
    }

    /// Checks pending signals and dispatches at most one.
    ///
    /// Uses the `possibly_has_signal`/process `has_pending` fast-path flags
    /// before taking any lock; on the slow path it dequeues the first
    /// unblocked signal, consults dequeue observers, and installs its frame.
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
        // Fast path
        if !self.possibly_has_signal.load(Ordering::Acquire)
            && !self.proc.has_pending.load(Ordering::Acquire)
        {
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
    /// Restoring also re-arms the pending check so a signal queued while the
    /// handler ran is noticed before returning to user mode.
    pub fn restore(&self, uctx: &mut UserContext) {
        let frame_ptr = uctx.sp() as *const SignalFrame;
        // SAFETY: pointer is valid
        let frame = unsafe { &*frame_ptr };

        uctx.restore_user_restorable(frame.saved);
        frame.ucontext.mcontext.restore(uctx);

        *self.blocked.lock() = frame.ucontext.sigmask;
        self.possibly_has_signal.store(true, Ordering::Release);
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
        let (ignored, is_blocked, fatal) = {
            let actions = handlers.actions.lock();
            let is_blocked = self.signal_blocked(signo);
            (
                super::process::signal_ignored_by(&actions, signo),
                is_blocked,
                super::process::signal_is_fatal(&actions, signo),
            )
        };

        if ignored && !is_blocked {
            return false;
        }

        // A fatal signal delivered to this thread terminates the whole process
        // when it is not blocked; a blocked one stays pending (and survives
        // `execve`) without marking the process as dying. Record it so a
        // concurrent `exec` sibling sweep cannot absorb it.
        if fatal && !is_blocked {
            self.proc.note_fatal_signal(signo);
        }
        if self.pending.lock().put_signal(sig) {
            self.possibly_has_signal.store(true, Ordering::Release);
        }
        let source = handlers.pending_changed.clone();
        drop(owner);
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
        if self
            .pending
            .lock()
            .put_signal(SignalInfo::new_kernel(Signo::SIGKILL))
        {
            self.possibly_has_signal.store(true, Ordering::Release);
        }
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
        *self.blocked.lock()
    }

    /// Replaces the blocked set, returning the previous one.
    ///
    /// `SIGKILL` and `SIGSTOP` are always cleared from `set` because they
    /// cannot be blocked. Re-arms the pending check so a signal that became
    /// deliverable by this change is handled before the next user return.
    pub fn set_blocked(&self, mut set: SignalSet) -> SignalSet {
        set.remove(Signo::SIGKILL);
        set.remove(Signo::SIGSTOP);
        self.possibly_has_signal.store(true, Ordering::Release);
        let mut guard = self.blocked.lock();
        let old = *guard;
        *guard = set;
        old
    }

    /// Checks whether the signal is currently blocked.
    pub fn signal_blocked(&self, signo: Signo) -> bool {
        self.blocked.lock().has(signo)
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
        *self.saved_sigmask.lock() = Some(sigmask);
    }

    /// Takes and clears the saved blocked-mask snapshot, if any.
    pub fn take_saved_sigmask(&self) -> Option<SignalSet> {
        self.saved_sigmask.lock().take()
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
        self.pending.lock().set | self.proc.pending()
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
