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

use kcpu::userspace::UserContext;
use kerrno::{KResult, LinuxError};
use kspin::SpinNoIrq;
use osvm::VirtMutPtr;

use super::ProcessSignalManager;
use crate::{
    DefaultSignalAction, PendingSignals, SignalAction, SignalActionFlags, SignalDisposition,
    SignalInfo, SignalOSAction, SignalSet, SignalStack, Signo,
    api::{SignalDequeueAction, notify_signal_dequeued},
    arch::UContext,
};

struct SignalFrame {
    ucontext: UContext,
    siginfo: SignalInfo,
    uctx: UserContext,
}

/// Thread-level signal manager.
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

    /// Dequeues a signal from the thread's pending signals.
    #[must_use]
    pub fn dequeue_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        loop {
            let signal = self.pending.lock().dequeue_signal(mask);
            if let Some(sig) = signal {
                if notify_signal_dequeued(&sig) == SignalDequeueAction::Deliver {
                    return Some(sig);
                }
                continue;
            }
            break;
        }

        self.possibly_has_signal.store(false, Ordering::Release);
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

    /// Dispatch a signal, building a user stack frame if needed.
    pub fn dispatch_irq_signal(
        &self,
        uctx: &mut UserContext,
        restore_blocked: SignalSet,
        sig: &SignalInfo,
        action: &SignalAction,
    ) -> Option<SignalOSAction> {
        let signo = sig.signo();
        debug!("Handle signal: {signo:?}");
        match action.disposition {
            SignalDisposition::Default => match signo.default_action() {
                DefaultSignalAction::Terminate => Some(SignalOSAction::Terminate),
                DefaultSignalAction::CoreDump => Some(SignalOSAction::CoreDump),
                DefaultSignalAction::Stop => Some(SignalOSAction::Stop),
                DefaultSignalAction::Ignore => None,
                DefaultSignalAction::Continue => Some(SignalOSAction::Continue),
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

                let aligned_sp = (sp - layout.size()) & !(layout.align() - 1);

                let frame_ptr = aligned_sp as *mut SignalFrame;
                if frame_ptr
                    .write_vm(SignalFrame {
                        ucontext: UContext::new(uctx, restore_blocked),
                        siginfo: sig.clone(),
                        uctx: *uctx,
                    })
                    .is_err()
                {
                    return Some(SignalOSAction::CoreDump);
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
                        return Some(SignalOSAction::CoreDump);
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
                    self.proc.actions.lock()[signo] = SignalAction::default();
                }
                *self.blocked.lock() |= add_blocked;
                Some(SignalOSAction::Handler)
            }
        }
    }

    #[cold]
    fn check_signals_slow(
        &self,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
    ) -> Option<(SignalInfo, SignalOSAction)> {
        let blocked = self.blocked.lock();
        let mask = !*blocked;
        let current_blocked = *blocked;
        drop(blocked);

        loop {
            let sig = self.dequeue_signal(&mask)?;
            let restore_blocked = restore_blocked
                .or_else(|| self.take_saved_sigmask())
                .unwrap_or(current_blocked);
            let action = self.proc.actions.lock()[sig.signo()].clone();

            if let Some(os_action) = self.dispatch_irq_signal(uctx, restore_blocked, &sig, &action)
            {
                break Some((sig, os_action));
            }
        }
    }

    /// Checks pending signals and dispatch_irq them.
    ///
    /// Returns the signal number and the action the OS should take, if any.
    /// Checks pending signals and dispatches one if available.
    pub fn check_signals(
        &self,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
    ) -> Option<(SignalInfo, SignalOSAction)> {
        // Fast path
        if !self.possibly_has_signal.load(Ordering::Acquire)
            && !self.proc.has_pending.load(Ordering::Acquire)
        {
            return None;
        }
        self.check_signals_slow(uctx, restore_blocked)
    }

    /// Restores the signal frame. Called by `sigreturn`.
    /// Restore user context from the signal frame during `sigreturn`.
    pub fn restore(&self, uctx: &mut UserContext) {
        let frame_ptr = uctx.sp() as *const SignalFrame;
        // SAFETY: pointer is valid
        let frame = unsafe { &*frame_ptr };

        *uctx = frame.uctx;
        frame.ucontext.mcontext.restore(uctx);

        *self.blocked.lock() = frame.ucontext.sigmask;
        self.possibly_has_signal.store(true, Ordering::Release);
    }

    /// Sends a signal to the thread.
    ///
    /// Returns `true` if the task was woken up by the signal (i.e. the signal
    /// was not blocked and not ignored).
    ///
    /// See [`ProcessSignalManager::send_signal`] for the process-level version.
    #[must_use]
    pub fn send_signal(&self, sig: SignalInfo) -> bool {
        let signo = sig.signo();
        if self.proc.signal_ignored(signo) {
            return false;
        }

        if self.pending.lock().put_signal(sig) {
            self.possibly_has_signal.store(true, Ordering::Release);
        }
        !self.signal_blocked(signo)
    }

    /// Gets the blocked signals.
    /// Returns the current blocked signal set.
    pub fn blocked(&self) -> SignalSet {
        *self.blocked.lock()
    }

    /// Sets the blocked signals. Return the old value.
    /// Replace the blocked set, returning the previous one.
    pub fn set_blocked(&self, mut set: SignalSet) -> SignalSet {
        set.remove(Signo::SIGKILL);
        set.remove(Signo::SIGSTOP);
        self.possibly_has_signal.store(true, Ordering::Release);
        let mut guard = self.blocked.lock();
        let old = *guard;
        *guard = set;
        old
    }

    /// Checks if a signal is blocked.
    /// Returns `true` if the signal is currently blocked.
    pub fn signal_blocked(&self, signo: Signo) -> bool {
        self.blocked.lock().has(signo)
    }

    /// Temporarily replaces the blocked signal set for the duration of `f`,
    /// then restores the original set.  Used by ppoll/pselect6/epoll_pwait to
    /// atomically swap the signal mask while waiting.
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

    pub(crate) fn take_saved_sigmask(&self) -> Option<SignalSet> {
        self.saved_sigmask.lock().take()
    }

    /// Gets the signal stack.
    /// Returns the signal handler stack configuration.
    pub fn stack(&self) -> SignalStack {
        self.stack.lock().clone()
    }

    /// Sets the signal stack.
    /// Sets the signal handler stack configuration.
    pub fn set_stack(&self, stack: SignalStack) {
        *self.stack.lock() = stack;
    }

    /// Gets current pending signals.
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
            uctx.set_retval(-(LinuxError::EINTR.into_raw() as isize) as usize);
        }
        _ => {}
    }
}
