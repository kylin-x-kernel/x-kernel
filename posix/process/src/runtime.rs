// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-thread runtime helpers used by process-related syscalls.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::ffi::c_long;

use bytemuck::AnyBitPattern;
use kerrno::{KError, KResult, LinuxError};
use khal::uspace::{ExceptionKind, ReturnReason, UserContext};
use kidentity::PidHandle;
use kprocess::{
    AsThread, CpuTimeState, Pid, Thread, Tid, UserThreadRuntimeAction, current_user_process,
    current_user_process_address_space, current_user_thread, poll_cpu_timers_for, process_exit,
    process_signals, scheduler,
};
use ksignal::{SignalInfo, SignalOsAction, SignalSet, Signo};
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
        // The child is already published; a failed TID writeback must not prevent it running.
        let _ = tid.write_vm(thr.tid());
    }

    info!("Enter user space: ip={:#x}, sp={:#x}", uctx.ip(), uctx.sp());

    while !thr.is_exiting() {
        let reason = uctx.run(
            || {
                thr.set_cpu_state(CpuTimeState::User);
                // SAFETY: UserContext masks IRQs and performs only audited
                // RCU-free architecture transfer after this callback. The loop
                // holds no RCU scope; task/thread references have independent ownership.
                unsafe { krcu::enter_user_quiescent() };
            },
            || {
                // SAFETY: UserContext restores kernel context on the same CPU
                // with IRQs masked, before dispatching any returned trap. Restore
                // watching before accounting can access ordinary kernel state.
                unsafe { krcu::exit_user_quiescent() };
                thr.set_cpu_state(CpuTimeState::Kernel);
            },
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
            #[expect(
                clippy::allow_attributes,
                reason = "CG-024-lint-exceptions: The exception label is used only by \
                          architecture-specific exception recovery branches"
            )]
            #[allow(
                unused_labels,
                reason = "CG-024-lint-exceptions: unused_labels: The exception label is used only \
                          by architecture-specific exception recovery branches"
            )]
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
        // A later clear is paired with another scan. Clearing after the last
        // scan would drop a group-exit notification queued in between, and the
        // following futex wait would not observe it.
        curr.clear_interrupt();

        // The handler path dequeues a signal before entering userspace, so this
        // check cannot see that same instance again. A signal queued while the
        // handler blocked it becomes deliverable once `rt_sigreturn` restores
        // the previous mask; leaving it pending drops the handler until some
        // later trap, and an interruptible wait in between never notices it.
        // Restart was already applied or discarded when the signal frame was
        // built, so signal return must not roll the restored context back.
        // CPU-time timers and preemption run between the signal drains, and the
        // no-handler rewrite waits until those drains finish. A handler that
        // arrives in the gap must still see the restart code. The rescan is one
        // shot: a sender that keeps posting signals must not pin this thread in
        // the kernel before the handler runs.
        apply_syscall_restart(&thr, &mut uctx, runtime_action, || {
            poll_cpu_timers_for(&thr);
            ktask::check_preempt_pending();
        });
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
        // Robust-list cleanup is best effort; a stale user pointer must not stop exit.
        let _ = handle_futex_death(pending_address, offset, tid, true);
    }
}

/// Terminates every sibling thread before `execve` swaps the address space
/// (Linux `de_thread()`).
///
/// Concurrent `exec` is serialized on the process `exec` gate, so only one
/// thread ever sweeps. Every sibling receives a thread-directed `SIGKILL` and is
/// waited out, so only the execing thread remains when the caller proceeds. The
/// swept targets are marked so their fatal death leaves the group-exit
/// bookkeeping alone; a genuine concurrent `exit_group()` still marks it and
/// aborts this exec. The sweep also runs when the execing thread is not the group
/// leader, including the sole-surviving-non-leader case, so the group number
/// moves onto it.
///
/// Returns `true` when this thread was made to exit and the caller must not swap
/// the address space. Call only in the current user thread's sleepable context;
/// `join` blocks until every sibling has fully exited.
pub fn exec_de_thread() -> bool {
    let thr = current_user_thread();
    let process = thr.process();
    let tid = thr.tid();

    // Serialize concurrent `exec` before touching the thread group, matching
    // Linux `cred_guard_mutex`: a losing thread waits here and the winner's
    // sweep kills it, instead of two threads joining each other in a deadlock.
    let Some(_gate) = process.acquire_exec_gate(&current(), &thr) else {
        // Interrupted while waiting for the winner's takeover: its sweep SIGKILL
        // is pending, so terminate through the signal path (a sweep target is
        // exempt from the group-exit marking) rather than resume the old image.
        do_exit_from_signal(Signo::SIGKILL as i32);
        return true;
    };

    // A group exit already in flight (a sibling in `exit_group(2)`) must win
    // over `exec`; Linux `de_thread` returns `-EAGAIN` in this case.
    if process.is_group_exited() {
        do_exit(process.exit_code(), true);
        return true;
    }

    // Snapshot the fatal-signal counter: a genuine fatal signal delivered to an
    // already-marked sweep target must abort this exec instead of being absorbed
    // by the sweep exemption.
    let fatal_snapshot = process
        .signal_manager()
        .ok()
        .map(|manager| manager.fatal_signal_snapshot());

    // A sibling can publish between the snapshot and the joins, so sweep until
    // only this thread remains. Linux `de_thread` likewise waits for every
    // other thread to exit; a thread that publishes after the sweep makes the
    // leader-takeover precondition below fail, and that is fatal rather than a
    // silent stale-image survivor.
    loop {
        let siblings: Vec<_> = scheduler::process_tasks(process.as_ref())
            .into_iter()
            .filter(|task| task.as_thread().tid() != tid)
            .collect();
        if siblings.is_empty() {
            break;
        }

        for task in &siblings {
            // Mark the target so its fatal death does not mark the whole group
            // exited or broadcast a group kill; this thread orchestrates the
            // sweep and must survive it.
            task.as_thread().mark_exec_sweep_target();
        }
        for task in &siblings {
            // Delivers the uncatchable kill and interrupts the target, so a
            // sibling in interruptible sleep wakes to take it. The delivery
            // bypasses the fatal-signal epoch so it cannot abort this exec.
            let _ = process_signals::send_sweep_kill(task);
        }
        // Wait for every sibling to fully exit before touching the shared
        // address space, mirroring Linux waiting on each thread's exit.
        for task in siblings {
            let _ = task.join();
        }
    }

    // A concurrent genuine termination must win over this exec: a delivered
    // fatal signal bumps the epoch. Sweep kills are excluded from it, so they
    // cannot abort the exec they belong to. Terminate with the racing signal so
    // the parent observes the real `WIFSIGNALED` status instead of a clean exit
    // or the sweep's own SIGKILL. The epoch and signal number come from one
    // packed snapshot, so they cannot be mismatched.
    let racing_signal = process.signal_manager().ok().and_then(|manager| {
        let (epoch, signo) = manager.fatal_signal_snapshot();
        (fatal_snapshot.map(|snapshot| snapshot.0) != Some(epoch)).then_some(signo)
    });
    if let Some(signo) = racing_signal {
        do_exit_from_signal(signo as i32);
        return true;
    }
    // A group exit already marked without an epoch bump is `exit_group(2)`; its
    // own exit code is authoritative.
    if process.is_group_exited() {
        do_exit(process.exit_code(), true);
        return true;
    }

    // If the surviving thread is not the group leader, move the thread-group
    // identity onto it here, while this phase can still sleep. The number
    // directory entry moves once and the thread adopts the group number;
    // membership and cgroup key on the stable identity and need no change, so
    // the address-space exchange below stays a pure pointer swap. Takeover also
    // re-verifies the single-survivor invariant; failing it is fatal, otherwise
    // a late sibling would keep running against the old image.
    if !process.assume_leader_identity(&current()) {
        error!("exec_de_thread: leader identity takeover failed after the sibling sweep");
        do_exit(Signo::SIGSEGV as i32, true);
        return true;
    }
    false
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
    do_exit_inner(exit_code, group_exit, false);
}

/// Tears down the current thread after a fatal signal.
///
/// A `de_thread` sibling-sweep target is exempt from the group-exit marking:
/// the execing thread kills it on purpose and must survive the sweep. A target
/// that instead reaches `exit_group(2)` goes through [`do_exit`] and still
/// records the genuine group exit, so the exec aborts.
pub(crate) fn do_exit_from_signal(exit_code: i32) {
    do_exit_inner(exit_code, true, true);
}

fn do_exit_inner(exit_code: i32, group_exit: bool, signal_death: bool) {
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

    // Release files before retiring membership so the last thread cannot
    // publish process completion while a sibling still owns descriptors.
    thr.files().exit_files();
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

    // A `de_thread` sweep target killed by the sweep's fatal signal must not
    // mark the group exited or broadcast a group kill: the execing thread
    // orchestrates that death and must survive it. The exemption applies only to
    // signal deaths, so a sweep target that instead calls `exit_group(2)` still
    // records the genuine group exit and makes the exec abort.
    let sweep_exempt = signal_death && thr.is_exec_sweep_target();
    if group_exit && !sweep_exempt && !process.is_group_exited() {
        process_exit::mark_group_exited(process);
        let sig = SignalInfo::new_kernel(Signo::SIGKILL);
        // `finish_thread_exit()` removed the current thread from the process
        // member table before group-exit delivery, so this broadcast targets
        // only surviving sibling threads.
        for task in kprocess::scheduler::process_tasks(process.as_ref()) {
            let tid = task.as_thread().tid();
            // A sibling may already have exited and needs no delivery.
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
        do_exit_from_signal(signo as i32);
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
///
/// # Returns
///
/// The OS action just performed, or `None` when nothing was delivered.
/// [`SignalOsAction::Handler`] means a user handler folded any restart code.
/// [`SignalOsAction::Continue`] means default `SIGCONT` was consumed and the
/// restart code is still the caller's to rewrite.
pub fn check_signals(
    thr: &Thread,
    uctx: &mut UserContext,
    restore_blocked: Option<SignalSet>,
) -> Option<SignalOsAction> {
    // do_exit returns to the kernel loop; its remaining iterations must not
    // touch user mappings or repeat teardown after membership/MM release.
    if thr.is_exiting() {
        return None;
    }
    let (sig, os_action) = thr.signal_manager().check_signals(uctx, restore_blocked)?;

    let signo = sig.signo();
    match os_action {
        SignalOsAction::Terminate => do_exit_from_signal(signo as i32),
        SignalOsAction::CoreDump => do_exit_from_signal(128 + signo as i32),
        SignalOsAction::Stop => do_exit_from_signal(1),
        SignalOsAction::Continue | SignalOsAction::Handler => {}
    }
    Some(os_action)
}

/// Drains pending signals around `between_scans`, then applies syscall restart.
///
/// The restart code is recorded before either drain. Every drain in this call
/// shares one handler flag. That flag drops the restart-block when the recorded
/// code is [`LinuxError::ERESTART_RESTARTBLOCK`]. The no-handler rewrite runs
/// only after those drains, so signal delivery still sees the code and can
/// publish `EINTR` into the handler frame. Default `SIGCONT` does not install
/// a handler, so the no-handler rewrite still runs.
///
/// `between_scans` is the timer poll and preemption check on the real return
/// path. One rescan then delivers a signal queued there. A handler ends the
/// drain so userspace runs it; further signals stay pending for the next return.
fn apply_syscall_restart(
    thr: &Thread,
    uctx: &mut UserContext,
    runtime_action: UserThreadRuntimeAction,
    between_scans: impl FnOnce(),
) {
    let restart_error = uctx.syscall_restart_error();
    let mut has_installed_handler = scan_installed_handler(thr, uctx);
    between_scans();
    // One rescan, not a loop on the interrupt flag. A peer that keeps posting
    // signals would otherwise hold this thread in the kernel: the flag never
    // stays clear, the handler never runs, and the CPU is never yielded.
    // Keep the first installed handler frame until userspace has run it. A
    // timer/preemption signal queued after that frame stays pending, with its
    // interrupt notification intact, for a later return (including sigreturn).
    // This intentionally does not promise immediate delivery inside a handler.
    if !has_installed_handler && current().is_interrupted() {
        let curr = current();
        // Clear only because this scan follows. A sender that races it keeps
        // the notification for the next return.
        curr.clear_interrupt();
        has_installed_handler = scan_installed_handler(thr, uctx);
    }
    if has_installed_handler && matches!(restart_error, Some(LinuxError::ERESTART_RESTARTBLOCK)) {
        current_user_thread().clear_restart_block();
    }
    restart_syscall_without_handler(uctx, runtime_action, has_installed_handler);
}

/// Non-handler signals one drain may consume before it stops.
///
/// A handler stops the drain immediately. This cap bounds the rest, so a sender
/// that re-queues signals between dequeues cannot spin the return path.
const MAX_NONHANDLER_SIGNALS_PER_DRAIN: usize = 8;

/// Returns whether this drain installed a user handler.
fn scan_installed_handler(thr: &Thread, uctx: &mut UserContext) -> bool {
    let mut delivered = 0;
    while delivered < MAX_NONHANDLER_SIGNALS_PER_DRAIN {
        let Some(action) = check_signals(thr, uctx, None) else {
            return false;
        };
        if matches!(action, SignalOsAction::Handler) {
            return true;
        }
        delivered += 1;
    }
    // The queue is still being refilled. Yield once so a same-CPU sender can
    // run, then leave the rest pending for a later return.
    if current().is_interrupted() {
        ktask::yield_now();
    }
    false
}

/// Replays one user-return restart decision without entering the trap loop.
#[cfg(unittest)]
pub fn replay_user_return_restart_for_test(
    thr: &Thread,
    uctx: &mut UserContext,
    skip_restart: bool,
) {
    replay_user_return_restart_with_gap_for_test(thr, uctx, skip_restart, || {});
}

/// Replays the return path with a hook between the first drain and the rescan.
///
/// Queue the late signal inside `between_scans`, then `interrupt()` the current
/// task. A queued signal left with a clear interrupt flag is not scanned again
/// before the restart rewrite.
#[cfg(unittest)]
pub fn replay_user_return_restart_with_gap_for_test(
    thr: &Thread,
    uctx: &mut UserContext,
    skip_restart: bool,
    between_scans: impl FnOnce(),
) {
    let runtime_action = if skip_restart {
        UserThreadRuntimeAction::SkipSyscallRestart
    } else {
        UserThreadRuntimeAction::Continue
    };
    apply_syscall_restart(thr, uctx, runtime_action, between_scans);
}

/// Rewinds a restartable syscall when no user handler was installed.
///
/// Timed futex wait uses [`LinuxError::ERESTART_RESTARTBLOCK`]: the syscall
/// number becomes `restart_syscall` so the stored absolute deadline still
/// applies. `SA_RESTART` is not consulted here. That flag only affects
/// [`LinuxError::ERESTARTSYS`] when a userspace handler actually runs, and a
/// delivered handler turns `ERESTART_RESTARTBLOCK` into `EINTR`. The caller
/// drops the restart-block in that case.
///
/// Consuming a default `SIGCONT` does not install a handler. The caller waits
/// until the signal scan finishes so a later handler can still reject restart.
/// `sigreturn` has already restored a context whose restart was decided.
fn restart_syscall_without_handler(
    uctx: &mut UserContext,
    runtime_action: UserThreadRuntimeAction,
    has_installed_handler: bool,
) {
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
unittest::path_mod_test!(tests, "../ut-tests/runtime/tests.rs");
