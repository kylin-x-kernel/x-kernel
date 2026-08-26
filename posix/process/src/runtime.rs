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

/// Create a new user task that runs in user space and handles traps.
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
        let reason = uctx.run();
        let mut runtime_action = UserThreadRuntimeAction::Continue;

        thr.set_cpu_state(CpuTimeState::Kernel);

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
                    thr.set_cpu_state(CpuTimeState::User);
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

        // Normally we check for pending signals after every trap return. The
        // exception is `rt_sigreturn`: it restores the pre-signal context, so
        // running `check_signals` here would see the same pending signal that the
        // handler just finished processing and immediately re-enter the handler,
        // looping forever. Skipping this check once is safe because the restored
        // context already has the correct signal state.
        if runtime_action != UserThreadRuntimeAction::SkipSignalCheckOnce {
            let mut handled_signal = false;
            while check_signals(&thr, &mut uctx, None) {
                handled_signal = true;
            }
            if !handled_signal {
                restart_syscall_without_signal(&mut uctx);
            }
        }

        thr.set_cpu_state(CpuTimeState::User);
        // Drop the trap-time interrupt flag before polling timers or preempting,
        // so `is_interrupted` below means a *new* `interrupt()` from this window.
        curr.clear_interrupt();
        poll_cpu_timers();
        ktask::check_preempt_pending();
        // `interrupt()` may have queued a signal in `poll_cpu_timers` (CPU-time
        // timer) or while this task was preempted (for example `alarm_task`).
        // Re-handle before `uctx.run()`; a NOHZ lone runner may never trap again.
        // `rt_sigreturn` still skips the trap check above. This path only runs
        // when a *new* interrupt arrived after `clear_interrupt`.
        if curr.is_interrupted() {
            while check_signals(&thr, &mut uctx, None) {}
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

fn dispatch_irq_futex_death(
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
            && dispatch_irq_futex_death(entry_address, offset, tid, false).is_err()
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
        let _ = dispatch_irq_futex_death(pending_address, offset, tid, true);
    }
}

/// Exit the current thread or process group and perform cleanup.
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

    let is_last_thread = process_exit::finish_thread_exit(process, &current(), exit_code);

    // `finish_thread_exit()` removes the current task from the process membership
    // table and the global TID directory. External TID lookup/signal delivery
    // already observes NoSuchProcess for this tid, but `thr` still holds the
    // current thread object for local teardown. Close its CPU-accounting
    // interval before any parent waiter can observe and reap the zombie.
    thr.set_cpu_state(kprocess::CpuTimeState::None);
    let (thread_utime, thread_stime) = thr.sample_cpu_time();
    process_exit::record_exited_thread_cpu_time(process, thread_utime, thread_stime);

    if is_last_thread {
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

    thr.set_exit();
}

/// Send a fatal signal to the current process.
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

/// Check for pending signals and execute default handlers if needed.
pub fn check_signals(
    thr: &Thread,
    uctx: &mut UserContext,
    restore_blocked: Option<SignalSet>,
) -> bool {
    let Some((sig, os_action)) = thr.signal_manager().check_signals(uctx, restore_blocked) else {
        return false;
    };

    let signo = sig.signo();
    match os_action {
        SignalOSAction::Terminate => do_exit(signo as i32, true),
        SignalOSAction::CoreDump => do_exit(128 + signo as i32, true),
        SignalOSAction::Stop => do_exit(1, true),
        SignalOSAction::Continue | SignalOSAction::Handler => {}
    }
    true
}

fn restart_syscall_without_signal(uctx: &mut UserContext) {
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
