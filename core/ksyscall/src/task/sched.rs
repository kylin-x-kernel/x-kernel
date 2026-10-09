// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduling-related syscall adapters tied to task state.

use kcred::Cred;
use kerrno::{KError, KResult};
use khal::percpu::this_cpu_id;
use kprocess::{AsThread, NiceValue, Process};
use ktask::{KCpuMask, KtaskRef, current};
use linux_raw_sys::general::{
    PRIO_PGRP, PRIO_PROCESS, PRIO_USER, SCHED_BATCH, SCHED_FIFO, SCHED_IDLE, SCHED_NORMAL, SCHED_RR,
};
use posix_types::{UserConstPtr, UserPtr, UserRead, UserWrite};

const MAX_REALTIME_PRIORITY: i32 = 99;

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
    cpusetsize: usize,
    user_mask: UserPtr<u8>,
) -> KResult<isize> {
    if cpusetsize * 8 < kbuild_config::NR_CPUS {
        return Err(KError::InvalidInput);
    }

    let task = affinity_target(pid)?;
    let mask = task.cpumask();
    let mask_bytes = mask.as_bytes();
    user_mask.write_vm_slice(mask_bytes)?;
    Ok(mask_bytes.len() as _)
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
    if !ktask::set_task_affinity(&task, cpu_mask) {
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

/// Shared owner gate for scheduling setters and `sched_setaffinity`.
///
/// Caller euid must match the target's ruid or euid, or the caller must be
/// privileged (`euid == 0`, standing in for `CAP_SYS_NICE`).
fn check_scheduler_owner_permission(caller: &Cred, target: &Cred) -> KResult<()> {
    if !caller.is_privileged() && caller.euid() != target.ruid() && caller.euid() != target.euid() {
        return Err(KError::OperationNotPermitted);
    }
    Ok(())
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

/// Returns the policy for a thread ID, or the calling thread when `pid` is zero.
///
/// A positive process PID denotes its leader TID, even from a worker caller.
/// There is no fallback to another process member if that TID is unpublished.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a negative identifier and
/// [`KError::NoSuchProcess`] for an absent or unpublished positive TID.
pub(crate) fn sys_sched_getscheduler(pid: i32) -> KResult<isize> {
    let task = scheduler_query_target(pid)?;

    Ok(task
        .as_thread()
        .scheduler_policy()
        .unwrap_or_else(configured_scheduler_policy) as _)
}

/// Sets policy for a published TID, or the caller when `pid` is zero.
///
/// An absent, unpublished, or negative target returns [`KError::NoSuchProcess`].
/// Unprivileged callers whose euid matches neither target ruid nor euid
/// receive [`KError::OperationNotPermitted`], even within one thread group.
/// Raw credential syscalls update only the calling thread; libc is responsible
/// for synchronizing credentials across POSIX threads.
pub(crate) fn sys_sched_setscheduler(
    pid: i32,
    policy: i32,
    param: UserConstPtr<()>,
) -> KResult<isize> {
    let task = scheduler_target(pid)?;
    let policy = validate_scheduler_policy(policy)?;
    let param = param.cast::<SchedParam>().read_vm()?;

    validate_scheduler_priority(policy, param.sched_priority)?;
    check_scheduler_owner_permission(&kprocess::current_cred(), &task.as_thread().real_cred())?;
    task.as_thread().set_scheduler(policy, param.sched_priority);

    Ok(0)
}

/// Returns parameters for a thread ID, or the calling thread when `pid` is zero.
///
/// A positive process PID denotes its leader TID, even from a worker caller.
/// There is no fallback to another process member if that TID is unpublished.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for a negative identifier and
/// [`KError::NoSuchProcess`] for an absent or unpublished positive TID.
/// A failure to write `param` propagates the user-memory access error.
pub(crate) fn sys_sched_getparam(pid: i32, param: UserPtr<()>) -> KResult<isize> {
    let task = scheduler_query_target(pid)?;
    let param = param.cast::<SchedParam>();

    param.write_vm(SchedParam {
        sched_priority: task.as_thread().scheduler_priority(),
    })?;

    Ok(0)
}

/// Updates parameters for a published TID, or the caller when `pid` is zero.
///
/// The policy is retained. An absent, unpublished, or negative target returns
/// [`KError::NoSuchProcess`]. Unrelated unprivileged callers receive
/// [`KError::OperationNotPermitted`] before the target metadata changes.
pub(crate) fn sys_sched_setparam(pid: i32, param: UserConstPtr<()>) -> KResult<isize> {
    let task = scheduler_target(pid)?;
    let thread = task.as_thread();
    let param = param.cast::<SchedParam>().read_vm()?;

    check_scheduler_owner_permission(&kprocess::current_cred(), &thread.real_cred())?;
    thread.set_scheduler_priority_with(
        param.sched_priority,
        configured_scheduler_policy(),
        validate_scheduler_priority,
    )?;

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
    // Preserve the existing setter ESRCH ABI while correcting published-TID
    // lookup. Linux returns EINVAL for negative IDs; aligning that errno is a
    // separate compatibility change, rather than part of this target fix.
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

fn scheduler_query_target(pid: i32) -> KResult<KtaskRef> {
    // pthread_getschedparam queries worker TIDs. Keep this read-only lookup
    // separate so getters keep their EINVAL contract for negative IDs.
    // Do not fall back to a representative thread: getpid() denotes the leader
    // TID, and an unpublished leader remains ESRCH. Negative getter IDs return
    // EINVAL; setters retain their existing ESRCH behavior for negative IDs.
    if pid < 0 {
        return Err(KError::InvalidInput);
    }
    if pid == 0 {
        return Ok(current().clone());
    }
    kprocess::scheduler::task_by_tid(pid as u32)
}

fn configured_scheduler_policy() -> u32 {
    SCHED_NORMAL
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
    if !caller.is_privileged() && caller.euid() != target.ruid() && caller.euid() != target.euid() {
        return Err(KError::OperationNotPermitted);
    }
    if requested_nice < current_nice && !caller.is_privileged() {
        return Err(KError::PermissionDenied);
    }
    Ok(())
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

#[unittest::mod_test]
mod scheduler_query_tests {
    use alloc::sync::Arc;

    use kerrno::KError;
    use kprocess::AsThread;
    use kspin::SpinNoPreempt;
    use ktask::{TaskInner, current};
    use linux_raw_sys::general::{SCHED_FIFO, SCHED_NORMAL};
    use posix_types::{UserConstPtr, UserPtr};
    use unittest::{TestResult, assert, assert_eq, def_test};
    use unittest_support::TestUserValue;

    use super::{
        SchedParam, scheduler_target, sys_sched_getparam, sys_sched_getscheduler,
        sys_sched_setparam, sys_sched_setscheduler,
    };

    fn run_worker(check_fn: fn() -> TestResult) -> TestResult {
        let prepared = kprocess::current_user_thread()
            .prepare_thread_clone()
            .expect("prepare scheduler query worker");
        let page_table_root = prepared.page_table_root();
        let (thread, task_number) = prepared.into_parts();
        let result = Arc::new(SpinNoPreempt::new(None));
        let worker_result = result.clone();
        let mut task = TaskInner::new_user(
            move || {
                let outcome = check_fn();
                *worker_result.lock() = Some(outcome);
                // Remove this fixture's published TID before its task exits.
                posix_process::do_exit(0, false);
            },
            "scheduler-query-worker".into(),
            task_number,
            thread,
        );
        task.ctx_mut().set_page_table_root(page_table_root);
        task.set_cpumask(current().cpumask());
        let worker = kprocess::start_user_task(task);
        worker.join();
        let outcome = result.lock().take().unwrap_or(TestResult::Failed);
        drop(worker);
        outcome
    }

    fn check_worker_queries() -> TestResult {
        let caller = current();
        let thread = caller.as_thread();
        let tid = thread.tid() as i32;
        assert!(tid != thread.pid() as i32, "fixture must be a non-leader");
        // Distinct fixture metadata makes a wrong representative observable.
        thread.set_scheduler(SCHED_FIFO, 7);
        let mut output = TestUserValue::from_value(SchedParam { sched_priority: -1 }).unwrap();
        for pid in [0, tid] {
            assert_eq!(sys_sched_getscheduler(pid), Ok(SCHED_FIFO as isize));
            output.write(SchedParam { sched_priority: -1 });
            assert_eq!(
                sys_sched_getparam(pid, UserPtr::from(output.as_user_ptr()).cast()),
                Ok(0)
            );
            assert_eq!(output.read().sched_priority, 7);
        }
        TestResult::Ok
    }

    fn check_worker_setter_thread_targets() -> TestResult {
        let caller = current();
        let thread = caller.as_thread();
        let pid = thread.pid() as i32;
        let tid = thread.tid() as i32;
        assert!(tid != pid, "fixture must be a non-leader");
        let leader = scheduler_target(pid).expect("published process leader");
        assert!(!caller.ptr_eq(&leader));
        assert_eq!(leader.as_thread().tid() as i32, pid);
        let leader_policy = leader.as_thread().scheduler_policy();
        let leader_priority = leader.as_thread().scheduler_priority();
        let mut param = TestUserValue::from_value(SchedParam { sched_priority: 7 }).unwrap();
        for target in [0, tid] {
            let resolved = scheduler_target(target).expect("calling worker target");
            assert!(caller.ptr_eq(&resolved));
            assert_eq!(
                sys_sched_setscheduler(
                    target,
                    SCHED_FIFO as i32,
                    UserConstPtr::from(param.as_user_ptr().addr()),
                ),
                Ok(0)
            );
            assert_eq!(sys_sched_getscheduler(tid), Ok(SCHED_FIFO as isize));
            param.write(SchedParam { sched_priority: 8 });
            assert_eq!(
                sys_sched_setparam(target, UserConstPtr::from(param.as_user_ptr().addr())),
                Ok(0)
            );
            assert_eq!(thread.scheduler_priority(), 8);
            assert_eq!(leader.as_thread().scheduler_policy(), leader_policy);
            assert_eq!(leader.as_thread().scheduler_priority(), leader_priority);
            param.write(SchedParam { sched_priority: 7 });
        }
        // Reapplying the leader's own parameter must not modify this worker.
        param.write(SchedParam {
            sched_priority: leader_priority,
        });
        assert_eq!(
            sys_sched_setparam(pid, UserConstPtr::from(param.as_user_ptr().addr())),
            Ok(0)
        );
        assert_eq!(thread.scheduler_priority(), 8);
        assert_eq!(leader.as_thread().scheduler_priority(), leader_priority);
        TestResult::Ok
    }

    #[def_test(user, serial)]
    fn scheduler_queries_reject_negative_and_missing_ids() {
        for pid in [-1, i32::MIN] {
            assert_eq!(sys_sched_getscheduler(pid), Err(KError::InvalidInput));
            assert_eq!(
                sys_sched_getparam(pid, UserPtr::default()),
                Err(KError::InvalidInput)
            );
        }
        assert_eq!(sys_sched_getscheduler(i32::MAX), Err(KError::NoSuchProcess));
        assert_eq!(
            sys_sched_getparam(i32::MAX, UserPtr::default()),
            Err(KError::NoSuchProcess)
        );
    }

    #[def_test(user, serial)]
    fn scheduler_worker_queries_resolve_zero_and_current_tid() -> TestResult {
        run_worker(check_worker_queries)
    }

    #[def_test(user, serial)]
    fn scheduler_query_and_setters_address_published_worker_tid() -> TestResult {
        let prepared = kprocess::current_user_thread()
            .prepare_thread_clone()
            .expect("prepare published scheduler query worker");
        let tid = prepared.tid() as i32;
        let page_table_root = prepared.page_table_root();
        let (thread, task_number) = prepared.into_parts();
        thread.set_scheduler(SCHED_FIFO, 7);
        let mut task = TaskInner::new_user(
            || {},
            "scheduler-inactive-worker".into(),
            task_number,
            thread,
        );
        task.ctx_mut().set_page_table_root(page_table_root);
        let published = kprocess::publish_user_task(task).unwrap();
        // This owned publication stays live deterministically until abort;
        // assertion returns still drop the handle and roll back publication.
        let outcome = (|| -> TestResult {
            assert!(tid != kprocess::current_user_thread().pid() as i32);
            let param = TestUserValue::from_value(SchedParam { sched_priority: 0 }).unwrap();
            let output = TestUserValue::from_value(SchedParam { sched_priority: -1 }).unwrap();
            assert_eq!(sys_sched_getscheduler(tid), Ok(SCHED_FIFO as isize));
            assert_eq!(
                sys_sched_getparam(tid, UserPtr::from(output.as_user_ptr()).cast()),
                Ok(0)
            );
            assert_eq!(output.read().sched_priority, 7);
            // The published worker stays root-owned while the caller drops
            // its credentials; both actual setters must deny cross-owner writes.
            let caller = kprocess::current_user_thread();
            let saved_cred = caller.cred();
            let denied = (|| -> TestResult {
                // unittest_support starts each user test with initial_cred();
                // fail explicitly if that root-owned fixture contract changes.
                assert_eq!(saved_cred.ruid(), 0);
                assert_eq!(saved_cred.euid(), 0);
                assert_eq!(crate::task::credentials::sys_setuid(3000), Ok(0));
                assert_eq!(caller.cred().euid(), 3000);
                assert_eq!(published.task().as_thread().pid(), caller.pid());
                assert_eq!(
                    published.task().as_thread().real_cred().ruid(),
                    saved_cred.ruid()
                );
                assert_eq!(
                    published.task().as_thread().real_cred().euid(),
                    saved_cred.euid()
                );
                assert_eq!(
                    sys_sched_setscheduler(
                        tid,
                        SCHED_NORMAL as i32,
                        UserConstPtr::from(param.as_user_ptr().addr())
                    ),
                    Err(KError::OperationNotPermitted)
                );
                let rt_param = TestUserValue::from_value(SchedParam { sched_priority: 8 }).unwrap();
                assert_eq!(
                    sys_sched_setparam(tid, UserConstPtr::from(rt_param.as_user_ptr().addr())),
                    Err(KError::OperationNotPermitted)
                );
                assert_eq!(sys_sched_getscheduler(tid), Ok(SCHED_FIFO as isize));
                assert_eq!(
                    sys_sched_getparam(tid, UserPtr::from(output.as_user_ptr()).cast()),
                    Ok(0)
                );
                assert_eq!(output.read().sched_priority, 7);
                TestResult::Ok
            })();
            // Restore credentials even when an inner assertion returns failure.
            caller.commit_creds((*saved_cred).clone());
            assert_eq!(denied, TestResult::Ok);
            for pid in [-1, i32::MIN, i32::MAX] {
                assert_eq!(
                    sys_sched_setscheduler(
                        pid,
                        SCHED_NORMAL as i32,
                        UserConstPtr::from(param.as_user_ptr().addr()),
                    ),
                    Err(KError::NoSuchProcess)
                );
                assert_eq!(
                    sys_sched_setparam(pid, UserConstPtr::from(param.as_user_ptr().addr())),
                    Err(KError::NoSuchProcess)
                );
            }
            assert_eq!(
                sys_sched_setscheduler(
                    tid,
                    SCHED_NORMAL as i32,
                    UserConstPtr::from(param.as_user_ptr().addr()),
                ),
                Ok(0)
            );
            assert_eq!(
                sys_sched_setparam(tid, UserConstPtr::from(param.as_user_ptr().addr())),
                Ok(0)
            );
            assert_eq!(sys_sched_getscheduler(tid), Ok(SCHED_NORMAL as isize));
            assert_eq!(
                sys_sched_getparam(tid, UserPtr::from(output.as_user_ptr()).cast()),
                Ok(0)
            );
            assert_eq!(output.read().sched_priority, 0);
            TestResult::Ok
        })();
        published.abort();
        // An unpublished worker TID must no longer be a read or write target.
        assert_eq!(sys_sched_getscheduler(tid), Err(KError::NoSuchProcess));
        assert_eq!(
            sys_sched_setscheduler(tid, SCHED_NORMAL as i32, UserConstPtr::default()),
            Err(KError::NoSuchProcess)
        );
        assert_eq!(
            sys_sched_setparam(tid, UserConstPtr::default()),
            Err(KError::NoSuchProcess)
        );
        assert_eq!(
            sys_sched_getparam(tid, UserPtr::default()),
            Err(KError::NoSuchProcess)
        );
        outcome
    }

    #[def_test(user, serial)]
    fn scheduler_setters_resolve_worker_and_leader_tids() -> TestResult {
        run_worker(check_worker_setter_thread_targets)
    }
}

#[unittest::mod_test]
mod tests {
    use kcred::Cred;
    use kerrno::KError;
    use kprocess::NiceValue;
    use ktask::{KCpuMask, current};
    use unittest::{assert, assert_eq, def_test};

    use super::{
        affinity_target, check_scheduler_owner_permission, check_setpriority_permission,
        prepare_setaffinity_target,
    };

    fn assert_affinity_err<T>(result: Result<T, KError>, expected: KError) {
        match result {
            Err(err) if err == expected => {}
            Err(err) => panic!("expected {expected:?}, got {err:?}"),
            Ok(_) => panic!("expected {expected:?}, got Ok"),
        }
    }

    #[def_test]
    fn getcpu_ignores_legacy_cache_with_null_outputs() {
        for cache in [0, 1, usize::MAX] {
            assert_eq!(
                super::sys_getcpu(
                    posix_types::UserPtr::default(),
                    posix_types::UserPtr::default(),
                    cache,
                ),
                Ok(0)
            );
        }
    }

    #[def_test]
    fn affinity_target_rejects_negative_pid() {
        assert_affinity_err(affinity_target(-1), KError::NoSuchProcess);
        assert_affinity_err(affinity_target(i32::MIN), KError::NoSuchProcess);
    }

    #[def_test]
    fn affinity_target_missing_id_is_no_such_process() {
        // Neither a published tid nor a live tgid.
        assert_affinity_err(affinity_target(i32::MAX), KError::NoSuchProcess);
    }

    #[def_test(user, serial)]
    fn affinity_target_zero_and_current_tid_resolve_caller() {
        let via_zero = affinity_target(0).expect("pid 0 is the caller");
        assert!(current().ptr_eq(&via_zero));

        let tid = kprocess::current_user_tid() as i32;
        let via_tid = affinity_target(tid).expect("caller tid must resolve");
        assert!(current().ptr_eq(&via_tid));
    }

    #[def_test(user, serial)]
    fn setaffinity_missing_pid_precedes_empty_mask_einval() {
        let empty = KCpuMask::new();
        assert_affinity_err(
            prepare_setaffinity_target(i32::MAX, &empty),
            KError::NoSuchProcess,
        );
    }

    #[def_test(user, serial)]
    fn setaffinity_empty_mask_is_einval_after_resolve() {
        let empty = KCpuMask::new();
        assert_affinity_err(prepare_setaffinity_target(0, &empty), KError::InvalidInput);
    }

    #[def_test]
    fn setaffinity_requires_matching_effective_uid() {
        let mut target = Cred::root();
        target
            .set_resuid(Some(1000), Some(2000), Some(1000))
            .unwrap();

        assert!(check_scheduler_owner_permission(&Cred::new(1000, 1), &target).is_ok());
        assert!(check_scheduler_owner_permission(&Cred::new(2000, 1), &target).is_ok());
        assert_eq!(
            check_scheduler_owner_permission(&Cred::new(3000, 1), &target),
            Err(KError::OperationNotPermitted)
        );
        assert!(check_scheduler_owner_permission(&Cred::root(), &target).is_ok());
    }

    #[def_test]
    fn setpriority_requires_matching_effective_uid() {
        let mut target = Cred::root();
        target
            .set_resuid(Some(1000), Some(2000), Some(1000))
            .unwrap();

        assert!(
            check_setpriority_permission(
                &Cred::new(1000, 1),
                &target,
                NiceValue::DEFAULT,
                NiceValue::new_clamped(1),
            )
            .is_ok()
        );
        assert!(
            check_setpriority_permission(
                &Cred::new(2000, 1),
                &target,
                NiceValue::DEFAULT,
                NiceValue::new_clamped(1),
            )
            .is_ok()
        );
        assert_eq!(
            check_setpriority_permission(
                &Cred::new(3000, 1),
                &target,
                NiceValue::DEFAULT,
                NiceValue::new_clamped(1),
            ),
            Err(KError::OperationNotPermitted)
        );
    }

    #[def_test]
    fn setpriority_raise_requires_privilege() {
        let target = Cred::new(1000, 1);

        assert_eq!(
            check_setpriority_permission(
                &target,
                &target,
                NiceValue::new_clamped(5),
                NiceValue::DEFAULT,
            ),
            Err(KError::PermissionDenied)
        );
        assert!(
            check_setpriority_permission(
                &Cred::root(),
                &target,
                NiceValue::new_clamped(5),
                NiceValue::DEFAULT,
            )
            .is_ok()
        );
    }
}
