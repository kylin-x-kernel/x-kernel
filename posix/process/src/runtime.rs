// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-thread runtime helpers used by process-related syscalls.

use alloc::sync::Arc;
use core::{ffi::c_long, sync::atomic::Ordering};

use bytemuck::AnyBitPattern;
use kbuild_config::KERNEL_STACK_SIZE;
use kerrno::{KError, KResult, LinuxError};
use khal::uspace::{ExceptionKind, ReturnReason, UserContext};
use kidentity::PidHandle;
use kprocess::{
    CpuTimeState, Pid, Thread, UserThreadRuntimeAction, current_futex_key, current_user_process,
    current_user_thread, poll_cpu_timers, process_exit, process_signals,
};
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
    task_number: Arc<PidHandle>,
    mut dispatch_syscall: impl FnMut(&mut UserContext) -> UserThreadRuntimeAction + Send + 'static,
) -> TaskInner {
    TaskInner::new_user(
        move || {
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
                            continue;
                        } else if !outcome.is_resolved() {
                            let signo = match outcome {
                                PageFaultOutcome::BusError => Signo::SIGBUS,
                                _ => Signo::SIGSEGV,
                            };
                            let exe_path = thr.process().exe_path().unwrap_or_default();
                            warn!(
                                "segfault at {:#x} ip {:#x} sp {:#x} in {} pid={} outcome={:?} \
                                 flags={:?}",
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
                poll_cpu_timers();
                curr.clear_interrupt();
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
    let key = current_futex_key(address);

    let futex_state = current_user_process().futex_state()?;
    let futex_table = futex_state.table_for(&key);
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
    let curr = current();
    let thr = current_user_thread();

    info!("{} exit with code: {}", curr.id_name(), exit_code);

    let clear_child_tid = thr.clear_child_tid() as *mut u32;
    if clear_child_tid.write_vm(0).is_ok() {
        let key = current_futex_key(clear_child_tid as usize);
        if let Ok(futex_state) = thr.process().futex_state() {
            let table = futex_state.table_for(&key);
            if let Some(futex) = table.get(&key) {
                futex.wq.wake(1, u32::MAX);
            }
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

    let process = thr.process().clone();
    let (utime_ns, stime_ns) = thr.sample_cpu_time_ns();
    process_exit::record_exited_thread_cpu_time(&process, utime_ns, stime_ns);
    if process_exit::finish_thread_exit(&process, thr.tid(), exit_code) {
        let _ = process.close_all_fds();

        process_exit::finalize_process_exit(&process);

        // Detach shared memory before waking the parent, so that
        // waitpid() returns only after segments marked IPC_RMID
        // have been destroyed.
        SHM_MANAGER.lock().clear_proc_shm(process.pid());
        #[cfg(feature = "tee")]
        let _ = process.clear_tee_runtime_private();
        if let Some(parent) = process.parent() {
            if let Some(signo) = process.exit_signal() {
                let _ = process_signals::send_to_process(
                    parent.pid(),
                    Some(SignalInfo::new_kernel(signo)),
                );
            }
            parent.child_exit_event().wake();
        }
        process.exit_event().wake();
    }

    if group_exit && !process.is_group_exited() {
        process_exit::mark_group_exited(&process);
        let sig = SignalInfo::new_kernel(Signo::SIGKILL);
        for tid in process.threads() {
            let _ = process_signals::send_to_thread(None, tid, Some(sig.clone()));
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
