// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, format, string::String, sync::Arc, vec, vec::Vec};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

use kcred::initial_cred;
use khal::{
    mem::{PAGE_SIZE_4K, VirtAddr},
    paging::MappingFlags,
};
use kpoll::PollRegistrations;
use ksignal::Signo;
use ktask::{TaskInner, prepare_task};
use ktime_types::TimeSpan;
use memspace::VmRuntimeRef;
use unittest::{assert, assert_eq, def_test};

use crate::{
    AsThread, ForkAddressSpace, ForkFdTable, ForkFs, ForkParent, ForkSignalActions, Process,
    ProcessExitPublication, ProcessForkConfig, build_process_thread,
    process::INIT_PROC,
    process_exit, process_signals, procfs,
    publication::{prepare_user_task, process_publication},
    publish_user_task, scheduler, wait_reap,
};

fn new_wake_counter() -> &'static AtomicUsize {
    Box::leak(Box::new(AtomicUsize::new(0)))
}

#[def_test]
fn test_shared_signal_exec_and_clone_concurrently_preserve_peer() {
    use ksignal::{
        SignalAction, SignalActionFlags, SignalDisposition, SignalSet, Signo,
        api::{ProcessSignalManager, SignalActions},
    };

    for iteration in 0..16 {
        let original = ProcessSignalManager::new(SignalActions::default(), 0);
        let mut mask = SignalSet::default();
        mask.add(Signo::SIGUSR2);
        original
            .set_signal_action(
                Signo::SIGUSR1,
                SignalAction {
                    disposition: SignalDisposition::Ignore,
                    flags: SignalActionFlags::RESTART,
                    mask,
                    restorer: None,
                },
            )
            .unwrap();
        let left = Arc::new(original.fork_shared().unwrap());
        let right = original.fork_shared().unwrap();
        let start = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(ktask::WaitQueue::new());
        let exec_valid = Arc::new(AtomicBool::new(false));
        let clone_valid = Arc::new(AtomicBool::new(false));
        let exec = ktask::spawn({
            let start = start.clone();
            let gate = gate.clone();
            let valid = exec_valid.clone();
            move || {
                gate.wait_until(|| start.load(Ordering::Acquire));
                right.reset_actions_for_exec().unwrap();
                valid.store(
                    right
                        .signal_action(Signo::SIGUSR1)
                        .unwrap()
                        .flags
                        .is_empty(),
                    Ordering::Release,
                );
            }
        });
        let clone = ktask::spawn({
            let left = left.clone();
            let start = start.clone();
            let gate = gate.clone();
            let valid = clone_valid.clone();
            move || {
                gate.wait_until(|| start.load(Ordering::Acquire));
                let child = if iteration % 2 == 0 {
                    left.fork_shared().unwrap()
                } else {
                    left.fork_private().unwrap()
                };
                let action = child.signal_action(Signo::SIGUSR1).unwrap();
                // Either side of exec is valid, but never a mixed snapshot.
                valid.store(
                    action.flags.is_empty() == action.mask.is_empty()
                        && matches!(action.disposition, SignalDisposition::Ignore),
                    Ordering::Release,
                );
            }
        });
        start.store(true, Ordering::Release);
        gate.notify_all(false);
        left.reset_actions_for_exec().unwrap();
        assert_eq!(exec.join(), 0);
        assert_eq!(clone.join(), 0);
        assert!(exec_valid.load(Ordering::Acquire));
        assert!(clone_valid.load(Ordering::Acquire));
        assert!(left.signal_action(Signo::SIGUSR1).unwrap().flags.is_empty());
        let action = original.signal_action(Signo::SIGUSR1).unwrap();
        assert!(action.flags.contains(SignalActionFlags::RESTART));
        assert!(action.mask.has(Signo::SIGUSR2));
    }
}

/// # Safety
///
/// `data` must be the pointer originally stored in a counter `RawWaker`.
unsafe fn counter_waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &COUNTER_WAKER_VTABLE)
}

/// # Safety
///
/// `data` must point to a leaked `'static` [`AtomicUsize`] created by
/// `new_wake_counter`.
unsafe fn counter_waker_wake(data: *const ()) {
    // SAFETY: `data` points to a leaked `'static` `AtomicUsize`, so shared
    // access through atomic operations is valid for the lifetime of the waker.
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

/// # Safety
///
/// `data` must point to a leaked `'static` [`AtomicUsize`] created by
/// `new_wake_counter`.
unsafe fn counter_waker_wake_by_ref(data: *const ()) {
    // SAFETY: `data` points to a leaked `'static` `AtomicUsize`, so shared
    // access through atomic operations is valid for the lifetime of the waker.
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

/// # Safety
///
/// `data` must be the pointer originally stored in a counter `RawWaker`.
unsafe fn counter_waker_drop(_data: *const ()) {}

static COUNTER_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    counter_waker_clone,
    counter_waker_wake,
    counter_waker_wake_by_ref,
    counter_waker_drop,
);

fn counter_waker(counter: &'static AtomicUsize) -> Waker {
    let raw = RawWaker::new(counter as *const _ as *const (), &COUNTER_WAKER_VTABLE);
    // SAFETY: `raw` is built from callbacks that preserve the `RawWaker`
    // contract for a leaked `'static` `AtomicUsize`.
    unsafe { Waker::from_raw(raw) }
}

fn register_poll_set(set: &kpoll::PollSet, waker: &Waker) -> PollRegistrations {
    let mut registrations = PollRegistrations::new();
    let cx = Context::from_waker(waker);
    registrations.context(&cx).register(set).unwrap();
    registrations
}

struct ExitAccountingObserver {
    process: Arc<Process>,
    expected_utime: TimeSpan,
    expected_stime: TimeSpan,
    failures: AtomicUsize,
}

fn new_exit_accounting_observer(
    process: Arc<Process>,
    expected_utime: TimeSpan,
    expected_stime: TimeSpan,
) -> &'static ExitAccountingObserver {
    Box::leak(Box::new(ExitAccountingObserver {
        process,
        expected_utime,
        expected_stime,
        failures: AtomicUsize::new(0),
    }))
}

/// # Safety
///
/// `data` must be the pointer originally stored in an accounting observer
/// `RawWaker`.
unsafe fn accounting_waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &ACCOUNTING_WAKER_VTABLE)
}

/// # Safety
///
/// `data` must point to a leaked `'static` [`ExitAccountingObserver`] created
/// by `new_exit_accounting_observer`.
unsafe fn accounting_waker_wake(data: *const ()) {
    // SAFETY: `data` points to a leaked `'static` observer used only through
    // shared references and atomics.
    let observer = unsafe { &*(data as *const ExitAccountingObserver) };
    let observed = observer.process.exited_thread_time();
    if observed != (observer.expected_utime, observer.expected_stime) {
        observer.failures.fetch_add(1, Ordering::SeqCst);
    }
}

/// # Safety
///
/// `data` must point to a leaked `'static` [`ExitAccountingObserver`] created
/// by `new_exit_accounting_observer`.
unsafe fn accounting_waker_wake_by_ref(data: *const ()) {
    // SAFETY: same invariant as `accounting_waker_wake`; the observer is
    // leaked for the test lifetime.
    unsafe { accounting_waker_wake(data) };
}

/// # Safety
///
/// `data` must be the pointer originally stored in an accounting observer
/// `RawWaker`.
unsafe fn accounting_waker_drop(_data: *const ()) {}

static ACCOUNTING_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    accounting_waker_clone,
    accounting_waker_wake,
    accounting_waker_wake_by_ref,
    accounting_waker_drop,
);

fn accounting_waker(observer: &'static ExitAccountingObserver) -> Waker {
    let raw = RawWaker::new(observer as *const _ as *const (), &ACCOUNTING_WAKER_VTABLE);
    // SAFETY: `raw` is built from callbacks that preserve the `RawWaker`
    // contract for a leaked `'static` observer.
    unsafe { Waker::from_raw(raw) }
}

struct ChildExitWakeObserver {
    parent: Arc<Process>,
    child: Arc<Process>,
    failures: AtomicUsize,
}

fn new_child_exit_wake_observer(
    parent: Arc<Process>,
    child: Arc<Process>,
) -> &'static ChildExitWakeObserver {
    Box::leak(Box::new(ChildExitWakeObserver {
        parent,
        child,
        failures: AtomicUsize::new(0),
    }))
}

/// # Safety
///
/// `data` must be the pointer originally stored in a child-exit observer
/// `RawWaker`.
unsafe fn child_exit_waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &CHILD_EXIT_WAKER_VTABLE)
}

/// # Safety
///
/// `data` must point to a leaked `'static` [`ChildExitWakeObserver`] created by
/// `new_child_exit_wake_observer`.
unsafe fn child_exit_waker_wake(data: *const ()) {
    // SAFETY: `data` points to a leaked `'static` observer used only through
    // shared references and atomics.
    let observer = unsafe { &*(data as *const ChildExitWakeObserver) };
    let child_is_linked = observer
        .parent
        .children()
        .iter()
        .any(|child| Arc::ptr_eq(child, &observer.child));
    if !observer.child.is_waitable_zombie() || !child_is_linked {
        observer.failures.fetch_add(1, Ordering::SeqCst);
    }
}

/// # Safety
///
/// `data` must point to a leaked `'static` [`ChildExitWakeObserver`] created by
/// `new_child_exit_wake_observer`.
unsafe fn child_exit_waker_wake_by_ref(data: *const ()) {
    // SAFETY: same invariant as `child_exit_waker_wake`; the observer is
    // leaked for the test lifetime.
    unsafe { child_exit_waker_wake(data) };
}

/// # Safety
///
/// `data` must be the pointer originally stored in a child-exit observer
/// `RawWaker`.
unsafe fn child_exit_waker_drop(_data: *const ()) {}

static CHILD_EXIT_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    child_exit_waker_clone,
    child_exit_waker_wake,
    child_exit_waker_wake_by_ref,
    child_exit_waker_drop,
);

fn child_exit_waker(observer: &'static ChildExitWakeObserver) -> Waker {
    let raw = RawWaker::new(observer as *const _ as *const (), &CHILD_EXIT_WAKER_VTABLE);
    // SAFETY: `raw` is built from callbacks that preserve the `RawWaker`
    // contract for a leaked `'static` observer.
    unsafe { Waker::from_raw(raw) }
}

fn ensure_init() -> Arc<Process> {
    if let Some(p) = INIT_PROC.get() {
        return p.clone();
    }

    // In unittest mode, INIT_PROC may already have been initialized by a
    // synthetic runtime used by earlier tests. If not, create one here.
    Process::new_init(1)
}

fn build_prepared_test_user_task() -> (Arc<Process>, TaskInner) {
    let parent = ensure_init();
    let task_number =
        kidentity::allocate_root_pid_handle().expect("test leader identity should allocate");
    let pid = task_number.root_nr();
    let process = parent.fork_with_task_number(task_number.clone(), Some(ksignal::Signo::SIGCHLD));

    let mut aspace = memspace::MmSpace::new_user_empty().expect("user mmspace should allocate");
    ksignal::map_signal_trampoline(&mut aspace).expect("signal trampoline should map");
    let address_space = Arc::new(ksync::Mutex::new(aspace));
    let fs_context = fs_context::copy_init_fs_struct();
    let signal_actions = ksignal::api::SignalActions::default();

    let thread = build_process_thread(
        process.clone(),
        task_number.clone(),
        String::from("[test-user-thread]"),
        Arc::new(vec![]),
        address_space,
        fs_context,
        signal_actions,
        initial_cred(),
    );
    let task = TaskInner::new_user(|| {}, String::from("test-user-thread"), task_number, thread);

    task.set_name(format!("test-user-thread-{pid}").as_str());
    (process, task)
}

fn publish_test_thread(process: &Arc<Process>, tid: crate::Tid) -> ktask::KtaskRef {
    let task_number = kidentity::PidHandle::fixed_root(tid);
    let mut aspace = memspace::MmSpace::new_user_empty().expect("user mmspace should allocate");
    ksignal::map_signal_trampoline(&mut aspace).expect("signal trampoline should map");
    let address_space = Arc::new(ksync::Mutex::new(aspace));
    let fs_context = fs_context::copy_init_fs_struct();
    let signal_actions = ksignal::api::SignalActions::default();
    let credentials = initial_cred();

    let thread = build_process_thread(
        process.clone(),
        task_number.clone(),
        String::from("[test-thread]"),
        Arc::new(vec![]),
        address_space,
        fs_context,
        signal_actions,
        credentials,
    );
    let task = TaskInner::new_user(|| {}, format!("test-thread-{tid}"), task_number, thread);

    let task = prepare_task(task);
    process_publication().publish_task(&task).unwrap();
    task
}

fn mapped_test_address_space() -> Arc<ksync::Mutex<memspace::MmSpace>> {
    let start = VirtAddr::from_usize(0x4000);
    let mut aspace = memspace::MmSpace::new_empty_user(start, PAGE_SIZE_4K * 4)
        .expect("user mmspace should allocate");
    aspace
        .map(
            start,
            PAGE_SIZE_4K,
            MappingFlags::READ | MappingFlags::WRITE,
            false,
            VmRuntimeRef::new_anon_private(start, khal::paging::PageSize::Size4K),
        )
        .expect("test mapping should install");
    Arc::new(ksync::Mutex::new(aspace))
}

pub(crate) fn process_with_address_space(
    pid: crate::Pid,
    address_space: Arc<ksync::Mutex<memspace::MmSpace>>,
) -> (Arc<Process>, TaskInner) {
    let parent = ensure_init();
    let task_number = kidentity::PidHandle::fixed_root(pid);
    let process = parent.fork_with_task_number(task_number.clone(), Some(ksignal::Signo::SIGCHLD));
    let fs_context = fs_context::copy_init_fs_struct();
    let signal_actions = ksignal::api::SignalActions::default();

    let thread = build_process_thread(
        process.clone(),
        task_number.clone(),
        String::from("[test-thread]"),
        Arc::new(vec![]),
        address_space,
        fs_context,
        signal_actions,
        initial_cred(),
    );
    let task = TaskInner::new_user(
        || {},
        format!("test-user-thread-{pid}"),
        task_number,
        thread,
    );

    (process, task)
}

#[def_test(serial)]
fn test_process_lifecycle() {
    let init = ensure_init();
    assert!(INIT_PROC.get().is_some());
    assert!(init.parent().is_none());

    // Test Fork
    let child_pid = 100;
    let child = init.fork(child_pid);
    assert_eq!(child.pid(), child_pid);

    let parent = child.parent().expect("Child must have a parent");
    assert_eq!(parent.pid(), init.pid());

    // Check children list of init
    let children = init.children();
    assert!(children.iter().any(|c| c.pid() == child_pid));

    // Test Process Group inheritance
    assert_eq!(child.group().pgid(), init.group().pgid());

    // Create new session for child
    let res = child.create_session();
    assert!(res.is_some());
    let (session, group) = res.unwrap();
    assert_eq!(session.sid(), child_pid);
    assert_eq!(group.pgid(), child_pid);
    assert_eq!(child.group().pgid(), child_pid);

    // Test Threads
    // Initially no threads (unless explicitly added in this implementation)
    // Verify threads count.
    assert!(!child.threads().contains(&child_pid));

    // Add main thread
    let leader_task = publish_test_thread(&child, child_pid);
    assert!(child.threads().contains(&child_pid));

    // Add secondary thread
    let sibling_task = publish_test_thread(&child, child_pid + 1);
    let threads = child.threads();
    assert!(threads.contains(&(child_pid + 1)));

    // Remove secondary thread
    let is_last = child.exit_thread(&sibling_task, 0);
    assert!(!is_last); // main thread is still there

    // Remove main thread
    let is_last = child.exit_thread(&leader_task, 0);
    assert!(is_last);

    // Test Group Exit
    assert!(!child.is_group_exited());
    child.group_exit();
    assert!(child.is_group_exited());

    // Test Exit
    assert!(!child.is_exited());
    child.exit_with_publication(ProcessExitPublication::WaitableZombie);
    assert!(child.is_exited());

    // Free
    child.free();
    // After free, it should be removed from parent
    let children_after = init.children();
    assert!(!children_after.iter().any(|c| c.pid() == child_pid));
}

#[def_test(serial)]
fn test_process_group_session() {
    let init = ensure_init();
    let p1 = init.fork(200);
    let p2 = init.fork(201);

    // Create a new session for p1
    let (s1, g1) = p1.create_session().expect("Failed to create session");
    assert_eq!(s1.sid(), 200);
    assert_eq!(g1.pgid(), 200);

    // Move p2 to p1's group - Should FAIL because they are in different sessions
    // p1 is in s1 (sid 200), p2 is in init's session (sid != 200)
    assert!(!p2.move_to_group(&g1));
    assert!(
        g1.processes()
            .iter()
            .all(|process| process.pid() != p2.pid()),
        "failed cross-session move must not publish target group membership"
    );

    // To test move_to_group successfully, we need a process in the SAME session.
    // Fork p1_child from p1. It inherits session s1.
    let p1_child = p1.fork(202);
    assert_eq!(p1_child.group().session().sid(), 200);
    assert_eq!(p1_child.group().pgid(), 200); // Inherits g1

    // Create a new group for p1_child
    let g_child = p1_child
        .create_group()
        .expect("Failed to create group for p1_child");
    assert_eq!(g_child.pgid(), 202);
    assert_eq!(p1_child.group().pgid(), 202);

    // Now move p1_child back to g1
    assert!(p1_child.move_to_group(&g1));
    assert_eq!(p1_child.group().pgid(), 200);

    // Clean up
    p1.exit_with_publication(ProcessExitPublication::WaitableZombie);
    p1.free();
    p2.exit_with_publication(ProcessExitPublication::WaitableZombie);
    p2.free();
    p1_child.exit_with_publication(ProcessExitPublication::WaitableZombie);
    p1_child.free();
}

#[def_test(serial)]
fn test_move_to_group_keeps_group_membership_consistent() {
    let init = ensure_init();
    let leader = init.fork(203);
    let (_session, primary_group) = leader
        .create_session()
        .expect("leader should create a private session");

    let mover = leader.fork(204);
    let alternate_group = mover
        .create_group()
        .expect("session peer should create an alternate group");

    assert!(
        primary_group
            .processes()
            .iter()
            .all(|process| process.pid() != mover.pid()),
        "creating a new group must remove the process from its old group membership"
    );
    assert!(
        alternate_group
            .processes()
            .iter()
            .any(|process| process.pid() == mover.pid()),
        "new group must list the moved process"
    );

    assert!(mover.move_to_group(&primary_group));
    assert!(
        primary_group
            .processes()
            .iter()
            .any(|process| process.pid() == mover.pid()),
        "target group must list the process after move_to_group"
    );
    assert!(
        alternate_group
            .processes()
            .iter()
            .all(|process| process.pid() != mover.pid()),
        "source group must stop listing the process after move_to_group"
    );

    mover.exit_with_publication(ProcessExitPublication::WaitableZombie);
    mover.free();
    leader.exit_with_publication(ProcessExitPublication::WaitableZombie);
    leader.free();
}

#[def_test(serial)]
fn test_exit_reparents_children_to_init() {
    let init = ensure_init();
    let parent = init.fork(300);
    let child = parent.fork(301);

    let child_parent = child
        .parent()
        .expect("child must have a parent before exit");
    assert_eq!(child_parent.pid(), parent.pid());
    assert!(
        parent
            .children()
            .iter()
            .any(|proc| proc.pid() == child.pid()),
        "parent should initially list its child"
    );

    parent.exit_with_publication(ProcessExitPublication::WaitableZombie);

    let reparented = child
        .parent()
        .expect("orphan child must be reparented to init");
    assert_eq!(
        reparented.pid(),
        init.pid(),
        "child should be reparented to init when its parent exits"
    );
    assert!(
        init.children().iter().any(|proc| proc.pid() == child.pid()),
        "init should observe reparented children after parent exit"
    );
    assert!(
        !parent
            .children()
            .iter()
            .any(|proc| proc.pid() == child.pid()),
        "exited parent should no longer own reparented children"
    );

    parent.free();
    child.exit_with_publication(ProcessExitPublication::WaitableZombie);
    child.free();
}

#[def_test(serial)]
fn test_exit_reparent_resets_child_exit_signal_to_sigchld() {
    let init = ensure_init();
    let parent = init.fork_with_exit_signal(302, Some(ksignal::Signo::SIGUSR1));
    let child = parent.fork_with_exit_signal(303, Some(ksignal::Signo::SIGUSR2));

    assert_eq!(child.exit_signal(), Some(ksignal::Signo::SIGUSR2));

    parent.exit_with_publication(ProcessExitPublication::WaitableZombie);

    let reparented = child.parent().expect("child should be reparented to init");
    assert_eq!(reparented.pid(), init.pid());
    assert_eq!(
        child.exit_signal(),
        Some(ksignal::Signo::SIGCHLD),
        "orphaned children should report to init using SIGCHLD"
    );

    parent.free();
    child.exit_with_publication(ProcessExitPublication::WaitableZombie);
    child.free();
}

#[def_test(serial)]
fn test_reparented_zombie_is_reported_to_its_new_parent() {
    // Linux `reparent_leader()` reports a zombie whose parent just changed with
    // `do_notify_parent()`, so the parent that inherits it is told about the
    // death. The report is prepared outside the process-domain lock; this case
    // pins what the exit transaction carries out of it (the synthetic test
    // processes have no signal runtime, so the delivery itself is only
    // observable in a live system).
    let init = ensure_init();
    let subreaper = init.fork(310);
    subreaper.set_child_subreaper(true);
    let parent = subreaper.fork(311);
    let zombie = parent.fork(312);

    zombie.exit_with_publication(ProcessExitPublication::WaitableZombie);
    let transition = parent.finish_exit_in_process_domain(
        ProcessExitPublication::WaitableZombie,
        |observed_parent| Some((observed_parent.pid(), false)),
    );

    assert_eq!(
        zombie.parent().map(|parent| parent.pid()),
        Some(subreaper.pid()),
        "the orphaned zombie must be adopted by the subreaper"
    );
    assert_eq!(
        transition.reparented_zombies.len(),
        1,
        "the exit transaction must carry the zombie handed to the new parent"
    );
    let reported = &transition.reparented_zombies[0];
    assert_eq!(reported.child.pid(), zombie.pid());
    assert_eq!(
        reported.parent.pid(),
        subreaper.pid(),
        "the report targets the subreaper that now owns the zombie"
    );
    assert_eq!(
        zombie.exit_signal(),
        Some(ksignal::Signo::SIGCHLD),
        "orphaned children report to the new parent using SIGCHLD"
    );

    parent.free();
    zombie.free();
    subreaper.exit_with_publication(ProcessExitPublication::WaitableZombie);
    subreaper.free();
}

#[def_test(serial)]
fn test_reap_after_reparent_removes_child_from_current_parent() {
    let init = ensure_init();
    let parent = init.fork(304);
    let child = parent.fork(305);

    child.exit_with_publication(ProcessExitPublication::WaitableZombie);
    parent.exit_with_publication(ProcessExitPublication::WaitableZombie);

    assert_eq!(
        child.parent().map(|parent| parent.pid()),
        Some(init.pid()),
        "exited child should be linked under init after old parent exits"
    );
    assert!(
        init.children().iter().any(|proc| proc.pid() == child.pid()),
        "init must own the reparented zombie before reap"
    );

    assert!(
        wait_reap::try_reap_zombie_process(&child),
        "reap should consume the zombie from its current parent relation"
    );
    assert!(
        !init.children().iter().any(|proc| proc.pid() == child.pid()),
        "reaped child must not remain linked under init"
    );
    assert!(
        init.children()
            .iter()
            .any(|proc| proc.pid() == parent.pid()),
        "reaping the reparented child must not detach the exited old parent"
    );

    parent.free();
}

#[def_test(serial)]
fn test_clone_parent_inherits_callers_exit_signal_contract() {
    let init = ensure_init();
    let parent_task_number = kidentity::PidHandle::fixed_root(1_500);
    let parent =
        init.fork_with_task_number(parent_task_number.clone(), Some(ksignal::Signo::SIGUSR1));
    let fs_context = fs_context::copy_init_fs_struct();
    let signal_actions = ksignal::api::SignalActions::default();
    let thread = build_process_thread(
        parent.clone(),
        parent_task_number.clone(),
        String::from("[clone-parent-test]"),
        Arc::new(vec![]),
        mapped_test_address_space(),
        fs_context,
        signal_actions,
        initial_cred(),
    );
    let task = TaskInner::new_user(
        || {},
        String::from("clone-parent-test"),
        parent_task_number,
        thread,
    );

    let prepared = task
        .as_thread()
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::CallerParent,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(ksignal::Signo::SIGUSR2),
        })
        .expect("CLONE_PARENT-style fork should prepare");
    let child = prepared.process().clone();

    assert_eq!(
        child.parent().map(|parent| parent.pid()),
        Some(init.pid()),
        "CLONE_PARENT child should be linked under the caller's parent"
    );
    assert_eq!(
        child.exit_signal(),
        Some(ksignal::Signo::SIGUSR1),
        "CLONE_PARENT child should inherit the caller's process exit signal"
    );

    child.discard_unpublished();
    parent.exit_with_publication(ProcessExitPublication::WaitableZombie);
    parent.free();
}

#[def_test(serial)]
fn test_clone_newnpid_allocates_child_in_fresh_pid_namespace() {
    let init = ensure_init();
    let parent_task_number = kidentity::PidHandle::fixed_root(1_700);
    let parent =
        init.fork_with_task_number(parent_task_number.clone(), Some(ksignal::Signo::SIGCHLD));
    let fs_context = fs_context::copy_init_fs_struct();
    let signal_actions = ksignal::api::SignalActions::default();
    let thread = build_process_thread(
        parent.clone(),
        parent_task_number.clone(),
        String::from("[newnpid-parent]"),
        Arc::new(vec![]),
        mapped_test_address_space(),
        fs_context,
        signal_actions,
        initial_cred(),
    );
    let task = TaskInner::new_user(
        || {},
        String::from("newnpid-parent"),
        parent_task_number,
        thread,
    );

    let parent_pid_ns = parent
        .pid_ns_for_children()
        .expect("parent runtime exposes the pid namespace");
    let prepared = task
        .as_thread()
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::NEWPID,
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        })
        .expect("CLONE_NEWPID fork should prepare");

    let child = prepared.process().clone();
    let child_pid_ns = child
        .pid_ns_for_children()
        .expect("child runtime exposes the pid namespace");

    // The child owns a fresh PID namespace nested one level below the parent's.
    assert!(!Arc::ptr_eq(&parent_pid_ns, &child_pid_ns));
    assert!(Arc::ptr_eq(
        child_pid_ns.parent().expect("child pid ns has parent"),
        &parent_pid_ns
    ));

    // The child leader is allocated inside the fresh namespace (number 1).
    // Following Linux `struct pid`/`upid`, the identity also carries fresh
    // projections for every ancestor namespace, including the parent's.
    let (_, task_number) = prepared.into_parts();
    assert_eq!(task_number.number_in(&child_pid_ns), Some(1));
    let parent_ns_nr = task_number
        .number_in(&parent_pid_ns)
        .expect("child identity holds a fresh number in the parent namespace too");
    assert_ne!(
        parent_ns_nr, 1,
        "parent namespace number is allocated independently"
    );
    assert_eq!(
        task_number.visible_number(&parent_pid_ns),
        Some(parent_ns_nr),
        "an observer in the parent namespace resolves the parent projection"
    );

    child.discard_unpublished();
    parent.exit_with_publication(ProcessExitPublication::WaitableZombie);
    parent.free();
}

#[def_test(serial)]
fn test_child_exit_si_pid_projected_into_parent_namespace() {
    // Two-level construction from the newnpid fixture: the root-namespace
    // parent forks a CLONE_NEWPID child (visible as 1 inside the fresh
    // namespace), and the child plain-forks a grandchild. Per the
    // pid_ns_for_children allocation rule the grandchild's number in the
    // child namespace differs from its root number, and si_pid must report
    // that child-namespace view to the waiting parent.
    let init = ensure_init();
    let parent_task_number = kidentity::PidHandle::fixed_root(1_800);
    let parent =
        init.fork_with_task_number(parent_task_number.clone(), Some(ksignal::Signo::SIGCHLD));
    let thread = build_process_thread(
        parent.clone(),
        parent_task_number.clone(),
        String::from("[nsi-pid-parent]"),
        Arc::new(vec![]),
        mapped_test_address_space(),
        fs_context::copy_init_fs_struct(),
        ksignal::api::SignalActions::default(),
        initial_cred(),
    );
    let task = TaskInner::new_user(
        || {},
        String::from("nsi-pid-parent"),
        parent_task_number,
        thread,
    );

    let prepared = task
        .as_thread()
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::NEWPID,
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        })
        .expect("CLONE_NEWPID fork should prepare");
    let nested = prepared.process().clone();
    let (nested_thread, nested_number) = prepared.into_parts();
    let nested_pid_ns = nested
        .pid_ns_for_children()
        .expect("nested runtime exposes the pid namespace");
    assert_eq!(nested_number.number_in(&nested_pid_ns), Some(1));

    // The nested child forks a grandchild WITHOUT CLONE_NEWPID: the
    // grandchild is allocated inside the inherited child namespace.
    let grand_prepared = nested_thread
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        })
        .expect("plain fork inside the child namespace should prepare");
    let grand = grand_prepared.process().clone();
    let (_, grand_number) = grand_prepared.into_parts();
    let grand_child_ns_nr = grand_number
        .number_in(&nested_pid_ns)
        .expect("grandchild identity holds a projection in the child namespace");

    // The exit-signal payload for the grandchild, as the waiting parent (the
    // nested process, whose active namespace is the child namespace)
    // observes it. The exact child-namespace number is the contract; the
    // root number is deliberately not compared, because the global root
    // counter's value depends on unrelated test ordering.
    let siginfo = process_exit::child_exit_signal_info(&grand, &nested, ksignal::Signo::SIGCHLD);
    let payload = siginfo
        .as_signal_info()
        .child_exit()
        .expect("child-exit signal should carry sigchld payload");
    assert_eq!(
        payload.pid(),
        grand_child_ns_nr,
        "si_pid must carry the parent-namespace number, not the root number"
    );

    grand.discard_unpublished();
    nested.discard_unpublished();
    parent.exit_with_publication(ProcessExitPublication::WaitableZombie);
    parent.free();
}

#[def_test(serial)]
fn test_apply_prepared_unshare_installs_namespace_bundle() {
    let (parent, task) = build_prepared_test_user_task();
    let thread = task.as_thread();

    let prepared = thread
        .process()
        .nsproxy()
        .expect("runtime exposes namespace bundle")
        .prepare_unshare(
            kns::NamespaceFlags::NEWUTS | kns::NamespaceFlags::NEWIPC,
            kns::UnshareContext {
                privileged: true,
                fs_shared_with_others: false,
                unshare_fs: false,
                unshare_files: false,
            },
            &thread.process().fs_context().expect("fs context").lock(),
        )
        .expect("unshare preparation should succeed");

    let before_uts = thread.process().uts_ns().expect("uts before");
    let before_ipc = thread.process().ipc_ns().expect("ipc before");
    thread
        .process()
        .apply_prepared_unshare(prepared)
        .expect("commit succeeds");

    let after_uts = thread.process().uts_ns().expect("uts after");
    let after_ipc = thread.process().ipc_ns().expect("ipc after");
    let after_nsproxy = thread.process().nsproxy().expect("bundle after");
    assert!(!Arc::ptr_eq(&before_uts, &after_uts));
    assert!(!Arc::ptr_eq(&before_ipc, &after_ipc));
    assert!(Arc::ptr_eq(&after_ipc, after_nsproxy.ipc_ns()));
    // Only requested slots changed: the mount namespace reference is preserved.
    assert!(Arc::ptr_eq(
        after_nsproxy.mnt_ns(),
        &thread.process().mnt_ns().expect("mnt after")
    ));

    // CLONE_FILES commits in two places: the prepared bundle installs on the
    // process, then the descriptor-table detach runs on the calling thread
    // (the syscall adapter's step, `sys_unshare`). This fixture only owns one
    // thread, so its fd table is already private and a detach here would be a
    // no-op; the prepare phase is asserted to record the request, and the
    // detach semantics themselves are covered by
    // `task_files_unshare_preserves_siblings_and_clone_files_policy`.
    let prepared_files = thread
        .process()
        .nsproxy()
        .expect("bundle")
        .prepare_unshare(
            kns::NamespaceFlags::empty(),
            kns::UnshareContext {
                privileged: false,
                fs_shared_with_others: false,
                unshare_fs: false,
                unshare_files: true,
            },
            &thread.process().fs_context().expect("fs context").lock(),
        )
        .expect("files-only unshare prepares");
    assert!(
        prepared_files.unshare_files(),
        "prepare must record the CLONE_FILES request for the commit phase"
    );
    thread
        .process()
        .apply_prepared_unshare(prepared_files)
        .expect("files unshare bundle commits");

    parent.exit_with_publication(ProcessExitPublication::WaitableZombie);
    parent.free();
}

#[def_test(serial)]
fn test_free_only_reaps_target_zombie_child() {
    let init = ensure_init();
    let first = init.fork(400);
    let second = init.fork(401);

    assert!(
        init.children().iter().any(|proc| proc.pid() == first.pid()),
        "init should initially list the first child"
    );
    assert!(
        init.children()
            .iter()
            .any(|proc| proc.pid() == second.pid()),
        "init should initially list the second child"
    );

    first.exit_with_publication(ProcessExitPublication::WaitableZombie);
    first.free();

    assert!(
        !init.children().iter().any(|proc| proc.pid() == first.pid()),
        "reaped zombie should be removed from parent children list"
    );
    assert!(
        init.children()
            .iter()
            .any(|proc| proc.pid() == second.pid()),
        "reaping one child must not hide live siblings"
    );

    second.exit_with_publication(ProcessExitPublication::WaitableZombie);
    second.free();
}

#[def_test(user, serial)]
fn test_exited_process_is_not_live_even_if_runtime_still_exists() {
    let (proc, prepared) = build_prepared_test_user_task();
    let publication = process_publication();

    assert!(proc.runtime_ref().is_some());
    publication.publish_process_identity(&proc);
    assert!(
        publication.live_process(proc.pid()).is_ok(),
        "non-exited process should be live before exit"
    );

    process_exit::finalize_process_exit(&proc);

    assert!(proc.is_exited());
    assert!(
        proc.runtime_ref().is_some(),
        "runtime may still exist while the owning thread object is not dropped yet"
    );
    assert!(
        publication.live_process(proc.pid()).is_err(),
        "exited process must stop participating in live-process lookups even before runtime drops"
    );

    drop(prepared);
    wait_reap::assert_reap_zombie_process(&proc);
}

#[def_test(user, serial)]
fn test_signal_send_reaches_published_zombie_until_reaped() {
    let (proc, prepared) = build_prepared_test_user_task();
    let publication = process_publication();
    publication.publish_process_identity(&proc);

    let sigterm = ksignal::SignalInfo::new_kernel(ksignal::Signo::SIGTERM);
    process_exit::finalize_process_exit(&proc);

    assert!(
        process_signals::send_to_process(proc.pid(), None).is_ok(),
        "kill(zombie, 0) must succeed while the child is still published"
    );
    assert!(
        process_signals::send_to_process(proc.pid(), Some(sigterm.clone())).is_ok(),
        "kill(zombie, SIGTERM) must succeed while the child is still published"
    );

    drop(prepared);
    wait_reap::assert_reap_zombie_process(&proc);

    assert!(
        process_signals::send_to_process(proc.pid(), Some(sigterm)).is_err(),
        "kill on a reaped PID must report ESRCH"
    );
}

#[def_test(serial)]
fn test_runtime_mm_capability_outlives_exit_and_preserves_metadata() {
    let address_space = mapped_test_address_space();
    let (mm_id, residency) = {
        let aspace = address_space.lock();
        (aspace.mm_id(), aspace.cpu_residency().clone())
    };
    let (proc, task) = process_with_address_space(8_139, address_space.clone());
    let runtime = proc.runtime_ref().expect("runtime must be attached");
    // Exercise acquisition in a non-sleeping section, but release the returned
    // capability only after leaving it: final release can tear down mappings.
    let capability = {
        let _pin = kspin::NoPreemptIrqSave::new();
        runtime.address_space().expect("MM user must be live")
    };
    assert_eq!(runtime.mm_id(), mm_id);
    assert!(!proc.exit_mm().expect("runtime must be attached"));
    assert!(runtime.address_space().is_none());
    assert_eq!(address_space.lock().vmas().count(), 1);

    drop(capability);
    assert_eq!(address_space.lock().vmas().count(), 0);
    assert!(!proc.exit_mm().expect("exit must be idempotent"));
    assert_eq!(runtime.mm_id(), mm_id);
    let cpu = khal::percpu::this_cpu_id();
    runtime.set_mm_resident_cpu(cpu);
    assert!(residency.snapshot().get(cpu.as_usize()));
    residency.clear_cpu(cpu);
    let observed = runtime.pinned_address_space_for_teardown_observation();
    assert!(Arc::ptr_eq(&observed, &address_space));

    process_exit::finalize_process_exit(&proc);
    wait_reap::assert_reap_zombie_process(&proc);
    drop(task);
    drop(runtime);
    assert_eq!(observed.lock().mm_id(), mm_id);
    assert_eq!(observed.lock().vmas().count(), 0);
}

#[def_test(serial)]
fn test_runtime_pin_retains_exited_mm_until_task_drop() {
    let address_space = mapped_test_address_space();
    let observer = Arc::downgrade(&address_space);
    let (proc, task) = process_with_address_space(8_140, address_space);
    assert!(proc.exit_mm().expect("runtime must be attached"));
    {
        let pinned = observer.upgrade().expect("runtime pin must retain MM");
        assert_eq!(pinned.lock().vmas().count(), 0);
    }
    process_exit::finalize_process_exit(&proc);
    wait_reap::assert_reap_zombie_process(&proc);
    drop(task);
    assert!(observer.upgrade().is_none(), "runtime pin must be released");
}

#[def_test(serial)]
fn test_runtime_drop_releases_active_mm_user() {
    let address_space = mapped_test_address_space();
    let (proc, task) = process_with_address_space(8_141, address_space.clone());
    process_exit::finalize_process_exit(&proc);
    wait_reap::assert_reap_zombie_process(&proc);
    assert_eq!(address_space.lock().vmas().count(), 1);
    drop(task);
    assert_eq!(address_space.lock().vmas().count(), 0);
}

#[def_test(serial)]
fn test_exit_cleanup_clears_exclusive_address_space() {
    let (proc, _task) = process_with_address_space(8_130, mapped_test_address_space());

    assert!(
        proc.exit_mm().expect("runtime must be attached"),
        "exclusive address space should be cleared during process exit"
    );
    assert!(
        proc.address_space().is_err(),
        "live address-space access must fail after the runtime mm user is released"
    );
    let address_space = proc
        .pinned_address_space_for_teardown_observation()
        .expect("cleared address space object should remain attached until task drop");
    assert_eq!(
        address_space.lock().vmas().count(),
        0,
        "exit cleanup must release user VMA metadata eagerly"
    );

    process_exit::finalize_process_exit(&proc);
    wait_reap::assert_reap_zombie_process(&proc);
}

#[def_test(serial)]
fn test_exit_owners_detach_before_process_exit_publication() {
    let (proc, task) = process_with_address_space(8_137, mapped_test_address_space());
    let resources = task.as_thread().files();
    let signals = proc.signal_manager().unwrap();

    assert!(proc.address_space().is_ok());
    assert!(resources.fd_table().is_ok());
    assert!(proc.fs_context().is_ok());
    assert!(proc.mnt_ns().is_ok());
    assert!(proc.uts_ns().is_ok());

    proc.exit_signal_handlers().unwrap();
    assert_eq!(
        signals.signal_action(Signo::SIGUSR1).err(),
        Some(kerrno::KError::NoSuchProcess)
    );
    proc.exit_signal_handlers().unwrap();
    proc.exit_mm().expect("runtime must be attached");
    task.as_thread().files().exit_files();
    proc.exit_fs().expect("runtime must be attached");
    proc.exit_namespaces().expect("runtime must be attached");

    assert!(!proc.is_exited());
    assert_eq!(
        proc.address_space().err(),
        Some(kerrno::KError::NoSuchProcess)
    );
    assert_eq!(
        resources.fd_table().err(),
        Some(kerrno::KError::NoSuchProcess)
    );
    assert_eq!(proc.fs_context().err(), Some(kerrno::KError::NoSuchProcess));
    assert_eq!(proc.mnt_ns().err(), Some(kerrno::KError::NoSuchProcess));
    assert_eq!(proc.uts_ns().err(), Some(kerrno::KError::NoSuchProcess));

    assert!(!proc.exit_mm().expect("owner release must be idempotent"));
    task.as_thread().files().exit_files();
    proc.exit_fs().expect("owner release must be idempotent");
    proc.exit_namespaces()
        .expect("owner release must be idempotent");

    process_exit::finalize_process_exit(&proc);
    assert!(proc.is_exited());
    assert_eq!(
        resources.fd_table().err(),
        Some(kerrno::KError::NoSuchProcess)
    );
    assert_eq!(proc.fs_context().err(), Some(kerrno::KError::NoSuchProcess));
    assert_eq!(proc.mnt_ns().err(), Some(kerrno::KError::NoSuchProcess));
    wait_reap::assert_reap_zombie_process(&proc);
}

#[def_test(serial)]
fn test_exit_cleanup_ignores_non_runtime_address_space_refs() {
    let observed_address_space = mapped_test_address_space();
    let (proc, _task) = process_with_address_space(8_131, observed_address_space.clone());

    assert!(
        proc.exit_mm().expect("runtime must be attached"),
        "temporary address-space references must not suppress final mm cleanup"
    );
    assert_eq!(
        observed_address_space.lock().vmas().count(),
        0,
        "last runtime mm user must release mappings even when other Arc holders remain"
    );

    process_exit::finalize_process_exit(&proc);
    wait_reap::assert_reap_zombie_process(&proc);
}

#[def_test(serial)]
fn test_exit_cleanup_preserves_clone_vm_address_space_until_last_runtime_user() {
    let shared_address_space = mapped_test_address_space();
    let (parent, parent_task) = process_with_address_space(8_132, shared_address_space.clone());
    parent.set_heap_top(0x6000).unwrap();
    let child = parent_task
        .as_thread()
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Shared,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        })
        .expect("CLONE_VM test child should prepare");
    let child_process = child.process().clone();

    assert_eq!(child_process.address_space().unwrap().heap_top(), 0x6000);
    parent.set_heap_top(0x7000).unwrap();
    assert_eq!(child_process.address_space().unwrap().heap_top(), 0x6000);
    child_process.set_heap_top(0x8000).unwrap();
    assert_eq!(parent.address_space().unwrap().heap_top(), 0x7000);

    assert_eq!(parent.mm_id().unwrap(), child_process.mm_id().unwrap());
    assert!(Arc::ptr_eq(
        &parent
            .pinned_address_space_for_teardown_observation()
            .unwrap(),
        &child_process
            .pinned_address_space_for_teardown_observation()
            .unwrap(),
    ));
    assert!(
        !parent.exit_mm().expect("runtime must be attached"),
        "shared VM mappings must remain while another process runtime still uses the mm"
    );
    assert_eq!(
        shared_address_space.lock().vmas().count(),
        1,
        "shared VM users must keep their mappings"
    );

    assert!(
        child_process
            .exit_mm()
            .expect("child runtime must be attached"),
        "last shared VM runtime user should clear mappings"
    );
    assert_eq!(
        shared_address_space.lock().vmas().count(),
        0,
        "last shared VM user must release mappings"
    );

    child_process.discard_unpublished();
    process_exit::finalize_process_exit(&parent);
    wait_reap::assert_reap_zombie_process(&parent);
}

#[def_test(serial)]
fn test_clone_fs_shares_the_filesystem_context_umask() {
    let (parent, parent_task) = process_with_address_space(8_136, mapped_test_address_space());
    parent.set_heap_top(0x6000).unwrap();
    parent
        .replace_umask(0o7000)
        .expect("parent runtime must expose its filesystem context");
    assert_eq!(
        parent
            .umask()
            .expect("parent runtime must expose its filesystem context"),
        0,
        "FsStruct must truncate umask to permission bits"
    );

    let child = parent_task
        .as_thread()
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Shared,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        })
        .expect("CLONE_FS test child should prepare");
    let child_process = child.process().clone();
    assert_eq!(child_process.address_space().unwrap().heap_top(), 0x6000);
    child_process.set_heap_top(0x7000).unwrap();
    assert_eq!(parent.address_space().unwrap().heap_top(), 0x6000);
    assert!(parent.mm_id().unwrap() != child_process.mm_id().unwrap());
    child_process
        .replace_umask(0o077)
        .expect("child runtime must expose the shared filesystem context");

    assert_eq!(
        parent
            .umask()
            .expect("parent runtime must expose the shared filesystem context"),
        0o077
    );

    child_process.discard_unpublished();
    process_exit::finalize_process_exit(&parent);
    wait_reap::assert_reap_zombie_process(&parent);
}

#[def_test(serial)]
fn test_failed_shared_vm_fork_rolls_back_tree_relation() {
    let shared_address_space = mapped_test_address_space();
    let (parent, parent_task) = process_with_address_space(8_134, shared_address_space);
    let child_count_before = parent.children().len();

    assert!(
        parent.exit_mm().expect("runtime must be attached"),
        "test setup must release the last active address-space user"
    );

    let err = match parent_task
        .as_thread()
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Shared,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        }) {
        Ok(_) => panic!("shared-VM fork must fail once address-space users are released"),
        Err(err) => err,
    };

    assert_eq!(err, kerrno::KError::NoSuchProcess);
    assert_eq!(
        parent.children().len(),
        child_count_before,
        "failed fork must roll back the unpublished child relation"
    );

    process_exit::finalize_process_exit(&parent);
    wait_reap::assert_reap_zombie_process(&parent);
}

#[def_test(serial)]
fn test_failed_private_vm_fork_after_parent_mm_teardown_rolls_back_tree_relation() {
    let shared_address_space = mapped_test_address_space();
    let (parent, parent_task) = process_with_address_space(8_135, shared_address_space);
    let child_count_before = parent.children().len();

    assert!(
        parent.exit_mm().expect("runtime must be attached"),
        "test setup must release the last active address-space user"
    );

    let err = match parent_task
        .as_thread()
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        }) {
        Ok(_) => panic!("private fork must fail once parent mm user is released"),
        Err(err) => err,
    };

    assert_eq!(err, kerrno::KError::NoSuchProcess);
    assert_eq!(
        parent.children().len(),
        child_count_before,
        "failed fork must roll back the unpublished child relation"
    );

    process_exit::finalize_process_exit(&parent);
    wait_reap::assert_reap_zombie_process(&parent);
}

#[def_test(serial)]
fn abandoned_clone_releases_handlers_despite_retained_manager() {
    let (parent, task) = process_with_address_space(8_151, mapped_test_address_space());
    let prepared = task
        .as_thread()
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Shared,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Shared,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(Signo::SIGCHLD),
        })
        .unwrap();
    let child = prepared.process().clone();
    let observer = child.signal_manager().unwrap();
    child.discard_unpublished();
    drop(prepared);
    assert_eq!(
        observer.signal_action(Signo::SIGUSR1).err(),
        Some(kerrno::KError::NoSuchProcess)
    );
    assert!(
        parent
            .signal_manager()
            .unwrap()
            .signal_action(Signo::SIGUSR1)
            .is_ok()
    );
    parent.exit_mm().unwrap();
    process_exit::finalize_process_exit(&parent);
    wait_reap::assert_reap_zombie_process(&parent);
}

#[def_test(serial)]
fn test_failed_fork_after_parent_files_exit_rolls_back_tree_relation() {
    let (parent, parent_task) = process_with_address_space(8_138, mapped_test_address_space());
    let child_count_before = parent.children().len();
    parent_task.as_thread().files().exit_files();

    let err = match parent_task
        .as_thread()
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        }) {
        Ok(_) => panic!("fork must fail once the parent files owner is released"),
        Err(err) => err,
    };

    assert_eq!(err, kerrno::KError::NoSuchProcess);
    assert_eq!(
        parent.children().len(),
        child_count_before,
        "failed fork must roll back the unpublished child relation"
    );

    parent.exit_mm().expect("runtime must be attached");
    parent.exit_fs().expect("runtime must be attached");
    parent.exit_namespaces().expect("runtime must be attached");
    process_exit::finalize_process_exit(&parent);
    wait_reap::assert_reap_zombie_process(&parent);
}

#[def_test(serial)]
fn test_group_exit_prevents_late_thread_exit_from_overwriting_exit_code() {
    let init = ensure_init();
    let proc = init.fork(500);

    let leader_task = publish_test_thread(&proc, 500);
    let sibling_task = publish_test_thread(&proc, 501);

    let not_last = proc.exit_thread(&leader_task, 11);
    assert!(!not_last, "first exiting thread should not be last");
    assert_eq!(
        proc.exit_code(),
        11,
        "early exiting thread publishes initial exit code"
    );

    proc.group_exit();
    let is_last = proc.exit_thread(&sibling_task, 22);
    assert!(is_last, "second exiting thread should be the last thread");
    assert_eq!(
        proc.exit_code(),
        11,
        "late exits after group_exit must not overwrite the published group exit code"
    );

    proc.exit_with_publication(ProcessExitPublication::WaitableZombie);
    proc.free();
}

#[def_test(serial)]
fn test_pidfd_exit_poll_registration_races_and_cancellation() {
    use kpoll::{IoEvents, Pollable};

    let init = ensure_init();
    let child = init.fork(582);
    let pidfd = crate::pidfd::PidFd::new(&child);
    assert!(!pidfd.poll().contains(IoEvents::IN));

    let counter = new_wake_counter();
    let waker = counter_waker(counter);
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    pidfd
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();

    let cancelled_counter = new_wake_counter();
    let cancelled_waker = counter_waker(cancelled_counter);
    let cancelled_context = Context::from_waker(&cancelled_waker);
    let mut cancelled = PollRegistrations::new();
    pidfd
        .register(&mut cancelled.context(&cancelled_context), IoEvents::IN)
        .unwrap();
    drop(cancelled);

    let unrelated_counter = new_wake_counter();
    let unrelated_waker = counter_waker(unrelated_counter);
    let unrelated_context = Context::from_waker(&unrelated_waker);
    let mut unrelated = PollRegistrations::new();
    pidfd
        .register(&mut unrelated.context(&unrelated_context), IoEvents::OUT)
        .unwrap();

    process_exit::finalize_process_exit(&child);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(cancelled_counter.load(Ordering::SeqCst), 0);
    assert_eq!(unrelated_counter.load(Ordering::SeqCst), 0);
    assert!(pidfd.poll().contains(IoEvents::IN));

    let late_counter = new_wake_counter();
    let late_waker = counter_waker(late_counter);
    let late_context = Context::from_waker(&late_waker);
    let mut late = PollRegistrations::new();
    pidfd
        .register(&mut late.context(&late_context), IoEvents::IN)
        .unwrap();
    assert_eq!(late_counter.load(Ordering::SeqCst), 0);
    assert!(pidfd.poll().contains(IoEvents::IN));

    wait_reap::assert_reap_zombie_process(&child);
    assert!(pidfd.poll().contains(IoEvents::IN));
    assert!(
        crate::pidfd::PidFd::new(&child)
            .poll()
            .contains(IoEvents::IN)
    );
}

#[def_test(serial)]
fn test_process_exit_notifies_pidfd_and_parent_waiters() {
    let init = ensure_init();
    let child = init.fork(580);
    let _leader_task = publish_test_thread(&child, 580);
    child.accumulate_exited_thread_time(TimeSpan::from_nanos(77), TimeSpan::from_nanos(88));

    let parent_counter = new_wake_counter();
    let parent_waker = counter_waker(parent_counter);
    let _parent_registration = register_poll_set(init.child_exit_event(), &parent_waker);

    let child_counter = new_wake_counter();
    let child_waker = counter_waker(child_counter);
    let _child_registration = register_poll_set(child.exit_event(), &child_waker);

    let accounting_observer = new_exit_accounting_observer(
        child.clone(),
        TimeSpan::from_nanos(77),
        TimeSpan::from_nanos(88),
    );
    let accounting_parent_waker = accounting_waker(accounting_observer);
    let _accounting_parent_registration =
        register_poll_set(init.child_exit_event(), &accounting_parent_waker);
    let accounting_child_waker = accounting_waker(accounting_observer);
    let _accounting_child_registration =
        register_poll_set(child.exit_event(), &accounting_child_waker);

    process_exit::finalize_process_exit(&child);

    assert!(
        child.is_exited(),
        "process exit state must stay observable for late observers"
    );
    assert_eq!(
        child_counter.load(Ordering::SeqCst),
        1,
        "process exit must wake pidfd-style observers"
    );
    assert_eq!(
        parent_counter.load(Ordering::SeqCst),
        0,
        "finalize_process_exit must not wake parent waiters before child-exit policy is resolved"
    );

    process_exit::notify_child_exit(&init);

    assert_eq!(
        parent_counter.load(Ordering::SeqCst),
        1,
        "explicit child-exit notification must wake parent waitpid observers"
    );
    assert_eq!(
        accounting_observer.failures.load(Ordering::SeqCst),
        0,
        "exit notifications must observe final exited-thread accounting"
    );

    wait_reap::assert_reap_zombie_process(&child);
}

#[def_test(serial)]
fn test_complete_process_exit_wakes_parent_after_child_is_waitable() {
    let init = ensure_init();
    let child = init.fork(579);

    let observer = new_child_exit_wake_observer(init.clone(), child.clone());
    let observer_waker = child_exit_waker(observer);
    let _observer_registration = register_poll_set(init.child_exit_event(), &observer_waker);

    let autoreap = process_exit::complete_process_exit(&child);

    assert!(!autoreap, "default SIGCHLD must leave a waitable child");
    assert_eq!(
        observer.failures.load(Ordering::SeqCst),
        0,
        "parent wait wake must observe a linked waitable zombie"
    );

    wait_reap::assert_reap_zombie_process(&child);
}

#[def_test(serial)]
fn test_child_exit_signal_info_carries_linux_child_payload() {
    let (child, prepared) = build_prepared_test_user_task();
    let task = prepare_user_task(prepared).publish().unwrap().activate();
    process_exit::finish_thread_exit(&child, &task, 9 << 8);

    // The fixture forks from init, so the parent is init in the root
    // namespace; the projection there equals the root number. The
    // namespace-relative path is covered by
    // test_child_exit_si_pid_projected_into_parent_namespace.
    let parent = child.parent().expect("fixture child keeps its init parent");
    let siginfo = process_exit::child_exit_signal_info(&child, &parent, ksignal::Signo::SIGCHLD);
    let payload = siginfo
        .as_signal_info()
        .child_exit()
        .expect("child-exit signal should carry sigchld payload");

    assert_eq!(siginfo.signo(), ksignal::Signo::SIGCHLD);
    assert_eq!(siginfo.as_signal_info().code(), 1);
    assert_eq!(payload.pid(), child.pid());
    assert_eq!(payload.uid(), 0);
    assert_eq!(payload.status(), 9);

    child.discard_unpublished();
}

#[def_test(user, serial)]
fn test_thread_scheduler_parameters_are_a_consistent_snapshot() {
    let (process, task) = build_prepared_test_user_task();
    let thread = task.as_thread();

    assert_eq!(thread.scheduler_parameters().policy(), None);
    assert_eq!(thread.scheduler_parameters().priority(), 0);

    thread.set_scheduler(1, 7);
    let parameters = thread.scheduler_parameters();
    assert_eq!(parameters.policy(), Some(1));
    assert_eq!(parameters.priority(), 7);

    thread
        .set_scheduler_priority_with(9, 0, |policy, priority| {
            if policy != 1 || priority != 9 {
                return Err(kerrno::KError::InvalidInput);
            }
            Ok(())
        })
        .expect("priority update should validate against locked scheduler policy");
    let parameters = thread.scheduler_parameters();
    assert_eq!(parameters.policy(), Some(1));
    assert_eq!(parameters.priority(), 9);

    process.discard_unpublished();
}

#[def_test(user, serial)]
fn test_clone_inherits_calling_thread_mask_without_copying_private_pending() {
    use ksignal::{SignalInfo, SignalSet, Signo};

    let same_set = |actual: SignalSet, expected: SignalSet| {
        (actual & !expected).is_empty() && (expected & !actual).is_empty()
    };
    let (process, task) = build_prepared_test_user_task();
    let leader = task.as_thread();
    let mut leader_mask = SignalSet::default();
    leader_mask.add(Signo::SIGUSR1);
    leader.signal_manager().set_blocked(leader_mask);
    let caller = leader.clone_thread_in_process().unwrap();
    assert!(same_set(caller.signal_manager().blocked(), leader_mask));

    let mut caller_mask = SignalSet::default();
    caller_mask.add(Signo::SIGUSR2);
    let mut requested_mask = caller_mask;
    requested_mask.add(Signo::SIGKILL);
    requested_mask.add(Signo::SIGSTOP);
    caller.signal_manager().set_blocked(requested_mask);
    assert!(same_set(caller.signal_manager().blocked(), caller_mask));
    assert!(same_set(leader.signal_manager().blocked(), leader_mask));
    assert!(
        !caller
            .signal_manager()
            .send_signal(SignalInfo::new_kernel(Signo::SIGUSR2))
    );
    let _ = caller
        .signal_manager()
        .process()
        .send_signal(SignalInfo::new_kernel(Signo::SIGTERM));

    let sibling = caller.clone_thread_in_process().unwrap();
    assert!(same_set(sibling.signal_manager().blocked(), caller_mask));
    let mut shared_pending = SignalSet::default();
    shared_pending.add(Signo::SIGTERM);
    assert!(same_set(sibling.signal_manager().pending(), shared_pending));
    sibling.signal_manager().set_blocked(SignalSet::default());
    assert!(same_set(caller.signal_manager().blocked(), caller_mask));

    for signal_actions in [ForkSignalActions::Private, ForkSignalActions::Shared] {
        let child = caller
            .fork_process_child(ProcessForkConfig {
                parent: ForkParent::Caller,
                address_space: if signal_actions == ForkSignalActions::Shared {
                    ForkAddressSpace::Shared
                } else {
                    ForkAddressSpace::Private
                },
                fs: ForkFs::Private,
                signal_actions,
                fd_table: ForkFdTable::Private,
                namespace_flags: kns::NamespaceFlags::empty(),
                exit_signal: Some(Signo::SIGCHLD),
            })
            .unwrap();
        assert!(same_set(child.signal_manager().blocked(), caller_mask));
        assert!(child.signal_manager().pending().is_empty());
        child.signal_manager().set_blocked(SignalSet::default());
        assert!(same_set(caller.signal_manager().blocked(), caller_mask));
        assert!(same_set(leader.signal_manager().blocked(), leader_mask));
        child.process().discard_unpublished();
    }
    assert!(same_set(
        caller.signal_manager().pending(),
        caller_mask | shared_pending
    ));
    process.discard_unpublished();
}

#[def_test(user, serial)]
fn test_oom_score_adjustment_is_process_shared_and_fork_inherited() {
    let (process, task) = build_prepared_test_user_task();
    let thread = task.as_thread();

    assert_eq!(thread.oom_score_adj(), 0);
    thread.set_oom_score_adj(321);

    let sibling = thread
        .clone_thread_in_process()
        .expect("same-process thread clone should prepare");
    assert_eq!(sibling.oom_score_adj(), 321);

    let child = thread
        .fork_process_child(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Private,
            signal_actions: ForkSignalActions::Private,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(ksignal::Signo::SIGCHLD),
        })
        .expect("process fork should prepare");
    assert_eq!(child.oom_score_adj(), 321);

    child.process().discard_unpublished();
    process.discard_unpublished();
}

#[def_test(serial)]
fn test_exit_retries_signal_prepare_after_parent_reparent_race() {
    let init = ensure_init();
    let parent = init.fork(582);
    let child = parent.fork(583);
    let raced = AtomicBool::new(false);

    let transition = child.finish_exit_in_process_domain(
        ProcessExitPublication::WaitableZombie,
        |observed_parent| {
            if !raced.swap(true, Ordering::SeqCst) {
                if observed_parent.pid() != parent.pid() {
                    panic!("first signal preparation should observe the original parent");
                }
                parent.exit_with_publication(ProcessExitPublication::WaitableZombie);
            }
            Some((observed_parent.pid(), false))
        },
    );

    assert_eq!(
        transition.parent.as_ref().map(|parent| parent.pid()),
        Some(init.pid()),
        "exit commit must retry after reparent and target the current parent"
    );
    assert_eq!(
        transition.prepared_sigchld,
        Some(init.pid()),
        "prepared SIGCHLD payload must come from the retried parent"
    );
    assert_eq!(
        child.parent().map(|parent| parent.pid()),
        Some(init.pid()),
        "child must be linked under init after parent exit races with child exit"
    );

    child.free();
    parent.free();
}

#[def_test(serial)]
fn test_complete_process_exit_is_idempotent_for_sigchld_delivery() {
    let init = ensure_init();
    let child = init.fork(584);

    assert!(
        child
            .finish_exit_in_process_domain(ProcessExitPublication::WaitableZombie, |parent| {
                Some((parent.pid(), false))
            })
            .prepared_sigchld
            .is_some(),
        "first exit commit should return prepared SIGCHLD delivery"
    );
    assert!(
        child
            .finish_exit_in_process_domain(ProcessExitPublication::WaitableZombie, |parent| {
                Some((parent.pid(), false))
            })
            .prepared_sigchld
            .is_none(),
        "idempotent exit commit must not return a stale prepared SIGCHLD delivery"
    );

    child.free();
}

#[def_test(serial)]
fn test_finalize_process_exit_is_idempotent_after_autoreap() {
    let init = ensure_init();
    let child = init.fork(581);

    let child_counter = new_wake_counter();
    let child_waker = counter_waker(child_counter);
    let _child_registration = register_poll_set(child.exit_event(), &child_waker);

    process_exit::finalize_process_exit_with_publication(
        &child,
        ProcessExitPublication::DetachedAutoreap,
    );
    assert!(child.is_exited());
    assert!(
        child.is_exited(),
        "autoreap exit state must stay observable for late observers"
    );
    assert!(
        !child.is_waitable_zombie(),
        "autoreap finalization must skip waitable zombie state"
    );
    assert_eq!(
        child_counter.load(Ordering::SeqCst),
        1,
        "first finalization wakes exit observers"
    );

    process_exit::finalize_process_exit(&child);
    assert!(
        !child.is_waitable_zombie(),
        "second finalization must not change Dead back to Zombie"
    );
    assert_eq!(
        child_counter.load(Ordering::SeqCst),
        1,
        "second finalization must not notify exit observers again"
    );

    wait_reap::reap_exited_process(&child);
}

#[def_test(serial)]
fn test_lifecycle_accumulates_exited_thread_and_child_cpu_time() {
    let init = ensure_init();
    let proc = init.fork(600);

    proc.accumulate_exited_thread_time(TimeSpan::from_nanos(11), TimeSpan::from_nanos(22));
    proc.accumulate_exited_thread_time(TimeSpan::from_nanos(33), TimeSpan::from_nanos(44));
    proc.accumulate_child_time(TimeSpan::from_nanos(55), TimeSpan::from_nanos(66));
    proc.accumulate_child_time(TimeSpan::from_nanos(77), TimeSpan::from_nanos(88));

    assert_eq!(
        proc.exited_thread_time(),
        (TimeSpan::from_nanos(44), TimeSpan::from_nanos(66))
    );
    assert_eq!(
        proc.child_time(),
        (TimeSpan::from_nanos(132), TimeSpan::from_nanos(154))
    );

    proc.exit_with_publication(ProcessExitPublication::WaitableZombie);
    proc.free();
}

#[def_test(serial)]
fn test_published_task_lookup_matches_published_user_thread() {
    let process = ensure_init().fork(740);
    let task = publish_test_thread(&process, process.pid());
    let tid = task.as_thread().tid();
    let publication = process_publication();

    let published_task = publication.task(tid).expect("published tid should resolve");
    assert_eq!(published_task.as_thread().tid(), tid);

    let published_process = publication
        .published_process(process.pid())
        .expect("published pid should resolve");
    assert!(
        Arc::ptr_eq(&published_process, &process),
        "published pid lookup must return the same stable process object"
    );
    assert!(
        publication
            .published_processes()
            .iter()
            .any(|published| Arc::ptr_eq(published, &process)),
        "published process enumeration must include the current process after publication"
    );

    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    process.free();
    let _ = publication.unpublish_process_if_matches(&process);
    drop(task);
    publication.cleanup();
}

#[def_test(user, serial)]
fn test_current_process_mutation_helpers_preserve_process_boundary() {
    let (process, _task) = process_with_address_space(741, mapped_test_address_space());

    let old_umask = process
        .replace_umask(0o7077)
        .expect("current process must expose a live umask");
    assert_eq!(
        process
            .umask()
            .expect("current process must expose a live umask"),
        0o077
    );
    process
        .replace_umask(old_umask)
        .expect("current process must restore its previous umask");

    let old_heap_top = process
        .heap_top()
        .expect("current process must expose a live heap top");
    let old_exe_path = process
        .exe_path()
        .expect("current process must expose a live exec path");
    let old_cmdline = process
        .cmdline()
        .expect("current process must expose a live cmdline");
    let new_heap_top = old_heap_top.saturating_add(0x1000);
    process
        .set_heap_top(new_heap_top)
        .expect("current process must expose a mutable heap top");
    assert_eq!(
        process
            .heap_top()
            .expect("current process must expose the updated heap top"),
        new_heap_top
    );

    let exe_path = String::from("/bin/unittest-boundary");
    let cmdline: Arc<Vec<String>> = Arc::new(vec![
        String::from("unittest-boundary"),
        String::from("--check"),
    ]);
    process
        .set_exec_metadata(exe_path.clone(), cmdline.clone(), None)
        .expect("current process must expose mutable exec metadata");
    assert_eq!(
        process
            .exe_path()
            .expect("current process must expose its exec path"),
        exe_path
    );
    assert!(
        Arc::ptr_eq(
            &process
                .cmdline()
                .expect("current process must expose its cmdline"),
            &cmdline
        ),
        "Process metadata helpers must update the stable process identity in place"
    );

    process
        .set_heap_top(old_heap_top)
        .expect("current process must restore its previous heap top");
    process
        .set_exec_metadata(old_exe_path, old_cmdline, None)
        .expect("current process must restore its previous exec metadata");

    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    process.free();
}

#[def_test(user, serial)]
fn test_prepare_thread_clone_defers_tid_visibility_until_publication() {
    let process = ensure_init().fork(742);
    let leader_task = publish_test_thread(&process, process.pid());
    let thread = leader_task.as_thread();
    let prepared = thread
        .prepare_thread_clone()
        .expect("thread clone should allocate a sibling tid");
    let tid = prepared.tid();

    assert!(
        !process.threads().contains(&tid),
        "prepared sibling tid must stay hidden until task publication"
    );

    let (cloned, task_number) = prepared.into_parts();
    let task = TaskInner::new_user(|| {}, String::from("prepared-thread"), task_number, cloned);
    let published = prepare_user_task(task).publish().unwrap();
    let sibling_task = published.task().clone();

    assert!(!thread.has_exit_address_space());
    assert!(!sibling_task.as_thread().has_exit_address_space());
    let is_last = process.exit_thread(&sibling_task, 0);
    assert!(
        !is_last,
        "published sibling thread removal must not tear down the whole process"
    );
    assert!(sibling_task.as_thread().has_exit_address_space());
    assert!(!sibling_task.as_thread().is_exiting());
    assert!(!process.contains_published_tid(tid));
    assert!(crate::lookup::task(tid).is_err());
    assert_eq!(process.thread_count(), 1);
    assert!(!process.exit_thread(&sibling_task, 0));
    drop(published);
    assert!(process.exit_thread(&leader_task, 0));
    assert!(thread.has_exit_address_space());
    assert_eq!(process.thread_count(), 0);
    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    process.free();
    let _ = process_publication().unpublish_process_if_matches(&process);
    drop(leader_task);
    process_publication().cleanup();
}

#[def_test(user, serial)]
fn test_process_migration_reconciles_a_prepared_sibling_at_publication() {
    let process = ensure_init().fork(1742);
    let leader_task = publish_test_thread(&process, process.pid());
    let root = leader_task
        .as_thread()
        .cgroup()
        .expect("published leader must have cgroup membership")
        .hierarchy_root();
    root.set_pids_subtree_enabled(true).unwrap();
    let target = root.create_child("publish-migration-target").unwrap();

    let prepared = leader_task
        .as_thread()
        .prepare_thread_clone()
        .expect("thread clone should reserve its original cgroup");
    scheduler::migrate_process_cgroup(&process, &target).unwrap();

    let (cloned, task_number) = prepared.into_parts();
    let sibling = prepare_user_task(TaskInner::new_user(
        || {},
        String::from("cgroup-reconciled-sibling"),
        task_number,
        cloned,
    ))
    .publish()
    .unwrap();
    assert!(Arc::ptr_eq(
        &sibling
            .task()
            .as_thread()
            .cgroup()
            .expect("published sibling must have cgroup membership"),
        &target
    ));

    sibling.task().as_thread().detach_cgroup();
    leader_task.as_thread().detach_cgroup();
    drop(sibling);
    process.exit_thread(&leader_task, 0);
    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    process.free();
    let _ = process_publication().unpublish_process_if_matches(&process);
    drop(leader_task);
    root.remove_child("publish-migration-target").unwrap();
    process_publication().cleanup();
}

#[def_test(user, serial)]
fn test_cgroup_reconciliation_failure_does_not_publish_task() {
    let process = ensure_init().fork(1743);
    let leader_task = publish_test_thread(&process, process.pid());
    let original = leader_task
        .as_thread()
        .cgroup()
        .expect("published leader must have cgroup membership");
    let prepared = leader_task
        .as_thread()
        .prepare_thread_clone()
        .expect("thread clone should reserve its original cgroup");
    let (cloned, task_number) = prepared.into_parts();
    let sibling_tid = task_number.root_nr();
    let sibling = prepare_user_task(TaskInner::new_user(
        || {},
        String::from("cgroup-reconciliation-failure"),
        task_number,
        cloned,
    ));

    *process.cgroup_transaction() = Some(kcgroup::CgroupNamespace::new().root());
    assert!(sibling.publish().is_err());
    assert!(process_publication().task(sibling_tid).is_err());

    *process.cgroup_transaction() = Some(original);
    process.exit_thread(&leader_task, 0);
    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    process.free();
    let _ = process_publication().unpublish_process_if_matches(&process);
    drop(leader_task);
    process_publication().cleanup();
}

/// `sched_setaffinity` resolves by tid first, then falls back to tgid/leader.
/// A non-leader tid must not be mistaken for a process id (illegal tgid).
#[def_test(user, serial)]
fn test_scheduler_resolves_non_leader_tid_not_as_tgid() {
    let process = ensure_init().fork(743);
    let leader_task = publish_test_thread(&process, process.pid());
    let prepared = leader_task
        .as_thread()
        .prepare_thread_clone()
        .expect("sibling tid allocation");
    let sibling_tid = prepared.tid();
    assert_ne!(
        sibling_tid,
        process.pid(),
        "sibling tid must differ from the process tgid/leader"
    );

    let (cloned, task_number) = prepared.into_parts();
    let sibling = prepare_user_task(TaskInner::new_user(
        || {},
        String::from("affinity-sibling"),
        task_number,
        cloned,
    ))
    .publish()
    .unwrap();
    let sibling_task = sibling.task().clone();

    let by_tid = scheduler::task_by_tid(sibling_tid).expect("published sibling tid");
    assert!(
        ktask::KtaskRef::ptr_eq(&by_tid, &sibling_task),
        "tid lookup must return the sibling task, not the leader"
    );
    assert_eq!(by_tid.as_thread().tid(), sibling_tid);

    // Fallback path used when tid lookup fails: this number is not a live tgid.
    match scheduler::target_task(sibling_tid as i32) {
        Err(kerrno::KError::NoSuchProcess) => {}
        Ok(_) => panic!("non-leader tid must not resolve as a process/tgid"),
        Err(other) => panic!("unexpected errno for illegal tgid: {other:?}"),
    }

    // tgid fallback returns a published thread of that process (any
    // representative), not a foreign task keyed by the sibling tid.
    let by_tgid = scheduler::target_task(process.pid() as i32).expect("live tgid");
    assert_eq!(by_tgid.as_thread().pid(), process.pid());
    assert!(
        ktask::KtaskRef::ptr_eq(&by_tgid, &leader_task)
            || ktask::KtaskRef::ptr_eq(&by_tgid, &sibling_task),
        "tgid lookup must stay inside the process thread group"
    );

    let _ = process.exit_thread(&sibling_task, 0);
    drop(sibling);
    process.exit_thread(&leader_task, 0);
    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    process.free();
    let _ = process_publication().unpublish_process_if_matches(&process);
    drop(leader_task);
    process_publication().cleanup();
}

#[def_test(user, serial)]
fn test_process_owns_tid_requires_published_task_binding() {
    let (process, _prepared) = build_prepared_test_user_task();

    assert!(
        !scheduler::process_owns_tid(&process, process.pid()),
        "prepared thread identity must stay hidden until publication installs the process-owned \
         task binding"
    );
}

#[def_test(user, serial)]
fn test_prepare_publish_stages_visibility_before_activation() {
    let (process, prepared) = build_prepared_test_user_task();
    let publication = process_publication();

    assert!(
        publication.task(process.pid()).is_err(),
        "prepared task must not be visible in the task registry before publish"
    );
    assert!(
        publication.published_process(process.pid()).is_err(),
        "prepared process must not be visible in the process registry before publish"
    );
    assert!(
        !process.threads().contains(&process.pid()),
        "prepared process leader must not appear in the thread set before publish"
    );

    let published = prepare_user_task(prepared).publish().unwrap();

    let published_task = publication
        .task(process.pid())
        .expect("published task should resolve by tid");
    assert_eq!(published_task.as_thread().tid(), process.pid());
    assert!(
        process.threads().contains(&process.pid()),
        "publishing must register the process leader tid together with task visibility"
    );

    let published_process = publication
        .published_process(process.pid())
        .expect("published process should resolve by pid");
    assert!(
        Arc::ptr_eq(&published_process, &process),
        "publish must expose the same stable process identity before activation"
    );

    drop(published);
    publication.cleanup();
}

#[def_test(user, serial)]
fn test_unpublished_tree_child_stays_hidden_from_group_membership() {
    let parent = ensure_init();
    let task_number =
        kidentity::allocate_root_pid_handle().expect("test child identity should allocate");
    let child = parent
        .fork_with_tree_parent(
            task_number,
            ForkParent::Caller,
            Some(ksignal::Signo::SIGCHLD),
        )
        .expect("tree fork should prepare a child relation");
    let group = child.group();

    assert!(
        parent
            .children()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &child)),
        "tree relation is owner-visible after fork preparation"
    );
    assert!(
        group
            .processes()
            .iter()
            .all(|member| !Arc::ptr_eq(member, &child)),
        "process-group membership must not expose a child before publication commits"
    );

    child.discard_unpublished();
}

#[def_test(user, serial)]
fn test_cleanup_preserves_reserved_process_publication_slot() {
    let parent = ensure_init();
    let task_number =
        kidentity::allocate_root_pid_handle().expect("test child identity should allocate");
    let child = parent
        .fork_with_tree_parent(
            task_number,
            ForkParent::Caller,
            Some(ksignal::Signo::SIGCHLD),
        )
        .expect("tree fork should prepare a child relation");
    let publication = process_publication();

    publication.publish_process_identity_after_cleanup_for_test(&child);
    let resolved = publication
        .published_process(child.pid())
        .expect("cleanup must not delete a reserved slot before publish commit");
    assert!(
        Arc::ptr_eq(&resolved, &child),
        "reserved slot must still publish the original process identity"
    );

    child.exit_with_publication(ProcessExitPublication::WaitableZombie);
    child.free();
    let _ = publication.unpublish_process_if_matches(&child);
    publication.cleanup();
}

#[def_test(user, serial)]
fn test_publish_user_task_exposes_handle_before_activation() {
    let parent = ensure_init();
    let (process, prepared) = build_prepared_test_user_task();
    let publication = process_publication();

    assert!(
        publication.task(process.pid()).is_err(),
        "prepared task must stay hidden before publication"
    );
    assert!(
        publication.published_process(process.pid()).is_err(),
        "prepared process must stay hidden before publication"
    );

    let published = publish_user_task(prepared).unwrap();

    let published_task = publication
        .task(process.pid())
        .expect("published task should resolve through the task registry");
    assert!(
        ktask::KtaskRef::ptr_eq(published.task(), &published_task),
        "published handle must reference the same task object that lookup returns"
    );
    let published_process = publication
        .published_process(process.pid())
        .expect("published pid should resolve before activation");
    assert!(
        Arc::ptr_eq(&published_process, &process),
        "publish_user_task must expose the stable process identity before activation"
    );

    published.abort();
    assert!(
        publication.task(process.pid()).is_err(),
        "aborting staged publication must remove the unpublished task binding"
    );
    assert!(
        publication.published_process(process.pid()).is_err(),
        "aborting staged publication must remove the unpublished process identity"
    );
    assert!(
        !process.threads().contains(&process.pid()),
        "aborting staged publication must remove the provisional thread membership"
    );
    assert!(
        parent
            .children()
            .iter()
            .all(|child| child.pid() != process.pid()),
        "aborting a never-started child must remove it from the parent's live child relation"
    );
    assert!(
        process
            .group()
            .processes()
            .iter()
            .all(|member| member.pid() != process.pid()),
        "aborting a never-started child must remove it from its provisional process-group \
         membership"
    );
}

#[def_test(user, serial)]
fn test_zombie_process_stays_published_until_reaped() {
    let (process, prepared) = build_prepared_test_user_task();
    let child_pid = process.pid();
    let publication = process_publication();

    assert!(
        publication.published_process(child_pid).is_err(),
        "prepared child must not be visible before explicit publication"
    );

    let published = prepare_user_task(prepared).publish().unwrap();

    let published_before_exit = publication
        .published_process(child_pid)
        .expect("published child must resolve before exit");
    assert!(
        Arc::ptr_eq(&published_before_exit, &process),
        "published registry must expose the stable child process identity"
    );

    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    assert!(
        process.is_exited(),
        "child exit must publish stable exited state"
    );
    assert!(
        publication.published_process(child_pid).is_ok(),
        "zombie child must stay published until an explicit reap/removal step"
    );

    process.free();
    let _ = publication.unpublish_process_if_matches(&process);
    assert!(
        publication.published_process(child_pid).is_err(),
        "reaped child must disappear from the published PID registry"
    );

    drop(published);
    publication.cleanup();
}

#[def_test(user, serial)]
fn test_published_process_count_ignores_retired_slots_before_cleanup() {
    let (process, prepared) = build_prepared_test_user_task();
    let pid = process.pid();
    let publication = process_publication();
    let before = publication.published_process_count();
    let published = prepare_user_task(prepared).publish().unwrap();

    assert_eq!(
        publication.published_process_count(),
        before + 1,
        "publishing a process must increase the externally visible count"
    );

    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    process.free();
    let _ = publication.unpublish_process_if_matches(&process);
    assert!(
        publication.published_process(pid).is_err(),
        "reaped process must disappear once its publication slot is retired"
    );
    assert_eq!(
        publication.published_process_count(),
        before,
        "retired publication slots must not contribute to the visible process count"
    );

    drop(published);
    publication.cleanup();
}

#[def_test(user, serial)]
fn test_exit_and_reap_unpublish_tid_and_pid() {
    let (process, prepared) = build_prepared_test_user_task();
    let publication = process_publication();
    let published = prepare_user_task(prepared).publish().unwrap().activate();
    let tid = published.as_thread().tid();
    let pid = process.pid();
    assert_eq!(procfs::thread_group_member_count(&process), 1);

    assert!(
        process.exit_thread(&published, 0),
        "single-threaded process exit must report the last thread"
    );
    assert!(
        publication.task(tid).is_err(),
        "retired tid must stop resolving through the task directory"
    );
    assert_eq!(scheduler::process_task_count(&process), 0);
    assert_eq!(procfs::thread_group_member_count(&process), 1);

    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    assert_eq!(procfs::thread_group_member_count(&process), 1);
    assert!(
        wait_reap::reap_zombie_process(&process),
        "waitable zombie must be consumable once"
    );
    assert!(
        publication.published_process(pid).is_err(),
        "reaped pid must stop resolving through the process directory"
    );
    // Strong Process/task handles still exist: reap, not Arc destruction,
    // releases the retained leader from the lifetime membership view.
    assert_eq!(procfs::thread_group_member_count(&process), 0);
}

#[def_test(user, serial)]
fn test_exited_leader_membership_survives_sibling_exit_until_reap() {
    let (process, prepared) = build_prepared_test_user_task();
    let leader = prepare_user_task(prepared).publish().unwrap().activate();
    let sibling_number = kidentity::allocate_root_pid_handle().unwrap();
    let sibling = publish_test_thread(&process, sibling_number.root_nr());
    assert_eq!(procfs::thread_group_member_count(&process), 2);

    assert!(!process.exit_thread(&leader, 0));
    assert_eq!(scheduler::process_task_count(&process), 1);
    assert_eq!(procfs::thread_group_member_count(&process), 2);
    assert!(
        process_publication()
            .task(leader.as_thread().tid())
            .is_err()
    );

    assert!(process.exit_thread(&sibling, 0));
    assert_eq!(scheduler::process_task_count(&process), 0);
    assert_eq!(procfs::thread_group_member_count(&process), 1);
    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    assert_eq!(procfs::thread_group_member_count(&process), 1);
    assert!(wait_reap::reap_zombie_process(&process));
    assert_eq!(procfs::thread_group_member_count(&process), 0);
}

#[def_test(user, serial)]
fn test_unpublish_task_if_matches_preserves_reused_tid_identity() {
    let process = ensure_init().fork(1_170);
    let publication = process_publication();
    let first = publish_test_thread(&process, 1_170);

    // Synthetic fixed-number replacement, not runtime allocator behavior:
    // kidentity never recycles numbers on drop or wraps at exhaustion. Retire
    // without map removal, then republish a different task under this number.
    publication.retire_task_slot_keep_entry_for_test(1_170);
    let second = publish_test_thread(&process, 1_170);
    assert!(
        publication
            .task(1_170)
            .is_ok_and(|task| ktask::KtaskRef::ptr_eq(&task, &second)),
        "republished tid must resolve to the newer task"
    );

    assert!(
        !publication.unpublish_task_if_matches(&first),
        "identity-checked unpublish must reject an older task after TID reuse"
    );
    let resolved = publication
        .task(1_170)
        .expect("reused tid must keep resolving to the live task");
    assert!(
        ktask::KtaskRef::ptr_eq(&resolved, &second),
        "retiring an older task must not disturb a republished live TID slot"
    );

    // Membership is keyed by stable identity, so the forced-retired `first`
    // task is still a live process member even though its TID slot was reused
    // by `second`. Exiting `second` therefore must not be the last-member exit.
    assert!(
        !process.exit_thread(&second, 0),
        "an older live member must keep the process alive"
    );
    assert!(
        process.exit_thread(&first, 0),
        "the last live member must be able to exit cleanly"
    );
    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    process.free();
    let _ = publication.unpublish_process_if_matches(&process);
    drop(first);
    drop(second);
}

#[def_test(serial)]
fn test_exec_leader_takeover_moves_number_onto_survivor() {
    let (process, leader_task) = build_prepared_test_user_task();
    let published_leader = prepare_user_task(leader_task).publish().unwrap();
    let leader = published_leader.task().clone();
    let pid = process.pid();
    assert_eq!(leader.as_thread().tid(), pid);

    let survivor = publish_test_thread(&process, 1_175);
    let survivor_tid = survivor.as_thread().tid();
    assert_ne!(survivor_tid, pid);

    // The `exec` sibling sweep retires the leader from the process first.
    assert!(!process.exit_thread(&leader, 0));
    assert_eq!(process.thread_count(), 1);

    assert!(process.assume_leader_identity(&survivor));
    assert_eq!(survivor.as_thread().tid(), pid);
    assert!(
        process_publication()
            .task(pid)
            .is_ok_and(|task| ktask::KtaskRef::ptr_eq(&task, &survivor)),
        "the group number must resolve to the surviving thread"
    );
    assert!(
        process_publication().task(survivor_tid).is_err(),
        "the survivor's old number entry must be gone"
    );
    assert_eq!(process.threads(), vec![pid]);
}

#[def_test(serial)]
fn test_unpublish_process_if_matches_preserves_different_identity() {
    let init = ensure_init();
    let published = init.fork(1_160);
    let impostor = Process::new_init(1_160);
    let publication = process_publication();

    publication.publish_process_identity(&published);
    assert!(
        !publication.unpublish_process_if_matches(&impostor),
        "identity-checked unpublish must reject a different process with the same pid"
    );

    let resolved = publication
        .published_process(published.pid())
        .expect("original published identity must remain visible");
    assert!(
        Arc::ptr_eq(&resolved, &published),
        "published pid must still resolve to the original process"
    );

    published.exit_with_publication(ProcessExitPublication::WaitableZombie);
    published.free();
    let _ = publication.unpublish_process_if_matches(&published);
}

#[def_test(serial)]
fn test_procfs_visibility_requires_representative_task() {
    let (process, prepared) = build_prepared_test_user_task();
    let pid = process.pid();
    let publication = process_publication();
    let published = prepare_user_task(prepared).publish().unwrap();

    assert!(
        procfs::visible_processes()
            .iter()
            .any(|candidate| candidate.pid() == pid),
        "published live process should appear in /proc visibility"
    );
    assert!(
        procfs::process_task(pid).is_ok(),
        "visible live process must resolve to a representative task"
    );

    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    drop(published);
    publication.cleanup();

    assert!(
        publication.published_process(pid).is_ok(),
        "zombie process should remain published until reaped"
    );
    assert!(
        procfs::process_task(pid).is_err(),
        "zombie process without surviving task should no longer resolve to a procfs task"
    );
    assert!(
        !procfs::visible_processes()
            .iter()
            .any(|candidate| candidate.pid() == pid),
        "procfs root listing must stay consistent with process_task lookup"
    );

    process.free();
    let _ = publication.unpublish_process_if_matches(&process);
    publication.cleanup();
}

#[def_test(user, serial)]
fn test_create_session_publishes_new_job_control_identity() {
    let init = ensure_init();
    let process = init.fork(700);
    let _leader_task = publish_test_thread(&process, 700);

    let publication = process_publication();
    publication.publish_process_identity(&process);

    let (_session, group) = process
        .create_session()
        .expect("published process should be able to create a new session");
    let published_group = publication
        .process_group(group.pgid())
        .expect("new session leader group must be published for later setpgid/kill lookups");
    assert_eq!(published_group.pgid(), group.pgid());
    assert_eq!(published_group.session().sid(), process.pid());

    process.exit_with_publication(ProcessExitPublication::WaitableZombie);
    process.free();
    let _ = publication.unpublish_process_if_matches(&process);
    publication.cleanup();
}

#[def_test(user, serial)]
fn test_create_group_publishes_new_process_group_identity() {
    let init = ensure_init();
    let leader = init.fork(710);
    let _leader_task = publish_test_thread(&leader, 710);
    let (_session, original_group) = leader
        .create_session()
        .expect("leader should be able to create its own session");
    let sibling = leader.fork(711);
    let _sibling_task = publish_test_thread(&sibling, 711);

    let publication = process_publication();
    publication.publish_process_identity(&leader);
    publication.publish_process_identity(&sibling);

    assert_eq!(sibling.group().pgid(), original_group.pgid());
    let new_group = sibling
        .create_group()
        .expect("sibling in same session should create a distinct process group");

    let published_group = publication
        .process_group(new_group.pgid())
        .expect("newly created process group must be visible through publication");
    assert_eq!(published_group.pgid(), new_group.pgid());
    assert_eq!(published_group.session().sid(), leader.pid());

    sibling.exit_with_publication(ProcessExitPublication::WaitableZombie);
    sibling.free();
    let _ = publication.unpublish_process_if_matches(&sibling);
    leader.exit_with_publication(ProcessExitPublication::WaitableZombie);
    leader.free();
    let _ = publication.unpublish_process_if_matches(&leader);
    publication.cleanup();
}

/// TIPC handle double that records `close()`, used to observe whether the
/// process exit path closes process-local TIPC handles.
#[cfg(feature = "tipc")]
struct ExitObservingHandle {
    wait_state: tipc_handle::HandleWaitState,
    closed: AtomicBool,
}

#[cfg(feature = "tipc")]
impl ExitObservingHandle {
    fn new() -> Self {
        Self {
            wait_state: tipc_handle::HandleWaitState::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

#[cfg(feature = "tipc")]
impl tipc_handle::Handle for ExitObservingHandle {
    fn kind(&self) -> tipc_handle::HandleKind {
        tipc_handle::HandleKind::Port
    }

    fn poll(&self, _finalize: bool) -> tipc_handle::HandleEventMask {
        tipc_handle::HandleEventMask::NONE
    }

    fn register(
        &self,
        context: &mut kpoll::PollContext<'_>,
        _event_mask: tipc_handle::HandleEventMask,
    ) -> Result<(), kpoll::PollRegisterError> {
        self.wait_state.register(context)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn set_cookie(&self, cookie: usize) {
        self.wait_state.set_cookie(cookie);
    }

    fn cookie(&self) -> usize {
        self.wait_state.cookie()
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// Installs `handle` into the TIPC handle table of a fresh single-thread
/// process and returns the process, its task, and the process-local handle ID.
#[cfg(feature = "tipc")]
fn process_with_tipc_handle(
    pid: crate::Pid,
    handle: Arc<ExitObservingHandle>,
) -> (Arc<Process>, TaskInner, i32) {
    let (process, task) = process_with_address_space(pid, mapped_test_address_space());
    let handle_id = process
        .with_tipc_handles(|table| {
            table
                .write()
                .uctx_handle_install(handle as Arc<dyn tipc_handle::Handle>)
        })
        .expect("runtime must be attached")
        .expect("handle install must succeed");
    (process, task, handle_id)
}

#[cfg(feature = "tipc")]
#[def_test(serial)]
fn test_tipc_handles_close_before_process_exit_publication() {
    let handle = Arc::new(ExitObservingHandle::new());
    let (process, task, handle_id) = process_with_tipc_handle(8_140, handle.clone());

    // Check the exit publication contract of `do_exit()`: mm, files, fs,
    // and namespaces are released before closing TIPC handles and publishing
    // process completion.
    process.exit_mm().expect("runtime must be attached");
    task.as_thread().files().exit_files();
    process.exit_fs().expect("runtime must be attached");
    process.exit_namespaces().expect("runtime must be attached");
    process
        .close_all_tipc_handles()
        .expect("runtime must be attached");

    // The handle must be closed and detached before any parent waiter can
    // observe the exit, so a device shutdown after publication never sees a
    // live channel/port owner.
    assert!(handle.is_closed());
    let lookup = process
        .with_tipc_handles(|table| table.read().uctx_handle_get(handle_id).map(|_| ()))
        .expect("runtime must be attached");
    assert_eq!(lookup, Err(kerrno::KError::BadFileDescriptor));

    process_exit::finalize_process_exit(&process);
    assert!(process.is_exited());
    assert!(handle.is_closed());

    wait_reap::assert_reap_zombie_process(&process);
}

#[def_test(user, serial)]
fn task_files_unshare_preserves_siblings_and_clone_files_policy() {
    let (process, task) = build_prepared_test_user_task();
    let leader = task.as_thread();
    let file = crate::PidFd::new_file(&process, kvfs::OpenFlags::READ_WRITE.bits(), initial_cred())
        .unwrap();
    let fd = leader.files().add_file(1024, file.clone(), false).unwrap();
    let sibling = leader.clone_thread_in_process().unwrap();
    sibling.files().unshare_fd_table().unwrap();
    sibling.files().close_file(fd).unwrap();
    assert!(Arc::ptr_eq(&leader.files().get_file(fd).unwrap(), &file));
    assert_eq!(
        sibling.files().get_file(fd).err(),
        Some(kerrno::KError::BadFileDescriptor)
    );

    let private = leader
        .prepare_thread_clone_with_files(false)
        .unwrap()
        .into_parts()
        .0;
    let shared = leader.clone_thread_in_process().unwrap();
    shared.files().close_file(fd).unwrap();
    assert_eq!(
        leader.files().get_file(fd).err(),
        Some(kerrno::KError::BadFileDescriptor)
    );
    assert!(Arc::ptr_eq(&private.files().get_file(fd).unwrap(), &file));
    private.files().exit_files();
    process.discard_unpublished();
}

/// Arms and disarms CPU-time itimers and checks the process gate follows.
///
/// Wall-clock itimers must never open the gate; every arm/disarm path funnels
/// through the single timer-manager wrapper that re-derives it.
#[def_test(serial)]
fn test_cpu_timer_gate_tracks_cpu_itimer_arm_and_disarm() {
    use posix_types::ITimerType;

    let period = TimeSpan::from_nanos(1_000_000);
    let (process, _task) = build_prepared_test_user_task();
    assert!(!process.cpu_timers_armed());

    process
        .set_itimer(ITimerType::Real, period, period)
        .expect("wall-clock itimer must arm on an attached runtime");
    assert!(
        !process.cpu_timers_armed(),
        "wall-clock itimers must not open the CPU-time gate"
    );

    process
        .set_itimer(ITimerType::Virtual, TimeSpan::ZERO, period)
        .expect("CPU-time itimer must arm on an attached runtime");
    assert!(
        process.cpu_timers_armed(),
        "an armed CPU-time itimer must open the gate"
    );

    process
        .set_itimer(ITimerType::Virtual, TimeSpan::ZERO, TimeSpan::ZERO)
        .expect("disarming the CPU-time itimer must succeed");
    assert!(
        !process.cpu_timers_armed(),
        "disarming the last CPU-time itimer must close the gate"
    );
}

/// Checks the gate short-circuits the poll before the runtime is touched.
///
/// A process with no attached runtime distinguishes the two branches: the
/// closed gate returns an empty delivery list, while an open gate must reach
/// the runtime and report it as gone.
#[def_test(serial)]
fn test_cpu_timer_gate_skips_poll_before_runtime_lookup() {
    let process = ensure_init().fork(9_150);
    assert!(!process.cpu_timers_armed());

    let deliveries = process
        .poll_cpu_timers()
        .expect("gate-closed poll must not reach the runtime");
    assert!(deliveries.is_empty());

    process.set_cpu_timers_armed(true);
    assert_eq!(
        process.poll_cpu_timers().err(),
        Some(kerrno::KError::NoSuchProcess),
        "an open gate must reach the runtime before polling"
    );
}
