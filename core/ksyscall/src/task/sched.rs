// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduling-related syscall adapters tied to task state.

use kcred::Cred;
use kerrno::{KError, KResult};
use khal::percpu::this_cpu_id;
use kprocess::{AsThread, NiceValue, Process};
use ktask::{
    DlParams, KCpuMask, KtaskRef, Nice, RtPrio, SchedAttr as LiveSchedAttr, SchedAttrUpdateError,
    SchedError, SchedPolicy, current, sched_getattr, sched_rr_interval, sched_update_attr,
};
use linux_raw_sys::general::{
    PRIO_PGRP, PRIO_PROCESS, PRIO_USER, SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO,
    SCHED_FLAG_RESET_ON_FORK, SCHED_IDLE, SCHED_NORMAL, SCHED_RESET_ON_FORK, SCHED_RR, timespec,
};
use osvm::VirtPtr;
use posix_types::{TimeSpanLike, UserConstPtr, UserPtr, UserRead, UserWrite};

use crate::cpuset::NativeCpuSet;

const MIN_REALTIME_PRIORITY: i32 = 1;
const MAX_REALTIME_PRIORITY: i32 = 99;

/// Linux `SCHED_ATTR_SIZE_VER0`: fields through `sched_period`.
const SCHED_ATTR_SIZE_VER0: u32 = 48;
/// Linux `SCHED_ATTR_SIZE_VER1`: adds `sched_util_min` / `sched_util_max`.
const SCHED_ATTR_SIZE_VER1: u32 = 56;
const SCHED_ATTR_PAGE_SIZE: u32 = 4096;

/// Maps a scheduler rejection onto the errno `sched_*` returns.
pub(crate) fn sched_errno(err: SchedError) -> KError {
    match err {
        SchedError::InvalidPolicy
        | SchedError::InvalidPriority
        | SchedError::InvalidNice
        | SchedError::KernelClassImmutable => KError::InvalidInput,
        SchedError::AdmissionFull => KError::ResourceBusy,
        SchedError::DeadlineFork => KError::WouldBlock,
    }
}

#[repr(C)]
#[derive(Clone, Copy, UserRead, UserWrite)]
struct SchedParam {
    sched_priority: i32,
}

/// Yields the processor voluntarily.
pub(crate) fn sys_sched_yield() -> KResult<isize> {
    ktask::yield_now();
    Ok(0)
}

/// Returns the CPU affinity mask for `pid` (0 = caller; otherwise tid/tgid).
pub(crate) fn sys_sched_getaffinity(
    pid: i32,
    cpusetsize: u32,
    user_mask: UserPtr<u8>,
) -> KResult<isize> {
    // Validate the raw ABI length before PID lookup and user access.
    if (cpusetsize as usize) < NativeCpuSet::BYTE_LEN
        || !(cpusetsize as usize).is_multiple_of(NativeCpuSet::WORD_BYTES)
    {
        return Err(KError::InvalidInput);
    }

    let task = affinity_target(pid)?;
    let bytes = NativeCpuSet::from(&task.cpumask()).to_ne_bytes();
    user_mask.write_vm_slice(&bytes)?;
    Ok(bytes.len() as isize)
}

/// Updates the CPU affinity mask for `pid` (0 = caller; otherwise tid/tgid).
pub(crate) fn sys_sched_setaffinity(
    pid: i32,
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

    // Linux order: copy mask → resolve pid (ESRCH) → same-owner / CAP_SYS_NICE
    // (EPERM) → empty intersection (EINVAL).
    let task = prepare_setaffinity_target(pid, &cpu_mask)?;

    // Linux migrates a running/ready target off CPUs outside the new mask
    // before returning success. A failed remote migrate maps to EBUSY rather
    // than a silent Ok(0).
    if !ktask::set_task_requested_affinity(&task, cpu_mask) {
        return Err(KError::ResourceBusy);
    }
    Ok(0)
}

/// Shared ESRCH / EPERM / EINVAL(empty) gate for [`sys_sched_setaffinity`].
fn prepare_setaffinity_target(pid: i32, cpu_mask: &KCpuMask) -> KResult<KtaskRef> {
    let task = affinity_target(pid)?;
    let thread = task.try_as_thread().ok_or(KError::NoSuchProcess)?;
    check_scheduler_owner_permission(&kprocess::current_cred(), &thread.real_cred())?;
    if cpu_mask.is_empty() {
        return Err(KError::InvalidInput);
    }
    Ok(task)
}

/// Shared scheduler authorization gate for scheduling setters and
/// `sched_setaffinity`.
///
/// Linux applies two independent checks: the caller's euid must match the
/// target's ruid or euid, or the caller must hold effective `CAP_SYS_NICE`;
/// then `cap_safe_nice()` requires the target's permitted capabilities to be a
/// subset of the caller's permitted set, unless `CAP_SYS_NICE` overrides it.
fn check_scheduler_owner_permission(caller: &Cred, target: &Cred) -> KResult<()> {
    check_scheduler_identity_or_privilege(caller, target)?;
    check_scheduler_capability_permission(caller, target)
}

/// The ownership/privilege half of Linux scheduler authorization.
///
/// This intentionally excludes `cap_safe_nice()` so `setpriority` can insert
/// its Linux `EACCES` nice-limit check before the capability check.
fn check_scheduler_identity_or_privilege(caller: &Cred, target: &Cred) -> KResult<()> {
    if !caller.has_cap(kcred::CAP_SYS_NICE)
        && caller.euid() != target.ruid()
        && caller.euid() != target.euid()
    {
        return Err(KError::OperationNotPermitted);
    }
    Ok(())
}

/// Linux `cap_safe_nice()`: permit the operation when the caller has effective
/// `CAP_SYS_NICE` or its permitted set contains the target's permitted set.
fn check_scheduler_capability_permission(caller: &Cred, target: &Cred) -> KResult<()> {
    if caller.has_scheduler_capability_over(target) {
        Ok(())
    } else {
        Err(KError::OperationNotPermitted)
    }
}

fn affinity_target(pid: i32) -> KResult<KtaskRef> {
    if pid < 0 {
        return Err(KError::NoSuchProcess);
    }
    if pid == 0 {
        return Ok(current().clone());
    }
    // Linux accepts a tid here (pthread_setaffinity_np). Fall back to the
    // existing process fallback used only by the affinity syscalls.
    match kprocess::scheduler::task_by_tid(pid as u32) {
        Ok(task) => Ok(task),
        Err(_) => kprocess::scheduler::target_task(pid),
    }
}

/// Returns the CPU and NUMA node on which the calling thread is running.
///
/// The legacy cache argument is ignored, matching Linux since 2.6.24.
/// Either output pointer may be null.
///
/// # Errors
///
/// Propagates user-copy errors for non-null CPU or NUMA-node output pointers.
pub(crate) fn sys_getcpu(cpu: UserPtr<u32>, node: UserPtr<u32>, _tcache: usize) -> KResult<isize> {
    if let Some(cpu) = cpu.check_non_null() {
        cpu.write_vm(this_cpu_id().as_usize() as u32)?;
    }

    if let Some(node) = node.check_non_null() {
        node.write_vm(0)?;
    }

    Ok(0)
}

/// Returns the minimum `sched_priority` for `policy`.
///
/// FIFO/RR are `1`; Fair and Deadline are `0`. Unknown policies are `EINVAL`.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a negative or unknown policy.
pub(crate) fn sys_sched_get_priority_min(policy: i32) -> KResult<isize> {
    Ok(sched_priority_bounds(policy)?.0 as isize)
}

/// Returns the maximum `sched_priority` for `policy`.
///
/// FIFO/RR are `99`; Fair and Deadline are `0`. Unknown policies are `EINVAL`.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a negative or unknown policy.
pub(crate) fn sys_sched_get_priority_max(policy: i32) -> KResult<isize> {
    Ok(sched_priority_bounds(policy)?.1 as isize)
}

/// Writes the `SCHED_RR` quantum for `pid` into `interval`.
///
/// `pid < 0` is `EINVAL`. A missing task is `ESRCH`. A null `interval` is
/// `EFAULT` after the pid check. Non-RR tasks write a zero timespec, which
/// Linux uses for an infinite quantum.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a negative pid,
/// [`KError::NoSuchProcess`] when the tid is absent, and
/// [`KError::BadAddress`] for a null interval. A mapped but unreadable
/// buffer propagates the user-memory error.
pub(crate) fn sys_sched_rr_get_interval(pid: i32, interval: UserPtr<timespec>) -> KResult<isize> {
    if pid < 0 {
        return Err(KError::InvalidInput);
    }
    let task = scheduler_target(pid)?;
    let interval = interval.check_non_null().ok_or(KError::BadAddress)?;
    interval.write_vm(timespec::from_time_span(sched_rr_interval(&task)))?;
    Ok(0)
}

/// Returns the live policy for a thread ID, or the caller when `pid` is zero.
///
/// Linux `sched_getscheduler(2)`: `pid < 0` is `EINVAL` before lookup. A
/// positive process PID denotes its leader TID. `SCHED_RESET_ON_FORK` is
/// OR-ed into the returned policy word when that flag is set.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a negative identifier and
/// [`KError::NoSuchProcess`] for an absent or unpublished positive TID.
pub(crate) fn sys_sched_getscheduler(pid: i32) -> KResult<isize> {
    if pid < 0 {
        return Err(KError::InvalidInput);
    }
    let task = scheduler_target(pid)?;
    let attr = sched_getattr(&task);
    let mut policy = attr.policy as u32;
    if attr.reset_on_fork {
        policy |= SCHED_RESET_ON_FORK;
    }
    Ok(policy as _)
}

/// Sets the live policy and class for a published TID, or the caller when `pid` is zero.
///
/// Linux `sched_setscheduler(2)`: `policy < 0` is `EINVAL` before the
/// parameter is read. A null `param` or `pid < 0` is also `EINVAL` before
/// lookup, and `param` is copied before lookup so a bad pointer is `EFAULT`
/// rather than `ESRCH`. Fair policies stay on EEVDF. `SCHED_FIFO` / `SCHED_RR`
/// move the task onto the RT class. `SCHED_DEADLINE` is rejected here;
/// [`sys_sched_setattr`] admits it. Unprivileged callers may not select a
/// realtime policy or clear an already-set `SCHED_RESET_ON_FORK` (`EPERM`;
/// effective `CAP_SYS_NICE` overrides those gates). Setting that flag needs no
/// extra privilege.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a null parameter, a negative pid, a
/// negative policy, or an illegal policy/priority pair,
/// [`KError::NoSuchProcess`] for a missing tid, and
/// [`KError::OperationNotPermitted`] when the caller does not own the target,
/// lacks privilege for a realtime policy, or tries to clear
/// `SCHED_RESET_ON_FORK`. A failure to read `param` propagates the
/// user-memory error and is reported before lookup.
pub(crate) fn sys_sched_setscheduler(
    pid: i32,
    policy: i32,
    param: UserConstPtr<()>,
) -> KResult<isize> {
    // Linux checks policy < 0 in the syscall wrapper and copies sched_param
    // before find_process_by_pid, so those failures beat ESRCH.
    if param.is_null() || pid < 0 || policy < 0 {
        return Err(KError::InvalidInput);
    }
    let param = param.cast::<SchedParam>().read_vm()?;
    let task = scheduler_target(pid)?;
    let thread = task.try_as_thread().ok_or(KError::NoSuchProcess)?;
    let (policy, reset_on_fork) = split_reset_on_fork(policy)?;

    validate_scheduler_priority(policy, param.sched_priority)?;
    let caller = kprocess::current_cred();
    check_setscheduler_permission(&caller, &thread.real_cred(), policy)?;
    let attr = update_live_sched_attr(&task, |live| {
        let had_reset_on_fork = live.reset_on_fork;
        live.policy = SchedPolicy::from_raw(policy).map_err(sched_errno)?;
        if live.policy.is_realtime() {
            live.rt_priority = RtPrio::new(param.sched_priority).map_err(sched_errno)?;
            live.dl_params = None;
        } else {
            live.rt_priority = RtPrio::NONE;
            live.dl_params = None;
        }
        check_reset_on_fork_permission(&caller, had_reset_on_fork, reset_on_fork)?;
        live.reset_on_fork = reset_on_fork;
        Ok(())
    })?;
    sync_thread_nice(&task, attr.nice);
    Ok(0)
}

/// Returns the live parameters for a thread ID, or the caller when `pid` is zero.
///
/// Linux `sched_getparam(2)`: a null `param` or `pid < 0` is `EINVAL` before
/// lookup. Only FIFO/RR report a non-zero `sched_priority`.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a null buffer or a negative identifier
/// and [`KError::NoSuchProcess`] for an absent or unpublished positive TID.
/// A failure to write `param` propagates the user-memory access error.
pub(crate) fn sys_sched_getparam(pid: i32, param: UserPtr<()>) -> KResult<isize> {
    if param.is_null() || pid < 0 {
        return Err(KError::InvalidInput);
    }
    let task = scheduler_target(pid)?;
    let attr = sched_getattr(&task);
    param.cast::<SchedParam>().write_vm(SchedParam {
        sched_priority: user_sched_priority(&attr),
    })?;
    Ok(0)
}

/// Updates parameters for a published TID without changing its policy.
///
/// Linux `sched_setparam(2)`: a null `param` or `pid < 0` is `EINVAL` before
/// lookup. A Deadline task is `EINVAL`; `sched_param` cannot carry a CBS triple.
/// The call leaves `SCHED_RESET_ON_FORK` unchanged.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a null buffer, a negative pid, a
/// Deadline target, or an illegal priority, [`KError::NoSuchProcess`] for a
/// missing tid, and [`KError::OperationNotPermitted`] when the caller does
/// not own the target or lacks privilege for its realtime policy.
pub(crate) fn sys_sched_setparam(pid: i32, param: UserConstPtr<()>) -> KResult<isize> {
    if param.is_null() || pid < 0 {
        return Err(KError::InvalidInput);
    }
    let task = scheduler_target(pid)?;
    let thread = task.try_as_thread().ok_or(KError::NoSuchProcess)?;
    let param = param.cast::<SchedParam>().read_vm()?;
    let caller = kprocess::current_cred();
    let target_cred = thread.real_cred();
    update_live_sched_attr(&task, |attr| {
        if attr.policy == SchedPolicy::Deadline {
            return Err(KError::InvalidInput);
        }
        let policy = attr.policy as u32;
        validate_scheduler_priority(policy, param.sched_priority)?;
        check_setscheduler_permission(&caller, &target_cred, policy)?;
        if attr.policy.is_realtime() {
            attr.rt_priority = RtPrio::new(param.sched_priority).map_err(sched_errno)?;
        } else if param.sched_priority != 0 {
            return Err(KError::InvalidInput);
        }
        Ok(())
    })?;
    Ok(0)
}

/// Kernel view of Linux `struct sched_attr` (`SCHED_ATTR_SIZE_VER0`).
#[derive(Clone, Copy, Debug, Default)]
struct UserSchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
    )]
    #[allow(
        unknown_lints,
        unitless_time_integer,
        reason = "CG-011-time-unit-names: Linux UAPI name `struct sched_attr` field \
                  `sched_deadline` from `include/uapi/linux/sched/types.h`"
    )]
    sched_deadline: u64,
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
    )]
    #[allow(
        unknown_lints,
        unitless_time_integer,
        reason = "CG-011-time-unit-names: Linux UAPI name `struct sched_attr` field \
                  `sched_period` from `include/uapi/linux/sched/types.h`"
    )]
    sched_period: u64,
}

/// Sets scheduler attributes for `pid` (`struct sched_attr`).
///
/// Linux `sched_setattr(2)`: unknown syscall `flags`, a null `uattr`, or
/// `pid < 0` is `EINVAL` before copy. `size` and trailing-nonzero errors
/// (`E2BIG`, `EFAULT`) happen before lookup. A negative `sched_policy` is
/// `EINVAL` before lookup. `SCHED_DEADLINE` is accepted
/// here. Unsupported `sched_flags` return `EINVAL`. An illegal
/// `sched_priority` or deadline triple is `EINVAL` before the privilege
/// check. A fair `sched_nice`
/// outside `-20..=19` is clamped, not rejected. `sched_runtime` is stored
/// only for `SCHED_DEADLINE`; Fair, FIFO, and RR ignore it.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for bad flags, a null buffer, a negative
/// pid, or an illegal attribute, [`KError::ArgumentListTooLong`] when a
/// nonzero `size` is below [`SCHED_ATTR_SIZE_VER0`], exceeds a page, or
/// trailing bytes are non-zero (the kernel size is written back),
/// [`KError::NoSuchProcess`]
/// for a missing tid, [`KError::OperationNotPermitted`] for the owner gate,
/// an unprivileged nice improvement, or clearing `SCHED_RESET_ON_FORK`, and
/// [`KError::ResourceBusy`] when Deadline admission is full. Setting that
/// flag needs no extra privilege.
pub(crate) fn sys_sched_setattr(pid: i32, uattr: UserConstPtr<u8>, flags: u32) -> KResult<isize> {
    if flags != 0 || uattr.is_null() || pid < 0 {
        return Err(KError::InvalidInput);
    }
    let attr = copy_sched_attr_from_user(uattr)?;
    if (attr.sched_policy as i32) < 0 {
        return Err(KError::InvalidInput);
    }
    let task = scheduler_target(pid)?;
    let thread = task.try_as_thread().ok_or(KError::NoSuchProcess)?;
    if attr.sched_flags & !u64::from(SCHED_FLAG_RESET_ON_FORK) != 0 {
        return Err(KError::InvalidInput);
    }

    let policy = validate_setattr_policy(attr.sched_policy)?;
    let reset_on_fork = attr.sched_flags & u64::from(SCHED_FLAG_RESET_ON_FORK) != 0;
    // Linux checks priority and the deadline triple before the privilege
    // gate, so a malformed FIFO/RR/DEADLINE request is EINVAL, not EPERM.
    let priority = attr.sched_priority as i32;
    validate_scheduler_priority(policy, priority)?;
    let dl_params = if policy == SCHED_DEADLINE {
        let period_ns = if attr.sched_period == 0 {
            attr.sched_deadline
        } else {
            attr.sched_period
        };
        Some(
            DlParams::new(attr.sched_runtime, attr.sched_deadline, period_ns)
                .map_err(sched_errno)?,
        )
    } else {
        None
    };
    let caller = kprocess::current_cred();
    check_setscheduler_permission(&caller, &thread.real_cred(), policy)?;

    let live = update_live_sched_attr(&task, |live| {
        let had_reset_on_fork = live.reset_on_fork;
        live.policy = SchedPolicy::from_raw(policy).map_err(sched_errno)?;
        if let Some(params) = dl_params {
            // Fair weight stays put so a later return to Fair restores it.
            live.rt_priority = RtPrio::NONE;
            live.dl_params = Some(params);
        } else {
            live.dl_params = None;
            if live.policy.is_realtime() {
                live.rt_priority = RtPrio::new(priority).map_err(sched_errno)?;
            } else {
                live.rt_priority = RtPrio::NONE;
                // Linux clamps sched_nice. The privilege check sees that value.
                let nice = Nice::new_clamped(attr.sched_nice);
                // sched_setattr(2) reports EPERM here. setpriority keeps EACCES.
                let current_nice = NiceValue::new_clamped(live.nice.as_i32());
                let requested_nice = NiceValue::new_clamped(nice.as_i32());
                if requested_nice < current_nice && !caller.has_cap(kcred::CAP_SYS_NICE) {
                    return Err(KError::OperationNotPermitted);
                }
                live.nice = nice;
            }
        }
        check_reset_on_fork_permission(&caller, had_reset_on_fork, reset_on_fork)?;
        live.reset_on_fork = reset_on_fork;
        Ok(())
    })?;
    sync_thread_nice(&task, live.nice);
    Ok(0)
}

/// Returns scheduler attributes for `pid` into user `struct sched_attr`.
///
/// Linux `sched_getattr(2)`: a null `uattr`, `pid < 0`, `size` below
/// [`SCHED_ATTR_SIZE_VER0`], `size` above a page, or nonzero `flags` is
/// `EINVAL` before lookup. The written `size` is `min(size, VER1)`, the
/// number of bytes filled. Bytes past that are cleared. `sched_nice` is the
/// stored value for Fair policies and 0 for FIFO, RR, and Deadline.
/// `sched_runtime` is 0 except for Deadline: Fair has no per-task slice.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a null buffer, a negative pid, a
/// size outside `VER0..=PAGE_SIZE`, or unknown flags, and
/// [`KError::NoSuchProcess`] for a missing tid. A short mapped buffer
/// propagates the user-memory error.
pub(crate) fn sys_sched_getattr(
    pid: i32,
    uattr: UserPtr<u8>,
    size: u32,
    flags: u32,
) -> KResult<isize> {
    if uattr.is_null()
        || pid < 0
        || !(SCHED_ATTR_SIZE_VER0..=SCHED_ATTR_PAGE_SIZE).contains(&size)
        || flags != 0
    {
        return Err(KError::InvalidInput);
    }
    let task = scheduler_target(pid)?;
    let live = sched_getattr(&task);
    let mut attr = UserSchedAttr {
        size: size.min(SCHED_ATTR_SIZE_VER1),
        sched_policy: live.policy as u32,
        sched_flags: 0,
        sched_nice: user_sched_nice(&live),
        sched_priority: user_sched_priority(&live) as u32,
        sched_runtime: 0,
        sched_deadline: 0,
        sched_period: 0,
    };
    if live.reset_on_fork {
        attr.sched_flags |= u64::from(SCHED_FLAG_RESET_ON_FORK);
    }
    if let Some(params) = live.dl_params {
        attr.sched_runtime = params.runtime_ns;
        attr.sched_deadline = params.deadline_ns;
        attr.sched_period = params.period_ns;
    }
    write_sched_attr_to_user(uattr, &attr, size)?;
    Ok(0)
}

/// Returns the current nice priority for the selected target.
pub(crate) fn sys_getpriority(which: u32, who: u32) -> KResult<isize> {
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
                if min_nice == Some(NiceValue::MIN) {
                    break;
                }
            }
            min_nice.map(raw_priority).ok_or(KError::NoSuchProcess)
        }
        PRIO_USER => {
            let uid = if who == 0 {
                kprocess::current_cred().ruid()
            } else {
                who
            };
            let mut min_nice = None;
            'processes: for proc in kprocess::scheduler::processes() {
                for task in kprocess::scheduler::process_tasks(&proc) {
                    let Some(thread) = task.try_as_thread() else {
                        continue;
                    };
                    if thread.real_cred().ruid() != uid {
                        continue;
                    }
                    update_min_nice(&mut min_nice, Some(thread.nice()));
                    if min_nice == Some(NiceValue::MIN) {
                        break 'processes;
                    }
                }
            }
            min_nice.map(raw_priority).ok_or(KError::NoSuchProcess)
        }
        _ => Err(KError::InvalidInput),
    }
}

/// Updates the current nice priority for the selected target.
pub(crate) fn sys_setpriority(which: u32, who: u32, prio: i32) -> KResult<isize> {
    debug!("sys_setpriority <= which: {which}, who: {who}, prio: {prio}");

    let nice = NiceValue::new_clamped(prio);
    let caller = kprocess::current_cred();

    match which {
        PRIO_PROCESS => {
            let proc = if who == 0 {
                kprocess::current_user_process()
            } else {
                kprocess::scheduler::target_process(who)?
            };
            let mut result = NiceUpdateResult::default();
            update_process_nice(&proc, nice, &caller, &mut result);
            result.finish()?;
            Ok(0)
        }
        PRIO_PGRP => {
            let pg = if who == 0 {
                kprocess::current_user_process().group()
            } else {
                kprocess::scheduler::target_group(who)?
            };
            let mut result = NiceUpdateResult::default();
            for proc in pg.processes() {
                update_process_nice(&proc, nice, &caller, &mut result);
            }
            result.finish()?;
            Ok(0)
        }
        PRIO_USER => {
            let uid = if who == 0 {
                kprocess::current_cred().ruid()
            } else {
                who
            };
            let mut result = NiceUpdateResult::default();
            for proc in kprocess::scheduler::processes() {
                for task in kprocess::scheduler::process_tasks(&proc) {
                    let Some(thread) = task.try_as_thread() else {
                        continue;
                    };
                    if thread.real_cred().ruid() == uid {
                        result.record(set_task_nice(&task, nice, &caller));
                    }
                }
            }
            result.finish()?;
            Ok(0)
        }
        _ => Err(KError::InvalidInput),
    }
}

fn scheduler_target(pid: i32) -> KResult<ktask::KtaskRef> {
    // Callers that follow Linux `EINVAL`-before-lookup reject `pid < 0`
    // themselves. This helper still maps a negative id to ESRCH so a missed
    // gate cannot look up `pid as u32`.
    if pid < 0 {
        return Err(KError::NoSuchProcess);
    }
    if pid == 0 {
        return Ok(current().clone());
    }
    // A positive process PID names its leader TID, not a representative worker.
    // An unpublished leader remains ESRCH while workers live: falling back
    // would update a different thread from the one the caller named.
    kprocess::scheduler::task_by_tid(pid as u32)
}

/// Same-owner gate, plus privilege for a live realtime or Deadline policy.
///
/// `SCHED_RESET_ON_FORK` is checked separately. [`sys_sched_setparam`] does
/// not carry the flag and must leave it unchanged.
fn check_setscheduler_permission(caller: &Cred, target: &Cred, policy: u32) -> KResult<()> {
    check_scheduler_owner_permission(caller, target)?;
    if matches!(policy, SCHED_FIFO | SCHED_RR | SCHED_DEADLINE)
        && !caller.has_cap(kcred::CAP_SYS_NICE)
    {
        return Err(KError::OperationNotPermitted);
    }
    Ok(())
}

/// Apply `update` to the live attribute under one run-queue lock.
///
/// User memory and credential snapshots stay outside this helper. `update`
/// must not sleep or take another run-queue lock.
fn update_live_sched_attr(
    task: &KtaskRef,
    update: impl FnOnce(&mut LiveSchedAttr) -> KResult<()>,
) -> KResult<LiveSchedAttr> {
    sched_update_attr(task, update).map_err(|err| match err {
        SchedAttrUpdateError::Rejected(err) => err,
        SchedAttrUpdateError::Sched(err) => sched_errno(err),
    })
}

/// `SCHED_RESET_ON_FORK` is sticky.
///
/// An unprivileged owner may set the flag. Clearing one that is already set
/// requires `CAP_SYS_NICE`. Linux
/// `sched_setscheduler(2)` and `sched_setattr(2)` return `EPERM` for that clear.
fn check_reset_on_fork_permission(caller: &Cred, current: bool, requested: bool) -> KResult<()> {
    if current && !requested && !caller.has_cap(kcred::CAP_SYS_NICE) {
        return Err(KError::OperationNotPermitted);
    }
    Ok(())
}

fn validate_scheduler_policy(policy: u32) -> KResult<u32> {
    match policy {
        SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE | SCHED_FIFO | SCHED_RR => Ok(policy),
        _ => Err(KError::InvalidInput),
    }
}

fn validate_setattr_policy(policy: u32) -> KResult<u32> {
    match policy {
        SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE | SCHED_FIFO | SCHED_RR | SCHED_DEADLINE => {
            Ok(policy)
        }
        _ => Err(KError::InvalidInput),
    }
}

fn split_reset_on_fork(policy: i32) -> KResult<(u32, bool)> {
    if policy < 0 {
        return Err(KError::InvalidInput);
    }
    let raw = policy as u32;
    let reset_on_fork = raw & SCHED_RESET_ON_FORK != 0;
    let policy = validate_scheduler_policy(raw & !SCHED_RESET_ON_FORK)?;
    Ok((policy, reset_on_fork))
}

fn validate_scheduler_priority(policy: u32, priority: i32) -> KResult<()> {
    match policy {
        SCHED_FIFO | SCHED_RR
            if (MIN_REALTIME_PRIORITY..=MAX_REALTIME_PRIORITY).contains(&priority) =>
        {
            Ok(())
        }
        SCHED_FIFO | SCHED_RR => Err(KError::InvalidInput),
        SCHED_DEADLINE if priority == 0 => Ok(()),
        _ if priority == 0 => Ok(()),
        _ => Err(KError::InvalidInput),
    }
}

/// Linux `sched_get_priority_{min,max}` table.
fn sched_priority_bounds(policy: i32) -> KResult<(i32, i32)> {
    if policy < 0 {
        return Err(KError::InvalidInput);
    }
    match policy as u32 {
        SCHED_FIFO | SCHED_RR => Ok((MIN_REALTIME_PRIORITY, MAX_REALTIME_PRIORITY)),
        SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => Ok((0, 0)),
        _ => Err(KError::InvalidInput),
    }
}

/// Linux `sched_getattr(2)` `sched_nice`.
///
/// Only Fair policies report the stored nice. FIFO, RR, and Deadline report
/// 0. The stored value is kept so a later return to Fair restores the weight.
fn user_sched_nice(attr: &LiveSchedAttr) -> i32 {
    match attr.policy {
        SchedPolicy::Normal | SchedPolicy::Batch | SchedPolicy::Idle => attr.nice.as_i32(),
        SchedPolicy::Fifo | SchedPolicy::Rr | SchedPolicy::Deadline => 0,
    }
}

/// Linux `sched_getparam(2)` / `sched_getattr(2)` `sched_priority`.
///
/// Only FIFO/RR report the live RT priority. Fair and Deadline report 0.
fn user_sched_priority(attr: &LiveSchedAttr) -> i32 {
    if attr.policy.is_realtime() {
        i32::from(attr.rt_priority.get())
    } else {
        0
    }
}

fn sync_thread_nice(task: &KtaskRef, nice: Nice) {
    if let Some(thread) = task.try_as_thread() {
        thread.set_nice(NiceValue::new_clamped(nice.as_i32()));
    }
}

fn copy_sched_attr_from_user(uattr: UserConstPtr<u8>) -> KResult<UserSchedAttr> {
    let mut size = uattr.cast::<u32>().read_vm()?;
    if size == 0 {
        size = SCHED_ATTR_SIZE_VER0;
    }
    if size < SCHED_ATTR_SIZE_VER0 {
        return setattr_size_mismatch(uattr);
    }
    if size > SCHED_ATTR_PAGE_SIZE {
        return setattr_size_mismatch(uattr);
    }
    let copy = (size as usize).min(SCHED_ATTR_SIZE_VER1 as usize);
    let bytes = uattr.load_vm_vec(copy)?;
    if size > SCHED_ATTR_SIZE_VER1 {
        let extra =
            UserConstPtr::<u8>::from(uattr.as_ptr() as usize + SCHED_ATTR_SIZE_VER1 as usize)
                .load_vm_vec((size - SCHED_ATTR_SIZE_VER1) as usize)?;
        if extra.iter().any(|&byte| byte != 0) {
            return setattr_size_mismatch(uattr);
        }
    }
    Ok(user_sched_attr_from_bytes(&bytes, size))
}

fn setattr_size_mismatch(uattr: UserConstPtr<u8>) -> KResult<UserSchedAttr> {
    // Linux ignores a failed `put_user` of the kernel size and still returns E2BIG.
    let _ = UserPtr::<u32>::from(uattr.as_ptr() as usize).write_vm(SCHED_ATTR_SIZE_VER1);
    Err(KError::ArgumentListTooLong)
}

fn user_sched_attr_from_bytes(bytes: &[u8], size: u32) -> UserSchedAttr {
    UserSchedAttr {
        size,
        sched_policy: read_u32(bytes, 4),
        sched_flags: read_u64(bytes, 8),
        sched_nice: read_u32(bytes, 16) as i32,
        sched_priority: read_u32(bytes, 20),
        sched_runtime: read_u64(bytes, 24),
        sched_deadline: read_u64(bytes, 32),
        sched_period: read_u64(bytes, 40),
    }
}

fn write_sched_attr_to_user(
    uattr: UserPtr<u8>,
    attr: &UserSchedAttr,
    user_size: u32,
) -> KResult<()> {
    let bytes = user_sched_attr_to_bytes(attr);
    let copy = (user_size as usize).min(bytes.len());
    uattr.write_vm_slice(&bytes[..copy])?;
    if (user_size as usize) > bytes.len() {
        zero_user_bytes(
            UserPtr::from(uattr.as_ptr() as usize + bytes.len()),
            user_size as usize - bytes.len(),
        )?;
    }
    Ok(())
}

fn zero_user_bytes(start: UserPtr<u8>, len: usize) -> KResult<()> {
    let zeros = [0u8; 64];
    let mut addr = start.as_ptr() as usize;
    let mut left = len;
    while left > 0 {
        let n = left.min(zeros.len());
        UserPtr::<u8>::from(addr).write_vm_slice(&zeros[..n])?;
        addr += n;
        left -= n;
    }
    Ok(())
}

fn user_sched_attr_to_bytes(attr: &UserSchedAttr) -> [u8; SCHED_ATTR_SIZE_VER1 as usize] {
    let mut bytes = [0u8; SCHED_ATTR_SIZE_VER1 as usize];
    write_u32(&mut bytes, 0, attr.size);
    write_u32(&mut bytes, 4, attr.sched_policy);
    write_u64(&mut bytes, 8, attr.sched_flags);
    write_u32(&mut bytes, 16, attr.sched_nice as u32);
    write_u32(&mut bytes, 20, attr.sched_priority);
    write_u64(&mut bytes, 24, attr.sched_runtime);
    write_u64(&mut bytes, 32, attr.sched_deadline);
    write_u64(&mut bytes, 40, attr.sched_period);
    bytes
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut raw = [0u8; 4];
    if offset + 4 <= bytes.len() {
        raw.copy_from_slice(&bytes[offset..offset + 4]);
    }
    u32::from_ne_bytes(raw)
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut raw = [0u8; 8];
    if offset + 8 <= bytes.len() {
        raw.copy_from_slice(&bytes[offset..offset + 8]);
    }
    u64::from_ne_bytes(raw)
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
}

fn raw_priority(nice: NiceValue) -> isize {
    nice.getpriority_raw()
}

fn process_raw_priority(proc: &kprocess::Process) -> KResult<isize> {
    process_min_nice(proc)
        .map(raw_priority)
        .ok_or(KError::NoSuchProcess)
}

fn check_setpriority_permission(
    caller: &Cred,
    target: &Cred,
    current_nice: NiceValue,
    requested_nice: NiceValue,
) -> KResult<()> {
    check_scheduler_identity_or_privilege(caller, target)?;
    if requested_nice < current_nice && !caller.has_cap(kcred::CAP_SYS_NICE) {
        return Err(KError::PermissionDenied);
    }
    check_scheduler_capability_permission(caller, target)
}

fn set_task_nice(task: &KtaskRef, nice: NiceValue, caller: &Cred) -> KResult<()> {
    let thread = task.try_as_thread().ok_or(KError::NoSuchProcess)?;
    check_setpriority_permission(caller, &thread.real_cred(), thread.nice(), nice)?;
    thread.set_nice(nice);
    if !ktask::set_task_prio(task, nice.fair_scheduler_priority()) {
        debug!("scheduler did not apply nice={nice:?} to the selected task");
    }
    Ok(())
}

fn update_process_nice(
    process: &Process,
    nice: NiceValue,
    caller: &Cred,
    result: &mut NiceUpdateResult,
) {
    for task in kprocess::scheduler::process_tasks(process) {
        if task.try_as_thread().is_none() {
            continue;
        }
        result.record(set_task_nice(&task, nice, caller));
    }
}

#[derive(Default)]
struct NiceUpdateResult {
    found: bool,
    error: Option<KError>,
}

impl NiceUpdateResult {
    fn record(&mut self, result: KResult<()>) {
        self.found = true;
        if let Err(error) = result {
            self.error = Some(error);
        }
    }

    fn finish(self) -> KResult<()> {
        if !self.found {
            return Err(KError::NoSuchProcess);
        }
        if let Some(error) = self.error {
            return Err(error);
        }
        Ok(())
    }
}

fn process_min_nice(proc: &kprocess::Process) -> Option<NiceValue> {
    let mut min_nice = None;
    for task in kprocess::scheduler::process_tasks(proc) {
        let Some(thread) = task.try_as_thread() else {
            continue;
        };
        update_min_nice(&mut min_nice, Some(thread.nice()));
        if min_nice == Some(NiceValue::MIN) {
            break;
        }
    }
    min_nice
}

fn update_min_nice(min_nice: &mut Option<NiceValue>, nice: Option<NiceValue>) {
    let Some(nice) = nice else {
        return;
    };
    *min_nice = Some(min_nice.map_or(nice, |current| current.min(nice)));
}

unittest::path_mod_test!(
    scheduler_query_tests,
    "../../ut-tests/task/sched/scheduler_query_tests.rs"
);
unittest::path_mod_test!(
    scheduler_abi_tests,
    "../../ut-tests/task/sched/scheduler_abi_tests.rs"
);

unittest::path_mod_test!(tests, "../../ut-tests/task/sched/tests.rs");
