//! Futex syscalls.
//!
//! This module implements fast userspace mutex (futex) operations including:
//! - Futex wait and wake operations
//! - Futex requeue operations
//! - Robust futex lists
//! - Priority-inheritance futexes

use core::sync::atomic::Ordering;

use kcore::{
    futex::FutexKey,
    task::{AsThread, get_task},
};
use kerrno::{KError, KResult, LinuxError};
use ktask::current;
use linux_raw_sys::general::{
    FUTEX_CMD_MASK, FUTEX_CMP_REQUEUE, FUTEX_REQUEUE, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE,
    FUTEX_WAKE_BITSET, robust_list_head, timespec,
};
use osvm::{VirtMutPtr, VirtPtr};

use crate::time::TimeValueLike;

/// Helper to ensure a value is non-negative (unsigned interpretation)
fn assert_unsigned(value: u32) -> KResult<u32> {
    if (value as i32) < 0 {
        Err(KError::InvalidInput)
    } else {
        Ok(value)
    }
}

/// Fast userspace mutex (futex) system call.
/// Implements Linux futex semantics for efficient synchronization primitives.
pub fn sys_futex(
    uaddr: *const u32,
    futex_op: u32,
    value: u32,
    timeout: *const timespec,
    uaddr2: *mut u32,
    value3: u32,
) -> KResult<isize> {
    debug!(
        "sys_futex <= uaddr: {uaddr:?}, futex_op: {futex_op}, value: {value}, uaddr2: {uaddr2:?}, \
         value3: {value3}",
    );

    // Create a unique key for this futex (by virtual address and process)
    let key = FutexKey::new_current(uaddr.addr());

    let curr = current();
    let thr = curr.as_thread();
    let proc_data = &thr.proc_data;
    // Get the futex table for the current process
    let futex_table = proc_data.futex_table_for(&key);

    // Extract the command (lower bits) from the futex_op
    let command = futex_op & (FUTEX_CMD_MASK as u32);
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            // Fast path: Check if the value at uaddr matches the expected value
            if uaddr.read_vm()? != value {
                return Err(KError::WouldBlock);
            }

            let timeout = if let Some(ts) = timeout.check_non_null() {
                // FIXME: AnyBitPattern
                let ts = unsafe { ts.read_uninit()?.assume_init() }.try_into_time_value()?;
                Some(ts)
            } else {
                None
            };

            let futex = futex_table.get_or_insert(&key);

            let bitset = if command == FUTEX_WAIT_BITSET {
                value3
            } else {
                u32::MAX
            };

            if !futex
                .wq
                .wait_if(bitset, timeout, || uaddr.read_vm() == Ok(value))?
            {
                return Err(KError::WouldBlock);
            }

            if futex.owner_dead.swap(false, Ordering::SeqCst) {
                Err(KError::from(LinuxError::EOWNERDEAD))
            } else {
                Ok(0)
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let futex = futex_table.get(&key);
            let mut count = 0;
            if let Some(futex) = futex {
                let bitset = if command == FUTEX_WAKE_BITSET {
                    value3
                } else {
                    u32::MAX
                };
                count = futex.wq.wake(value as _, bitset);
            }
            ktask::yield_now();
            Ok(count as _)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            assert_unsigned(value)?;
            if command == FUTEX_CMP_REQUEUE && uaddr.read_vm()? != value3 {
                return Err(KError::WouldBlock);
            }
            let value2 = assert_unsigned(timeout.addr() as u32)?;

            let futex = futex_table.get(&key);
            let key2 = FutexKey::new_current(uaddr2.addr());
            let table2 = proc_data.futex_table_for(&key2);
            let futex2 = table2.get_or_insert(&key2);

            let mut count = 0;
            if let Some(futex) = futex {
                count = futex.wq.wake(value as _, u32::MAX);
                if count == value as usize {
                    count += futex.wq.requeue(value2 as _, &futex2.wq) as usize;
                }
            }
            Ok(count as _)
        }
        _ => Err(KError::Unsupported),
    }
}

pub fn sys_get_robust_list(
    tid: u32,
    head: *mut *const robust_list_head,
    size: *mut usize,
) -> KResult<isize> {
    let task = get_task(tid)?;
    head.write_vm(task.as_thread().robust_list_head() as _)?;
    size.write_vm(size_of::<robust_list_head>())?;

    Ok(0)
}

pub fn sys_set_robust_list(head: *const robust_list_head, size: usize) -> KResult<isize> {
    if size != size_of::<robust_list_head>() {
        return Err(KError::InvalidInput);
    }
    current().as_thread().set_robust_list_head(head.addr());

    Ok(0)
}
