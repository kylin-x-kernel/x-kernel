// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduling-related syscall adapters tied to task state.

use kerrno::{KError, KResult};
use khal::percpu::this_cpu_id;
use kprocess::AsThread;
use ktask::{KCpuMask, current};
use linux_raw_sys::general::{
    PRIO_PGRP, PRIO_PROCESS, PRIO_USER, SCHED_BATCH, SCHED_FIFO, SCHED_IDLE, SCHED_NORMAL, SCHED_RR,
};
use posix_types::{UserConstPtr, UserPtr, UserRead, UserWrite};

const MAX_REALTIME_PRIORITY: i32 = 99;
const MIN_NICE: i32 = -20;
const MAX_NICE: i32 = 19;

#[repr(C)]
#[derive(Clone, Copy, UserRead, UserWrite)]
struct SchedParam {
    sched_priority: i32,
}

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
    if cpusetsize * 8 < kbuild_config::NR_CPUS {
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
    let size = cpusetsize.min(kbuild_config::NR_CPUS.div_ceil(8));
    let user_mask = user_mask.load_vm_vec(size)?;
    let mut cpu_mask = KCpuMask::new();

    for i in 0..(size * 8).min(kbuild_config::NR_CPUS) {
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
pub fn sys_sched_getscheduler(pid: i32) -> KResult<isize> {
    let task = scheduler_target(pid)?;

    Ok(task
        .as_thread()
        .scheduler_policy()
        .unwrap_or_else(configured_scheduler_policy) as _)
}

/// Sets the scheduler policy.
pub fn sys_sched_setscheduler(pid: i32, policy: i32, param: UserConstPtr<()>) -> KResult<isize> {
    let task = scheduler_target(pid)?;
    let policy = validate_scheduler_policy(policy)?;
    let param = param.cast::<SchedParam>().read_vm()?;

    validate_scheduler_priority(policy, param.sched_priority)?;
    task.as_thread().set_scheduler(policy, param.sched_priority);

    Ok(0)
}

/// Returns scheduler parameters.
pub fn sys_sched_getparam(pid: i32, param: UserPtr<()>) -> KResult<isize> {
    let task = scheduler_target(pid)?;
    let param = param.cast::<SchedParam>();

    param.write_vm(SchedParam {
        sched_priority: task.as_thread().scheduler_priority(),
    })?;

    Ok(0)
}

/// Updates scheduler parameters without changing the scheduler policy.
pub fn sys_sched_setparam(pid: i32, param: UserConstPtr<()>) -> KResult<isize> {
    let task = scheduler_target(pid)?;
    let thread = task.as_thread();
    let policy = thread
        .scheduler_policy()
        .unwrap_or_else(configured_scheduler_policy);
    let param = param.cast::<SchedParam>().read_vm()?;

    validate_scheduler_priority(policy, param.sched_priority)?;
    thread.set_scheduler(policy, param.sched_priority);

    Ok(0)
}

/// Returns the current nice priority for the selected target.
pub fn sys_getpriority(which: u32, who: u32) -> KResult<isize> {
    debug!("sys_getpriority <= which: {which}, who: {who}");

    match which {
        PRIO_PROCESS => {
            let proc = if who == 0 {
                kprocess::current_user_process()
            } else {
                kprocess::scheduler::target_process(who)?
            };
            process_raw_priority(&proc)
        }
        PRIO_PGRP => {
            let pg = if who == 0 {
                kprocess::current_user_process().group()
            } else {
                kprocess::scheduler::target_group(who)?
            };
            let mut min_nice = None;
            for proc in pg.processes() {
                update_min_nice(&mut min_nice, process_min_nice(&proc));
                if min_nice == Some(MIN_NICE) {
                    break;
                }
            }
            min_nice.map(raw_priority).ok_or(KError::NoSuchProcess)
        }
        PRIO_USER => {
            let uid = if who == 0 {
                kprocess::with_current_credentials(|credentials| credentials.ruid())
            } else {
                who
            };
            let mut min_nice = None;
            for proc in kprocess::scheduler::processes() {
                if proc
                    .credentials_snapshot()
                    .is_ok_and(|credentials| credentials.ruid() == uid)
                {
                    update_min_nice(&mut min_nice, process_min_nice(&proc));
                    if min_nice == Some(MIN_NICE) {
                        break;
                    }
                }
            }
            min_nice.map(raw_priority).ok_or(KError::NoSuchProcess)
        }
        _ => Err(KError::InvalidInput),
    }
}

/// Updates the current nice priority for the selected target.
pub fn sys_setpriority(which: u32, who: u32, prio: i32) -> KResult<isize> {
    debug!("sys_setpriority <= which: {which}, who: {who}, prio: {prio}");

    let prio = prio.clamp(MIN_NICE, MAX_NICE);

    match which {
        PRIO_PROCESS => {
            let proc = if who == 0 {
                kprocess::current_user_process()
            } else {
                kprocess::scheduler::target_process(who)?
            };
            set_process_nice(&proc, prio)?;
            Ok(0)
        }
        PRIO_PGRP => {
            let pg = if who == 0 {
                kprocess::current_user_process().group()
            } else {
                kprocess::scheduler::target_group(who)?
            };
            let mut updated = false;
            for proc in pg.processes() {
                updated |= set_process_nice_if_present(&proc, prio);
            }
            if !updated {
                return Err(KError::NoSuchProcess);
            }
            Ok(0)
        }
        PRIO_USER => {
            let uid = if who == 0 {
                kprocess::with_current_credentials(|credentials| credentials.ruid())
            } else {
                who
            };
            let mut updated = false;
            for proc in kprocess::scheduler::processes() {
                if proc
                    .credentials_snapshot()
                    .is_ok_and(|credentials| credentials.ruid() == uid)
                {
                    updated |= set_process_nice_if_present(&proc, prio);
                }
            }
            if !updated {
                return Err(KError::NoSuchProcess);
            }
            Ok(0)
        }
        _ => Err(KError::InvalidInput),
    }
}

fn scheduler_target(pid: i32) -> KResult<ktask::KtaskRef> {
    kprocess::scheduler::target_task(pid)
}

fn configured_scheduler_policy() -> u32 {
    if kbuild_config::KFEAT_SCHED_FIFO {
        SCHED_FIFO
    } else if kbuild_config::KFEAT_SCHED_RR {
        SCHED_RR
    } else {
        SCHED_NORMAL
    }
}

fn validate_scheduler_policy(policy: i32) -> KResult<u32> {
    if policy < 0 {
        return Err(KError::InvalidInput);
    }

    match policy as u32 {
        SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE | SCHED_FIFO | SCHED_RR => Ok(policy as u32),
        _ => Err(KError::InvalidInput),
    }
}

fn validate_scheduler_priority(policy: u32, priority: i32) -> KResult<()> {
    match policy {
        SCHED_FIFO | SCHED_RR if (1..=MAX_REALTIME_PRIORITY).contains(&priority) => Ok(()),
        SCHED_FIFO | SCHED_RR => Err(KError::InvalidInput),
        _ if priority == 0 => Ok(()),
        _ => Err(KError::InvalidInput),
    }
}

fn raw_priority(nice: i32) -> isize {
    (20 - nice) as isize
}

fn process_raw_priority(proc: &kprocess::Process) -> KResult<isize> {
    process_min_nice(proc)
        .map(raw_priority)
        .ok_or(KError::NoSuchProcess)
}

fn process_min_nice(proc: &kprocess::Process) -> Option<i32> {
    let mut min_nice = None;
    for task in kprocess::scheduler::process_tasks(proc) {
        let Some(thread) = task.try_as_thread() else {
            continue;
        };
        update_min_nice(&mut min_nice, Some(thread.nice()));
        if min_nice == Some(MIN_NICE) {
            break;
        }
    }
    min_nice
}

fn set_process_nice_if_present(proc: &kprocess::Process, nice: i32) -> bool {
    let mut updated = false;
    for task in kprocess::scheduler::process_tasks(proc) {
        let Some(thread) = task.try_as_thread() else {
            continue;
        };
        thread.set_nice(nice);
        let _ = ktask::set_task_prio(&task, nice as isize);
        updated = true;
    }
    updated
}

fn update_min_nice(min_nice: &mut Option<i32>, nice: Option<i32>) {
    let Some(nice) = nice else {
        return;
    };
    *min_nice = Some(min_nice.map_or(nice, |current| current.min(nice)));
}

fn set_process_nice(proc: &kprocess::Process, nice: i32) -> KResult<()> {
    if set_process_nice_if_present(proc, nice) {
        Ok(())
    } else {
        Err(KError::NoSuchProcess)
    }
}
