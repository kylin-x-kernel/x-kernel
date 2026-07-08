// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-thread runtime helpers used by process-related syscalls.

use core::{ffi::c_long, sync::atomic::Ordering};

use bytemuck::AnyBitPattern;
use kbuild_config::KERNEL_STACK_SIZE;
use kerrno::{KError, KResult, LinuxError};
use khal::uspace::{ExceptionKind, ReturnReason, UserContext};
use kprocess::{AsThread, Tid, process_exit};
use ksignal::{SignalInfo, SignalOSAction, SignalSet, Signo};
use ktask::{TaskInner, current};
use linux_raw_sys::general::ROBUST_LIST_LIMIT;
use linux_sysno::Sysno;
use memspace::PageFaultOutcome;
use osvm::{VirtMutPtr, VirtPtr};
use posix_ipc::SHM_MANAGER;

/// Create a new user task that runs in user space and handles traps.
pub fn new_user_task(
    name: &str,
    mut uctx: UserContext,
    set_child_tid: usize,
    task_number: alloc::sync::Arc<kidentity::PidHandle>,
    mut dispatch_syscall: impl FnMut(&mut UserContext) -> kprocess::UserThreadRuntimeAction
    + Send
    + 'static,
) -> TaskInner {
    TaskInner::new_user(
        move || {
            if let Some(tid) = (set_child_tid as *mut Tid).check_non_null() {
                tid.write_vm(kprocess::current_user_tid()).ok();
            }

            info!("Enter user space: ip={:#x}, sp={:#x}", uctx.ip(), uctx.sp());

            let thr = kprocess::current_user_thread();
            while !thr.is_exiting() {
                let reason = uctx.run();
                let mut runtime_action = kprocess::UserThreadRuntimeAction::Continue;

                thr.set_cpu_state(kprocess::CpuTimeState::Kernel);

                match reason {
                    ReturnReason::Syscall => {
                        uctx.save_syscall_args();
                        runtime_action = dispatch_syscall(&mut uctx);
                    }
                    ReturnReason::PageFault(addr, flags) => {
                        let outcome = thr
                            .process()
                            .address_space()
                            .expect("running user thread must still expose a process address space")
                            .lock()
                            .handle_page_fault(addr, flags);
                        if outcome.is_retryable() {
                            continue;
                        } else if !outcome.is_resolved() {
                            let signo = match outcome {
                                PageFaultOutcome::BusError => Signo::SIGBUS,
                                _ => Signo::SIGSEGV,
                            };
                            warn!(
                                "{:?}: page fault at {:#x} {:?} => {:?}",
                                thr.process(),
                                addr,
                                flags,
                                outcome
                            );
                            raise_signal_fatal(SignalInfo::new_kernel(signo))
                                .expect("Failed to send fatal fault signal");
                        }
                    }
                    ReturnReason::Interrupt => {}
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
                        raise_signal_fatal(SignalInfo::new_kernel(signo))
                            .expect("Failed to send SIGTRAP");
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
                if runtime_action != kprocess::UserThreadRuntimeAction::SkipSignalCheckOnce {
                    let mut handled_signal = false;
                    while check_signals(&thr, &mut uctx, None) {
                        handled_signal = true;
                    }
                    if !handled_signal {
                        restart_syscall_without_signal(&mut uctx);
                    }
                }

                thr.set_cpu_state(kprocess::CpuTimeState::User);
                kprocess::poll_cpu_timers();
                ktask::current().clear_interrupt();
            }
        },
        name.into(),
        KERNEL_STACK_SIZE,
        task_number,
    )
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

fn dispatch_irq_futex_death(entry: *mut RobustList, offset: i64) -> KResult<()> {
    let address = (entry as u64)
        .checked_add_signed(offset)
        .ok_or(KError::InvalidInput)?;
    let address: usize = address.try_into().map_err(|_| KError::InvalidInput)?;
    let key = kprocess::current_futex_key(address);

    let process = kprocess::current_user_process();
    let futex_table = process
        .futex_state()
        .expect("current user thread must still expose process futex state")
        .table_for(&key);
    let Some(futex) = futex_table.get(&key) else {
        return Ok(());
    };

    futex.owner_dead.store(true, Ordering::SeqCst);
    futex.wq.wake(1, u32::MAX);
    Ok(())
}

/// Process robust futex list on thread exit and wake waiting threads.
fn exit_robust_list(head: *const RobustListHead) {
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

    while !core::ptr::eq(entry, end_ptr) {
        let Some(next_entry) = entry.read_vm().map(|node| node.next).ok() else {
            return;
        };
        if entry != pending && dispatch_irq_futex_death(entry, offset).is_err() {
            return;
        }
        entry = next_entry;

        limit -= 1;
        if limit == 0 {
            break;
        }
        ktask::yield_now();
    }

    if !pending.is_null() {
        let _ = dispatch_irq_futex_death(pending, offset);
    }
}

/// Exit the current thread or process group and perform cleanup.
pub fn do_exit(exit_code: i32, group_exit: bool) {
    let thr = kprocess::current_user_thread();
    let process = thr.process();

    info!("{} exit with code: {}", current().id_name(), exit_code);

    let clear_child_tid = thr.clear_child_tid() as *mut u32;
    if clear_child_tid.write_vm(0).is_ok() {
        let key = kprocess::current_futex_key(clear_child_tid as usize);
        let table = thr
            .process()
            .futex_state()
            .expect("exiting thread must still expose process futex state")
            .table_for(&key);
        if let Some(futex) = table.get(&key) {
            futex.wq.wake(1, u32::MAX);
        }
        ktask::yield_now();
    }

    let head = thr.robust_list_head() as *const RobustListHead;
    if !head.is_null() {
        exit_robust_list(head);
    }

    // Per-thread TEE session cleanup when this thread holds a session context.
    // tee_session_release_state() is a no-op when none exists. Safe here because
    // do_exit runs after syscall/trap return; tee_session_ctx must not be held.
    #[cfg(feature = "tee")]
    if let Err(e) = tee_kernel::tee::tee_session_release_state() {
        error!("tee_session_release_state on thread exit: {e:#010X?}");
    }

    let is_last_thread =
        process_exit::finish_thread_exit(process, kprocess::current_user_tid(), exit_code);
    let parent_exit_signal = if is_last_thread {
        process.exit_signal()
    } else {
        None
    };
    if is_last_thread {
        process
            .close_all_fds()
            .expect("last exiting thread must still expose process resources");

        process_exit::finalize_process_exit(process);

        // Detach shared memory before waking the parent, so that
        // waitpid() returns only after segments marked IPC_RMID
        // have been destroyed.
        SHM_MANAGER.lock().clear_proc_shm(process.pid());
        #[cfg(feature = "tee")]
        process
            .clear_tee_runtime_private()
            .expect("last exiting thread must still expose tee runtime state");
    }

    // Preserve the exiting thread's final CPU sample on the stable process object
    // before any parent waiter can observe and reap the zombie.
    thr.set_cpu_state(kprocess::CpuTimeState::None);
    let (thread_utime_ns, thread_stime_ns) = thr.sample_cpu_time_ns();
    process_exit::record_exited_thread_cpu_time(process, thread_utime_ns, thread_stime_ns);

    if is_last_thread {
        if let Some(parent) = process.parent() {
            if let Some(signo) = parent_exit_signal {
                let _ = kprocess::process_signals::send_to_process(
                    parent.pid(),
                    Some(SignalInfo::new_kernel(signo)),
                );
            }
            parent.child_exit_event().wake();
        }
        process.exit_event().wake();
    }

    if group_exit && !process.is_group_exited() {
        process_exit::mark_group_exited(process);
        let sig = SignalInfo::new_kernel(Signo::SIGKILL);
        for task in kprocess::scheduler::process_tasks(process.as_ref()) {
            let tid = task.as_thread().tid();
            let _ = kprocess::process_signals::send_to_thread(None, tid, Some(sig.clone()));
        }
    }

    thr.set_exit();
}

/// Send a fatal signal to the current process.
pub fn raise_signal_fatal(sig: SignalInfo) -> KResult<()> {
    let thread = kprocess::current_user_thread();

    let signo = sig.signo();
    info!("Send fatal signal {signo:?} to the current process");
    if thread
        .process()
        .signal_manager()
        .expect("current process must still expose a signal manager")
        .send_signal(sig)
        .is_some_and(|tid| kprocess::process_signals::interrupt_thread(tid).is_ok())
    {
    } else {
        do_exit(signo as i32, true);
    }

    Ok(())
}

/// Check for pending signals and execute default handlers if needed.
pub fn check_signals(
    thr: &kprocess::Thread,
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
