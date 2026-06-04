// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduling-related syscall adapters tied to task state.

use kerrno::{KError, KResult};
use khal::percpu::this_cpu_id;
use ktask::{KCpuMask, current};
use linux_raw_sys::general::{PRIO_PGRP, PRIO_PROCESS, PRIO_USER, SCHED_RR};
use posix_types::{UserConstPtr, UserPtr};

/// Yields the processor voluntarily.
pub fn sys_sched_yield() -> KResult<isize> {
    ktask::yield_now();
    Ok(0)
}

/// Returns the current CPU affinity mask.
pub fn sys_sched_getaffinity(
    pid: i32,
    cpusetsize: usize,
    user_mask: UserPtr<u8>,
) -> KResult<isize> {
    if cpusetsize * 8 < kbuild_config::CPU_NUM {
        return Err(KError::InvalidInput);
    }

    if pid != 0 {
        return Err(KError::OperationNotPermitted);
    }

    let mask = current().cpumask();
    let mask_bytes = mask.as_bytes();
    user_mask.write_vm_slice(mask_bytes)?;
    Ok(mask_bytes.len() as _)
}

/// Updates the current CPU affinity mask.
pub fn sys_sched_setaffinity(
    _pid: i32,
    cpusetsize: usize,
    user_mask: UserConstPtr<u8>,
) -> KResult<isize> {
    let size = cpusetsize.min(kbuild_config::CPU_NUM.div_ceil(8));
    let user_mask = user_mask.load_vm_vec(size)?;
    let mut cpu_mask = KCpuMask::new();

    for i in 0..(size * 8).min(kbuild_config::CPU_NUM) {
        if user_mask[i / 8] & (1 << (i % 8)) != 0 {
            cpu_mask.set(i, true);
        }
    }

    ktask::set_current_affinity(cpu_mask);
    Ok(0)
}

/// Returns the CPU and NUMA node on which the calling thread is running.
pub fn sys_getcpu(cpu: UserPtr<u32>, node: UserPtr<u32>, tcache: usize) -> KResult<isize> {
    if tcache != 0 {
        return Err(KError::InvalidInput);
    }

    if let Some(cpu) = cpu.check_non_null() {
        cpu.write_vm(this_cpu_id().as_usize() as u32)?;
    }

    if let Some(node) = node.check_non_null() {
        node.write_vm(0)?;
    }

    Ok(0)
}

/// Returns the current scheduler policy.
pub fn sys_sched_getscheduler(_pid: i32) -> KResult<isize> {
    Ok(SCHED_RR as _)
}

/// Sets the scheduler policy.
pub fn sys_sched_setscheduler(_pid: i32, _policy: i32, _param: UserConstPtr<()>) -> KResult<isize> {
    Ok(0)
}

/// Returns scheduler parameters.
pub fn sys_sched_getparam(_pid: i32, _param: UserPtr<()>) -> KResult<isize> {
    Ok(0)
}

/// Returns the current nice priority for the selected target.
pub fn sys_getpriority(which: u32, who: u32) -> KResult<isize> {
    debug!("sys_getpriority <= which: {which}, who: {who}");

    match which {
        PRIO_PROCESS => {
            if who != 0 {
                let _proc = kthread::get_process_state(who)?;
            }
            Ok(20)
        }
        PRIO_PGRP => {
            if who != 0 {
                let _pg = kthread::get_process_group(who)?;
            }
            Ok(20)
        }
        PRIO_USER => {
            if who == 0 {
                Ok(20)
            } else {
                Err(KError::NoSuchProcess)
            }
        }
        _ => Err(KError::InvalidInput),
    }
}

/// Updates the current nice priority for the selected target.
pub fn sys_setpriority(which: u32, who: u32, prio: i32) -> KResult<isize> {
    debug!("sys_setpriority <= which: {which}, who: {who}, prio: {prio}");

    if !(-20..=19).contains(&prio) {
        return Err(KError::InvalidInput);
    }

    match which {
        PRIO_PROCESS => {
            if who != 0 {
                let _proc = kthread::get_process_state(who)?;
            }
            Ok(0)
        }
        PRIO_PGRP => {
            if who != 0 {
                let _pg = kthread::get_process_group(who)?;
            }
            Ok(0)
        }
        PRIO_USER => {
            if who == 0 {
                Ok(0)
            } else {
                Err(KError::NoSuchProcess)
            }
        }
        _ => Err(KError::InvalidInput),
    }
}
