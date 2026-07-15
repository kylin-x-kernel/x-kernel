// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Futex and robust-list syscall adapters.

use core::{mem::size_of, sync::atomic::Ordering};

use kerrno::{KError, KResult, LinuxError};
use kfutex::FutexKey;
use kprocess::{AsThread, current_futex_key};
use kuaccess::atomic_u32_eq;
use linux_raw_sys::general::{
    FUTEX_CMD_MASK, FUTEX_CMP_REQUEUE, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_WAIT,
    FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET, robust_list_head, timespec,
};
use osvm::VirtPtr;
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
/// Linux semantics follow `futex(2)`.
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

    let process = kprocess::current_user_process();
    let futex_table = process.futex_state()?.table_for(&key);

    let command = futex_op & (FUTEX_CMD_MASK as u32);
    let uaddr_usize = uaddr.as_ptr() as usize;
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            if !atomic_u32_eq(uaddr_usize, value)? {
                return Err(KError::WouldBlock);
            }

            let timeout = if let Some(ts) =
                UserConstPtr::<timespec>::from(timeout_or_value2).check_non_null()
            {
                let ts = ts.read_vm()?.try_into_time_value()?;
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

            let wait_result = futex.wq.wait_if(bitset, timeout, || {
                atomic_u32_eq(uaddr_usize, value).unwrap_or(false)
            });
            match wait_result {
                Ok(false) => {
                    return Err(KError::WouldBlock);
                }
                Ok(true) => {}
                Err(err) => {
                    #[cfg(target_arch = "x86_64")]
                    if timeout.is_none() && LinuxError::from(err) == LinuxError::EINTR {
                        return Err(KError::from(LinuxError::ERESTARTSYS));
                    }
                    return Err(err);
                }
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
            if command == FUTEX_CMP_REQUEUE && !atomic_u32_eq(uaddr_usize, value3)? {
                return Err(KError::WouldBlock);
            }
            let value2 = validate_non_negative(timeout_or_value2 as u32)?;

            let futex = futex_table.get(&key);
            let key2 = current_key_for_futex_op(uaddr2.as_ptr() as usize, futex_op);
            let table2 = process.futex_state()?.table_for(&key2);
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

/// Returns the robust-list head of the selected thread.
pub fn sys_get_robust_list(
    tid: u32,
    head: UserPtr<*const robust_list_head>,
    size: UserPtr<usize>,
) -> KResult<isize> {
    let task = kprocess::pidfd::robust_list_task(tid)?;
    head.write_vm(task.as_thread().robust_list_head() as _)?;
    size.write_vm(size_of::<robust_list_head>())?;
    Ok(0)
}

/// Sets the robust-list head for the current thread.
pub fn sys_set_robust_list(head: UserConstPtr<robust_list_head>, size: usize) -> KResult<isize> {
    if size != size_of::<robust_list_head>() {
        return Err(KError::InvalidInput);
    }
    kprocess::current_user_thread().set_robust_list_head(head.as_ptr() as usize);
    Ok(0)
}
