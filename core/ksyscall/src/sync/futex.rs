// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Futex and robust-list syscall adapters.

use core::mem::size_of;

use kerrno::{KError, KResult, LinuxError};
use kfutex::{FutexKey, FutexWakeOp, global_table};
use kprocess::{AsThread, current_user_mm_id, current_user_process_address_space};
use ktime_types::TimeSpan;
use linux_raw_sys::general::{
    FUTEX_CLOCK_REALTIME, FUTEX_CMD_MASK, FUTEX_CMP_REQUEUE, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE,
    FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET, FUTEX_WAKE_OP, robust_list_head,
    timespec,
};
use osvm::VirtPtr;
use posix_types::{TimeSpanLike, UserConstPtr, UserPtr};

/// Converts a wake-style count from the syscall `u32` ABI.
///
/// Linux passes this as `int nr_wake`. A value that is negative as `i32`
/// must not yield `EINVAL` for `FUTEX_WAKE` / `FUTEX_WAKE_OP`; clamp to 0
/// so the call succeeds without waking anyone. (`futex_requeue` is
/// different — see [`validate_requeue_count`].)
fn as_wake_count(value: u32) -> usize {
    match value as i32 {
        n if n <= 0 => 0,
        n => n as usize,
    }
}

/// Validates a requeue count against Linux `futex_requeue` ABI.
///
/// Linux rejects negative `nr_wake` / `nr_requeue` with `EINVAL`.
fn validate_requeue_count(value: u32) -> KResult<usize> {
    match value as i32 {
        n if n < 0 => Err(KError::InvalidInput),
        n => Ok(n as usize),
    }
}

fn resolve_key(address: usize, is_private: bool) -> KResult<FutexKey> {
    if is_private {
        // Private keys only need `mm_id` + VA; `mm_id` is immutable for the
        // lifetime of the address space, so skip the mmap/munmap mutex.
        return FutexKey::resolve_private(current_user_mm_id(), address);
    }
    let address_space = current_user_process_address_space();
    let address_space = address_space.lock();
    FutexKey::resolve(&address_space, address, false)
}

/// Resolves two futex keys under one address-space lock when needed.
///
/// Shared (non-private) compound operations must not resolve `uaddr` and
/// `uaddr2` separately; a concurrent `mmap`/`munmap` could otherwise observe
/// inconsistent backing metadata within a single syscall. Private keys skip
/// the address-space lock entirely.
fn resolve_key_pair(
    first: usize,
    second: usize,
    is_private: bool,
) -> KResult<(FutexKey, FutexKey)> {
    if is_private {
        let mm_id = current_user_mm_id();
        let first_key = FutexKey::resolve_private(mm_id, first)?;
        let second_key = FutexKey::resolve_private(mm_id, second)?;
        return Ok((first_key, second_key));
    }
    let address_space = current_user_process_address_space();
    let address_space = address_space.lock();
    let first_key = FutexKey::resolve(&address_space, first, false)?;
    let second_key = FutexKey::resolve(&address_space, second, false)?;
    Ok((first_key, second_key))
}

fn parse_timeout(
    command: u32,
    is_realtime: bool,
    timeout_address: usize,
) -> KResult<Option<TimeSpan>> {
    let Some(timeout) = UserConstPtr::<timespec>::from(timeout_address).check_non_null() else {
        return Ok(None);
    };
    let timeout = timeout.read_vm()?;
    // `FUTEX_WAIT` is always relative; `FUTEX_CLOCK_REALTIME` only selects the
    // clock used to measure that relative interval. `FUTEX_WAIT_BITSET` is
    // always an absolute deadline on the selected clock.
    Ok(Some(match command {
        FUTEX_WAIT => timeout.try_into_time_span()?,
        FUTEX_WAIT_BITSET => {
            if is_realtime {
                posix_types::try_into_realtime_deadline(timeout)?
                    .duration_since(ktime::realtime())
                    .unwrap_or(TimeSpan::ZERO)
            } else {
                timeout
                    .try_into_time_span()?
                    .saturating_sub(khal::time::monotonic_time().span_since_origin())
            }
        }
        _ => return Err(KError::InvalidInput),
    }))
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

    let command = futex_op & (FUTEX_CMD_MASK as u32);
    let is_private = futex_op & FUTEX_PRIVATE_FLAG != 0;
    let is_realtime = futex_op & FUTEX_CLOCK_REALTIME != 0;
    if is_realtime && !matches!(command, FUTEX_WAIT | FUTEX_WAIT_BITSET) {
        return Err(KError::Unsupported);
    }

    let uaddr_usize = uaddr.as_ptr() as usize;
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if command == FUTEX_WAIT_BITSET {
                if value3 == 0 {
                    return Err(KError::InvalidInput);
                }
                value3
            } else {
                u32::MAX
            };
            let timeout = parse_timeout(command, is_realtime, timeout_or_value2)?;
            let key = resolve_key(uaddr_usize, is_private)?;

            match global_table().wait(key, uaddr_usize, value, bitset, timeout) {
                Ok(false) => Err(KError::WouldBlock),
                Ok(true) => Ok(0),
                Err(err) => {
                    if timeout.is_none() && LinuxError::from(err) == LinuxError::EINTR {
                        Err(KError::from(LinuxError::ERESTARTSYS))
                    } else {
                        Err(err)
                    }
                }
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let key = resolve_key(uaddr_usize, is_private)?;
            let count = as_wake_count(value);
            let bitset = if command == FUTEX_WAKE_BITSET {
                if value3 == 0 {
                    return Err(KError::InvalidInput);
                }
                value3
            } else {
                u32::MAX
            };
            Ok(global_table().wake(key, count, bitset) as isize)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            let uaddr2_usize = uaddr2.as_ptr() as usize;
            let (key, key2) = resolve_key_pair(uaddr_usize, uaddr2_usize, is_private)?;
            let wake_count = validate_requeue_count(value)?;
            let requeue_count = validate_requeue_count(timeout_or_value2 as u32)?;
            let compare = (command == FUTEX_CMP_REQUEUE).then_some((uaddr_usize, value3));
            global_table()
                .requeue(key, key2, wake_count, requeue_count, compare)
                .map(|count| count as isize)
        }
        FUTEX_WAKE_OP => {
            let uaddr2_usize = uaddr2.as_ptr() as usize;
            let (key, key2) = resolve_key_pair(uaddr_usize, uaddr2_usize, is_private)?;
            let source_count = as_wake_count(value);
            let target_count = as_wake_count(timeout_or_value2 as u32);
            let operation = FutexWakeOp::decode(value3)?;
            global_table()
                .wake_op(
                    key,
                    key2,
                    uaddr2_usize,
                    source_count,
                    target_count,
                    operation,
                )
                .map(|count| count as isize)
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
