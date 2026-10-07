// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-thread runtime helpers used by process-related syscalls.

use alloc::{boxed::Box, sync::Arc};
use core::ffi::c_long;

use bytemuck::AnyBitPattern;
use kerrno::{KError, KResult, LinuxError};
use khal::uspace::{ExceptionKind, ReturnReason, UserContext};
use kidentity::PidHandle;
use kprocess::{
    AsThread, CpuTimeState, Pid, Thread, Tid, UserThreadRuntimeAction, current_user_process,
    current_user_process_address_space, current_user_thread, poll_cpu_timers, process_exit,
    process_signals,
};
use ksignal::{SignalInfo, SignalOSAction, SignalSet, Signo};
use ktask::{TaskInner, current};
use kuaccess::{atomic_cmpxchg_u32, atomic_load_u32};
use linux_raw_sys::general::{FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS, ROBUST_LIST_LIMIT};
use linux_sysno::Sysno;
use memspace::PageFaultOutcome;
use osvm::{VirtMutPtr, VirtPtr};
use posix_ipc::SHM_MANAGER;

/// Constructs an unpublished user task whose entry loop dispatches traps.
///
/// `task_number` must identify the supplied `thread`; `uctx` must describe its
/// prepared image. `set_child_tid`, when nonzero, is a userspace TID writeback
/// address consumed on first execution. Publish and activate the returned task
/// through `kprocess::publish_user_task` after completing parent-side setup.
/// `dispatch_syscall` runs in the task's process context after each syscall trap.
///
/// See the complete identity, runtime and publication sequence in
/// [`spawn_init_process`](crate::spawn_init_process).
pub fn new_user_task(
    name: &str,
    uctx: UserContext,
    set_child_tid: usize,
    task_number: Arc<PidHandle>,
    thread: Box<Thread>,
    dispatch_syscall: impl FnMut(&mut UserContext) -> UserThreadRuntimeAction + Send + 'static,
) -> TaskInner {
    TaskInner::new_user(
        move || {
            run_user_thread_loop(uctx, set_child_tid, dispatch_syscall);
        },
        name.into(),
        task_number,
        thread,
    )
}

/// Runs the current task as a user thread until the process exits.
pub(crate) fn run_user_thread_loop(
    mut uctx: UserContext,
    set_child_tid: usize,
    mut dispatch_syscall: impl FnMut(&mut UserContext) -> UserThreadRuntimeAction,
) {
    let curr = current();
    let thr = current_user_thread();

    if let Some(tid) = (set_child_tid as *mut Pid).check_non_null() {
        tid.write_vm(thr.tid()).ok();
    }

    info!("Enter user space: ip={:#x}, sp={:#x}", uctx.ip(), uctx.sp());

    while !thr.is_exiting() {
        let reason = uctx.run(
            || thr.set_cpu_state(CpuTimeState::User),
            || thr.set_cpu_state(CpuTimeState::Kernel),
        );
        let mut runtime_action = UserThreadRuntimeAction::Continue;

        match reason {
            ReturnReason::Syscall => {
                uctx.save_syscall_args();
                runtime_action = dispatch_syscall(&mut uctx);
            }
            ReturnReason::PageFault(addr, flags) => {
                let address_space = thr
                    .process()
                    .address_space()
                    .expect("current user thread must still expose process address space");
                let outcome = address_space.lock().handle_page_fault(addr, flags);
                if outcome.is_retryable() {
                    ktask::check_preempt_pending();
                    continue;
                } else if !outcome.is_resolved() {
                    let signo = match outcome {
                        PageFaultOutcome::BusError => Signo::SIGBUS,
                        _ => Signo::SIGSEGV,
                    };
                    let exe_path = thr.process().exe_path().unwrap_or_default();
                    warn!(
                        "segfault at {:#x} ip {:#x} sp {:#x} in {} pid={} outcome={:?} flags={:?}",
                        addr,
                        uctx.ip(),
                        uctx.sp(),
                        exe_path,
                        thr.pid(),
                        outcome,
                        flags,
                    );
                    raise_signal_fatal(SignalInfo::new_kernel(signo))
                        .expect("Failed to send fatal fault signal");
                }
            }
            ReturnReason::Interrupt => {
                ktask::check_preempt_pending();
            }
            #[allow(unused_labels)]
            ReturnReason::Exception(exc_info) => 'exc: {
                let signo = match exc_info.kind() {
                    ExceptionKind::Misaligned => {
                        #[cfg(target_arch = "loongarch64")]
                        // SAFETY: This path only runs for a LoongArch misaligned-access
                        // exception reported by `uctx.run()`. The user context still
                        // contains the faulting instruction state, which is exactly the
                        // precondition required by `emulate_unaligned`.
                        if unsafe { uctx.emulate_unaligned() }.is_ok() {
                            break 'exc;
                        }
                        Signo::SIGBUS
                    }
                    ExceptionKind::Breakpoint => Signo::SIGTRAP,
                    ExceptionKind::IllegalInstruction => Signo::SIGILL,
                    _ => Signo::SIGTRAP,
                };
                raise_signal_fatal(SignalInfo::new_kernel(signo)).expect("Failed to send SIGTRAP");
            }
            reason => {
                warn!("Unexpected return reason: {reason:?}");
                raise_signal_fatal(SignalInfo::new_kernel(Signo::SIGSEGV))
                    .expect("Failed to send SIGSEGV");
            }
        }

        // Consume the previous trap's interrupt before checking pending signals.
        // A concurrent sender after this point must leave its notification set
        // until we handle the signal or an interruptible syscall observes it.
        curr.clear_interrupt();

        // The handler path dequeues a signal before entering userspace, so this
        // check cannot see that same instance again. A signal queued while the
        // handler blocked it becomes deliverable once `rt_sigreturn` restores
        // the previous mask; leaving it pending drops the handler until some
        // later trap, and an interruptible wait in between never notices it.
        // Restart was already applied or discarded when the signal frame was
        // built, so signal return must not roll the restored context back.
        let mut has_installed_handler = false;
        while let Some(action) = check_signals(&thr, &mut uctx, None) {
            has_installed_handler |= matches!(action, SignalOSAction::Handler);
        }
        restart_syscall_without_handler(&mut uctx, runtime_action, has_installed_handler);

        poll_cpu_timers();
        ktask::check_preempt_pending();
        // `interrupt()` may have queued a signal in `poll_cpu_timers` (CPU-time
        // timer) or while this task was preempted (for example `alarm_task`).
        // Re-handle before `uctx.run()`; a NOHZ lone runner may never trap again.
        // This path only runs when a new interrupt arrived after `clear_interrupt`.
        if curr.is_interrupted() {
            while check_signals(&thr, &mut uctx, None).is_some() {}
        }
    }
}

/// Robust futex list node for robust mutexes.
#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
struct RobustList {
    next: *mut RobustList,
}

/// Head of a robust futex list with pending operation state.
#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
struct RobustListHead {
    list: RobustList,
    futex_offset: c_long,
    list_op_pending: *mut RobustList,
}

const ROBUST_LIST_PI_TAG: usize = 1;

fn decode_robust_pointer(pointer: *mut RobustList) -> (*mut RobustList, bool) {
    let bits = pointer as usize;
    (
        (bits & !ROBUST_LIST_PI_TAG) as *mut RobustList,
        bits & ROBUST_LIST_PI_TAG != 0,
    )
}

/// Userspace robust-mutex lock word (`u32` futex value).
///
/// Layout matches the Linux robust-futex ABI: TID in the low bits, plus
/// `FUTEX_OWNER_DIED` / `FUTEX_WAITERS` in the high bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
struct RobustFutexWord(u32);

impl RobustFutexWord {
    fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    fn bits(self) -> u32 {
        self.0
    }

    fn tid(self) -> Tid {
        self.0 & FUTEX_TID_MASK
    }

    /// Preserve `WAITERS`, clear the TID, and set `OWNER_DIED`.
    fn with_owner_died(self) -> Self {
        Self((self.0 & FUTEX_WAITERS) | FUTEX_OWNER_DIED)
    }
}

fn mark_futex_owner_died(address: usize, tid: Tid, is_pending: bool) -> KResult<Option<bool>> {
    loop {
        let observed = RobustFutexWord::from_bits(atomic_load_u32(address).map_err(KError::from)?);
        if observed.tid() != tid {
            // A non-PI lock operation can publish list_op_pending before the
            // userspace word. Linux wakes in the unlocked-word case so a
            // waiter cannot be stranded between those two stores.
            return Ok((is_pending && observed.bits() == 0).then_some(true));
        }
        let newval = observed.with_owner_died();
        let (exchanged, _) =
            atomic_cmpxchg_u32(address, observed.bits(), newval.bits()).map_err(KError::from)?;
        if exchanged {
            return Ok(Some(observed.bits() & FUTEX_WAITERS != 0));
        }
    }
}

fn handle_futex_death(
    entry: *mut RobustList,
    offset: i64,
    tid: Tid,
    is_pending: bool,
) -> KResult<()> {
    let address = (entry as u64)
        .checked_add_signed(offset)
        .ok_or(KError::InvalidInput)?;
    let address: usize = address.try_into().map_err(|_| KError::InvalidInput)?;
    let Some(has_waiters) = mark_futex_owner_died(address, tid, is_pending)? else {
        return Ok(());
    };
    if !has_waiters {
        return Ok(());
    }

    let address_space = current_user_process_address_space();
    let key = kfutex::FutexKey::resolve(&address_space.lock(), address, false)?;
    kfutex::global_table().wake(key, 1, u32::MAX);
    Ok(())
}

/// Process robust futex list on thread exit and wake waiting threads.
fn exit_robust_list(head: *const RobustListHead, tid: Tid) {
    let mut limit = ROBUST_LIST_LIMIT;

    // SAFETY: `head` comes from the task's registered robust-list head, and we
    // only take the address of its embedded sentinel field for list termination checks.
    let end_ptr = unsafe { &raw const (*head).list };
    let Some(head) = head.read_vm().ok() else {
        return;
    };
    let mut entry = head.list.next;
    let offset = head.futex_offset;
    let pending = head.list_op_pending;
    let (pending_address, pending_is_pi) = decode_robust_pointer(pending);

    loop {
        let (entry_address, entry_is_pi) = decode_robust_pointer(entry);
        if core::ptr::eq(entry_address, end_ptr.cast_mut()) {
            break;
        }
        let Some(next_entry) = entry_address.read_vm().map(|node| node.next).ok() else {
            break;
        };
        if entry_address != pending_address
            && !entry_is_pi
            && handle_futex_death(entry_address, offset, tid, false).is_err()
        {
            break;
        }
        entry = next_entry;

        limit -= 1;
        if limit == 0 {
            break;
        }
        ktask::yield_now();
    }

    if !pending_address.is_null() && !pending_is_pi {
        let _ = handle_futex_death(pending_address, offset, tid, true);
    }
}

/// Tears down the current user thread and optionally terminates its siblings.
///
/// `exit_code` records the thread's status; `group_exit` requests SIGKILL for
/// surviving siblings. This marks the thread exited and returns to its trap
/// loop; it does not directly terminate the kernel task. Call only in the
/// current user thread's sleepable context, without reentering its teardown.
/// Last-thread resource cleanup precedes parent-visible process completion.
///
/// # Panics
///
/// The current-thread accessor panics if no user-thread runtime is installed.
pub fn do_exit(exit_code: i32, group_exit: bool) {
    let curr = current();
    let thr = current_user_thread();
    let process = thr.process();

    info!("{} exit with code: {}", curr.id_name(), exit_code);

    let head = thr.robust_list_head() as *const RobustListHead;
    if !head.is_null() {
        exit_robust_list(head, thr.tid());
    }

    let clear_child_tid = thr.clear_child_tid() as *mut u32;
    if clear_child_tid.write_vm(0).is_ok() {
        let key = process.address_space().and_then(|address_space| {
            let address_space = address_space.lock();
            kfutex::FutexKey::resolve(&address_space, clear_child_tid as usize, false)
        });
        if let Ok(key) = key {
            kfutex::global_table().wake(key, 1, u32::MAX);
        }
    }

    // Per-thread TEE session cleanup when this thread holds a session context.
    // tee_session_release_state() is a no-op when none exists. Safe here because
    // do_exit runs after syscall/trap return; tee_session_ctx must not be held.
    #[cfg(feature = "tee")]
    if let Err(e) = tee_kernel::tee::tee_session_release_state() {
        error!("tee_session_release_state on thread exit: {e:#010X?}");
    }

    thr.detach_cgroup();
    let is_last_thread = process_exit::finish_thread_exit(process, &current(), exit_code);

    // `finish_thread_exit()` removes the current task from the process membership
    // table and the global TID directory. External TID lookup/signal delivery
    // already observes NoSuchProcess for this tid, but `thr` still holds the
    // current thread object for local teardown. Close its CPU-accounting
    // interval before any parent waiter can observe and reap the zombie.
    thr.set_exit();
    thr.set_cpu_state(CpuTimeState::Inactive);
    let (thread_utime, thread_stime) = thr.sample_cpu_time();
    process_exit::record_exited_thread_cpu_time(process, thread_utime, thread_stime);

    if is_last_thread {
        if let Err(err) = process.exit_signal_handlers() {
            error!("exit_signal_handlers on process exit failed: {err:?}");
        }
        if let Err(err) = process.exit_mm() {
            error!("exit_mm on process exit failed: {err:?}");
        }

        // Detach shared memory before waking the parent, so that
        // waitpid() returns only after segments marked IPC_RMID
        // have been destroyed.
        SHM_MANAGER.lock().clear_proc_shm(process.pid());
        #[cfg(feature = "tee")]
        if let Err(err) = process.clear_tee_runtime_private() {
            error!("clear_tee_runtime_private on process exit failed: {err:?}");
        }

        if let Err(err) = process.exit_files() {
            error!("exit_files on process exit failed: {err:?}");
        }
        if let Err(err) = process.exit_fs() {
            error!("exit_fs on process exit failed: {err:?}");
        }
        if let Err(err) = process.exit_namespaces() {
            error!("exit_namespaces on process exit failed: {err:?}");
        }
        #[cfg(feature = "tipc")]
        if let Err(err) = process.close_all_tipc_handles() {
            error!("close_all_tipc_handles on process exit failed: {err:?}");
        }

        process_exit::complete_process_exit(process);
    }

    if group_exit && !process.is_group_exited() {
        process_exit::mark_group_exited(process);
        let sig = SignalInfo::new_kernel(Signo::SIGKILL);
        // `finish_thread_exit()` removed the current thread from the process
        // member table before group-exit delivery, so this broadcast targets
        // only surviving sibling threads.
        for task in kprocess::scheduler::process_tasks(process.as_ref()) {
            let tid = task.as_thread().tid();
            let _ = kprocess::process_signals::send_to_thread(None, tid, Some(sig.clone()));
        }
    }
}

/// Queues a fatal signal for the current process or starts group exit.
///
/// Runs in current-user-thread context. If no thread accepts the signal or its
/// target cannot be interrupted, calls [`do_exit`] with the signal number.
///
/// # Errors
///
/// Forwards `Process::signal_manager` errors when the process runtime is gone.
///
/// # Panics
///
/// The current-process accessor panics outside a user-thread runtime.
pub fn raise_signal_fatal(sig: SignalInfo) -> KResult<()> {
    let signo = sig.signo();
    info!("Send fatal signal {signo:?} to the current process");
    let process = current_user_process();
    if let Some(tid) = process.signal_manager()?.send_signal(sig)
        && process_signals::interrupt_thread(tid).is_ok()
    {
    } else {
        do_exit(signo as i32, true);
    }

    Ok(())
}

/// Handles one pending signal and returns the action that was performed.
///
/// `thr` and `uctx` must belong to the current user thread. `restore_blocked`
/// supplies the optional mask restoration passed to its signal manager.
/// Terminate/CoreDump/Stop actions perform group teardown; handlers and continue
/// actions retain the signal manager's context changes. Only Handler resolves
/// syscall restart state; Continue must not suppress the no-handler path.
/// Stop currently exits with code 1, and CoreDump exits without generating a
/// core file.
/// This may block and must not run in interrupt context.
/// Exited threads do not dequeue signals or construct further user frames.
pub fn check_signals(
    thr: &Thread,
    uctx: &mut UserContext,
    restore_blocked: Option<SignalSet>,
) -> Option<SignalOSAction> {
    // do_exit returns to the kernel loop; its remaining iterations must not
    // touch user mappings or repeat teardown after membership/MM release.
    if thr.is_exiting() {
        return None;
    }
    let (sig, os_action) = thr.signal_manager().check_signals(uctx, restore_blocked)?;

    let signo = sig.signo();
    match os_action {
        SignalOSAction::Terminate => do_exit(signo as i32, true),
        SignalOSAction::CoreDump => do_exit(128 + signo as i32, true),
        SignalOSAction::Stop => do_exit(1, true),
        SignalOSAction::Continue | SignalOSAction::Handler => {}
    }
    Some(os_action)
}

fn restart_syscall_without_handler(
    uctx: &mut UserContext,
    runtime_action: UserThreadRuntimeAction,
    has_installed_handler: bool,
) {
    // Consuming a default SIGCONT does not install a handler. Wait until the
    // entire signal scan finishes so a later handler can still reject restart.
    // sigreturn has already restored a context whose restart was decided.
    if has_installed_handler || runtime_action == UserThreadRuntimeAction::SkipSyscallRestart {
        return;
    }
    let Some(err) = uctx.syscall_restart_error() else {
        return;
    };

    match err {
        LinuxError::ERESTARTSYS | LinuxError::ERESTARTNOINTR | LinuxError::ERESTARTNOHAND => {
            uctx.rollback_syscall();
        }
        LinuxError::ERESTART_RESTARTBLOCK => {
            uctx.restart_with_syscall(Sysno::restart_syscall as usize);
        }
        _ => {}
    }
}

// x86 exposes syscall-origin state through set_sysno; other architectures keep
// it private to trap entry. Do not add an architecture API just for these tests.
#[cfg(all(unittest, target_arch = "x86_64"))]
mod tests {
    use ksignal::{
        SignalAction,
        api::{ProcessSignalManager, SignalActions, ThreadSignalManager},
    };
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    fn interrupted_context(error: LinuxError) -> UserContext {
        let mut uctx = UserContext::new(0x1004, 0x2000usize.into(), 7);
        uctx.save_syscall_args();
        uctx.set_sysno(Sysno::read as usize);
        uctx.set_retval((-(error.into_raw() as isize)) as usize);
        uctx
    }

    #[def_test]
    fn default_continue_does_not_suppress_syscall_restart() {
        let process = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
        let signal = ThreadSignalManager::new(1, process);
        let mut uctx = interrupted_context(LinuxError::ERESTARTSYS);
        let action = signal.handle_signal(
            &mut uctx,
            SignalSet::default(),
            &SignalInfo::new_kernel(Signo::SIGCONT),
            &SignalAction::default(),
        );
        assert_eq!(action, Some(SignalOSAction::Continue));
        let has_installed_handler = matches!(action, Some(SignalOSAction::Handler));
        restart_syscall_without_handler(
            &mut uctx,
            UserThreadRuntimeAction::Continue,
            has_installed_handler,
        );
        assert!(uctx.syscall_restart_error().is_none());
        assert!(uctx.ip() < 0x1004);
    }

    #[def_test]
    fn handler_and_sigreturn_contexts_are_not_restarted_again() {
        for (runtime_action, has_installed_handler) in [
            (UserThreadRuntimeAction::Continue, true),
            (UserThreadRuntimeAction::SkipSyscallRestart, false),
            (UserThreadRuntimeAction::SkipSyscallRestart, true),
        ] {
            let mut uctx = interrupted_context(LinuxError::ERESTARTSYS);
            restart_syscall_without_handler(&mut uctx, runtime_action, has_installed_handler);
            assert_eq!(uctx.ip(), 0x1004);
            assert_eq!(uctx.syscall_restart_error(), Some(LinuxError::ERESTARTSYS));
        }
    }
}
