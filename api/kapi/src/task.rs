// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User task entry, exit, and robust futex cleanup helpers.

use core::{ffi::c_long, sync::atomic::Ordering};

use bytemuck::AnyBitPattern;
use kcore::{
    futex::FutexKey,
    shm::SHM_MANAGER,
    task::{
        AsThread, get_process_data, get_task, send_signal_to_process, send_signal_to_thread,
        set_timer_state,
    },
    time::TimerState,
};
use kerrno::{KError, KResult};
use khal::uspace::{ExceptionKind, ReturnReason, UserContext};
use kprocess::Pid;
use ksignal::{SignalInfo, Signo};
use ktask::{TaskInner, current};
use linux_raw_sys::general::ROBUST_LIST_LIMIT;
use osvm::{VirtMutPtr, VirtPtr};

use crate::{
    signal::{check_signals, unblock_next_signal},
    syscall::dispatch_irq_syscall,
};

/// Create a new user task that runs in user space and handles traps.
pub fn new_user_task(name: &str, mut uctx: UserContext, set_child_tid: usize) -> TaskInner {
    TaskInner::new(
        move || {
            let curr = ktask::current();

            if let Some(tid) = (set_child_tid as *mut Pid).check_non_null() {
                tid.write_vm(curr.id().as_u64() as Pid).ok();
            }

            info!("Enter user space: ip={:#x}, sp={:#x}", uctx.ip(), uctx.sp());

            let thr = curr.as_thread();
            while !thr.pending_exit() {
                let reason = uctx.run();

                set_timer_state(&curr, TimerState::Kernel);

                match reason {
                    ReturnReason::Syscall => dispatch_irq_syscall(&mut uctx),
                    ReturnReason::PageFault(addr, flags) => {
                        if !thr
                            .proc_data
                            .aspace
                            .lock()
                            .dispatch_irq_page_fault(addr, flags)
                        {
                            info!(
                                "{:?}: segmentation fault at {:#x} {:?}",
                                thr.proc_data.proc, addr, flags
                            );
                            raise_signal_fatal(SignalInfo::new_kernel(Signo::SIGSEGV))
                                .expect("Failed to send SIGSEGV");
                        }
                    }
                    ReturnReason::Interrupt => {}
                    #[allow(unused_labels)]
                    ReturnReason::Exception(exc_info) => 'exc: {
                        // TODO: detailed handling
                        let signo = match exc_info.kind() {
                            ExceptionKind::Misaligned => {
                                #[cfg(target_arch = "loongarch64")]
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
                    r => {
                        warn!("Unexpected return reason: {r:?}");
                        raise_signal_fatal(SignalInfo::new_kernel(Signo::SIGSEGV))
                            .expect("Failed to send SIGSEGV");
                    }
                }

                if !unblock_next_signal() {
                    while check_signals(thr, &mut uctx, None) {}
                }

                set_timer_state(&curr, TimerState::User);
                curr.clear_interrupt();
            }
        },
        name.into(),
        kcore::config::KERNEL_STACK_SIZE,
    )
}

/// Robust futex list node for robust mutexes
#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct RobustList {
    /// Next list node.
    pub next: *mut RobustList,
}

/// Head of a robust futex list with pending operation state
#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct RobustListHead {
    /// List head.
    pub list: RobustList,
    /// Offset from list entry to futex word.
    pub futex_offset: c_long,
    /// Pending list operation entry.
    pub list_op_pending: *mut RobustList,
}

/// Mark a futex as owned by a dead task and wake waiting threads.
fn dispatch_irq_futex_death(entry: *mut RobustList, offset: i64) -> KResult<()> {
    let address = (entry as u64)
        .checked_add_signed(offset)
        .ok_or(KError::InvalidInput)?;
    let address: usize = address.try_into().map_err(|_| KError::InvalidInput)?;
    let key = FutexKey::new_current(address);

    let curr = current();
    let futex_table = curr.as_thread().proc_data.futex_table_for(&key);

    let Some(futex) = futex_table.get(&key) else {
        return Ok(());
    };
    futex.owner_dead.store(true, Ordering::SeqCst);
    futex.wq.wake(1, u32::MAX);
    Ok(())
}

/// Process robust futex list on thread exit and wake waiting threads.
pub fn exit_robust_list(head: *const RobustListHead) -> KResult<()> {
    // Reference: https://elixir.bootlin.com/linux/v6.13.6/source/kernel/futex/core.c#L777

    let mut limit = ROBUST_LIST_LIMIT;

    let end_ptr = unsafe { &raw const (*head).list };
    let head = head.read_vm()?;
    let mut entry = head.list.next;
    let offset = head.futex_offset;
    let pending = head.list_op_pending;

    while !core::ptr::eq(entry, end_ptr) {
        let next_entry = entry.read_vm()?.next;
        if entry != pending {
            dispatch_irq_futex_death(entry, offset)?;
        }
        entry = next_entry;

        limit -= 1;
        if limit == 0 {
            return Err(KError::FilesystemLoop);
        }
        ktask::yield_now();
    }

    Ok(())
}

/// Exit the current thread or process group and perform cleanup.
pub fn do_exit(exit_code: i32, group_exit: bool) {
    let curr = current();
    let thr = curr.as_thread();

    info!("{} exit with code: {}", curr.id_name(), exit_code);

    let clear_child_tid = thr.clear_child_tid() as *mut u32;
    if clear_child_tid.write_vm(0).is_ok() {
        let key = FutexKey::new_current(clear_child_tid as usize);
        let table = thr.proc_data.futex_table_for(&key);
        let guard = table.get(&key);
        if let Some(futex) = guard {
            futex.wq.wake(1, u32::MAX);
        }
        ktask::yield_now();
    }
    let head = thr.robust_list_head() as *const RobustListHead;
    if !head.is_null()
        && let Err(err) = exit_robust_list(head)
    {
        warn!("exit robust list failed: {err:?}");
    }

    let process = &thr.proc_data.proc;
    if process.exit_thread(curr.id().as_u64() as Pid, exit_code) {
        process.exit();
        if let Some(parent) = process.parent() {
            if let Some(signo) = thr.proc_data.exit_signal {
                let _ = send_signal_to_process(parent.pid(), Some(SignalInfo::new_kernel(signo)));
            }
            if let Ok(data) = get_process_data(parent.pid()) {
                data.child_exit_event.wake();
            }
        }
        thr.proc_data.exit_event.wake();

        SHM_MANAGER.lock().clear_proc_shm(process.pid());
    }
    if group_exit && !process.is_group_exited() {
        process.group_exit();
        let sig = SignalInfo::new_kernel(Signo::SIGKILL);
        for tid in process.threads() {
            let _ = send_signal_to_thread(None, tid, Some(sig.clone()));
        }
    }
    thr.set_exit();
}

/// Sends a fatal signal to the current process.
pub fn raise_signal_fatal(sig: SignalInfo) -> KResult<()> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;

    let signo = sig.signo();
    info!("Send fatal signal {signo:?} to the current process");
    if let Some(tid) = proc_data.signal.send_signal(sig)
        && let Ok(task) = get_task(tid)
    {
        task.interrupt();
    } else {
        // No task wants to dispatch_irq the signal, abort the task
        do_exit(signo as i32, true);
    }

    Ok(())
}
