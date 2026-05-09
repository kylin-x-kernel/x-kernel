// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX/Linux synchronization syscall implementations.

#![no_std]

#[macro_use]
extern crate klogger;

use core::{mem::size_of, sync::atomic::Ordering};

use kerrno::{KError, KResult, LinuxError};
use kthread::{AsThread, FutexKey, current_futex_key, get_task};
use linux_raw_sys::general::{
    FUTEX_CMD_MASK, FUTEX_CMP_REQUEUE, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_WAIT,
    FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET, robust_list_head, timespec,
};
use osvm::{VirtMutPtr, VirtPtr};
use posix_types::{TimeValueLike, UserConstPtr, UserPtr};

/// Returns an error if the value would be negative when interpreted as signed.
fn validate_non_negative(value: u32) -> KResult<u32> {
    if (value as i32) < 0 {
        Err(KError::InvalidInput)
    } else {
        Ok(value)
    }
}

fn current_key_for_futex_op(address: usize, futex_op: u32) -> FutexKey {
    if futex_op & FUTEX_PRIVATE_FLAG != 0 {
        FutexKey::Private { address }
    } else {
        current_futex_key(address)
    }
}

/// Fast userspace mutex (futex) system call.
///
/// Implements Linux futex semantics for efficient synchronization primitives.
/// See <https://man7.org/linux/man-pages/man2/futex.2.html>.
pub fn sys_futex(
    uaddr: UserPtr<u32>,
    futex_op: u32,
    value: u32,
    timeout_or_value2: usize,
    uaddr2: UserPtr<u32>,
    value3: u32,
) -> KResult<isize> {
    debug!(
        "sys_futex <= uaddr: {:?}, futex_op: {futex_op}, value: {value}, uaddr2: {:?}, value3: \
         {value3}",
        uaddr.as_ptr(),
        uaddr2.as_ptr(),
    );

    let key = current_key_for_futex_op(uaddr.as_ptr() as usize, futex_op);

    let thr = kthread::current_thread();
    let proc_state = &thr.proc_state;
    let futex_table = proc_state.futex_table_for(&key);

    let command = futex_op & (FUTEX_CMD_MASK as u32);
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            if uaddr.read_vm()? != value {
                return Err(KError::WouldBlock);
            }

            let timeout = if let Some(ts) =
                UserConstPtr::<timespec>::from(timeout_or_value2).check_non_null()
            {
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
            validate_non_negative(value)?;
            if command == FUTEX_CMP_REQUEUE && uaddr.read_vm()? != value3 {
                return Err(KError::WouldBlock);
            }
            let value2 = validate_non_negative(timeout_or_value2 as u32)?;

            let futex = futex_table.get(&key);
            let key2 = current_key_for_futex_op(uaddr2.as_ptr() as usize, futex_op);
            let table2 = proc_state.futex_table_for(&key2);
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
    head: UserPtr<*const robust_list_head>,
    size: UserPtr<usize>,
) -> KResult<isize> {
    let task = get_task(tid)?;
    head.write_vm(task.as_thread().robust_list_head() as _)?;
    size.write_vm(size_of::<robust_list_head>())?;

    Ok(0)
}

pub fn sys_set_robust_list(head: UserConstPtr<robust_list_head>, size: usize) -> KResult<isize> {
    if size != size_of::<robust_list_head>() {
        return Err(KError::InvalidInput);
    }
    kthread::current_thread().set_robust_list_head(head.as_ptr() as usize);

    Ok(0)
}
