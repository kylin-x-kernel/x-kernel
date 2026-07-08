// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for kprocess

#![cfg(unittest)]

use alloc::{format, string::String, sync::Arc, vec, vec::Vec};

use kcred::Credentials;
use ktask::{TaskInner, current, prepare_task};
use unittest::{assert, assert_eq, def_test};

use crate::{
    AsThread, Process, build_process_thread, current_user_process, install_process_thread,
    process::INIT_PROC,
    process_exit, procfs,
    publication::{prepare_user_task, process_publication, task_identity_matches_thread},
    publish_user_task, scheduler, wait_reap,
};

fn ensure_init() -> Arc<Process> {
    if let Some(p) = INIT_PROC.get() {
        return p.clone();
    }

    // In unittest mode, INIT_PROC may already have been initialized by a
    // synthetic runtime used by earlier tests. If not, create one here.
    Process::new_init(1)
}

fn build_prepared_test_user_task() -> (Arc<Process>, TaskInner) {
    let parent = current_user_process();
    let task_number =
        kidentity::allocate_root_pid_handle().expect("test leader identity should allocate");
    let mut task = TaskInner::new_user(
        || {},
        String::from("test-user-thread"),
        16 * 1024,
        task_number.clone(),
    );
    let pid = task_number.root_nr();
    let process = parent.fork_with_task_number(task_number.clone(), Some(ksignal::Signo::SIGCHLD));

    let exe_path = parent
        .exe_path()
        .expect("current process must expose a live exec path");
    let cmdline = parent
        .cmdline()
        .expect("current process must expose a live cmdline");
    let address_space = parent
        .address_space()
        .expect("current process must expose a live address space");
    let fs_context = parent
        .fs_context()
        .expect("current process must expose a live fs context");
    let signal_actions = parent
        .signal_actions()
        .expect("current process must expose live signal actions");
    let credentials = parent
        .credentials_snapshot()
        .unwrap_or_else(|_| Credentials::root());

    install_process_thread(
        &mut task,
        process.clone(),
        exe_path,
        cmdline,
        address_space,
        fs_context,
        signal_actions,
        credentials,
    );

    task.set_name(format!("test-user-thread-{pid}").as_str());
    (process, task)
}

fn publish_test_thread(process: &Arc<Process>, tid: crate::Tid) -> ktask::KtaskRef {
    let task_number = kidentity::PidHandle::fixed_root(tid);
    let mut task = TaskInner::new_user(|| {}, format!("test-thread-{tid}"), 16 * 1024, task_number);
    let mut aspace = memspace::MmSpace::new_user_empty().expect("user mmspace should allocate");
    ksignal::map_signal_trampoline(&mut aspace).expect("signal trampoline should map");
    let address_space = Arc::new(ksync::Mutex::new(aspace));
    let fs_context = fs_context::copy_init_fs_struct();
    let signal_actions = Arc::new(ksync::spin::SpinNoIrq::new(
        ksignal::api::SignalActions::default(),
    ));
    let credentials = Credentials::root();

    install_process_thread(
        &mut task,
        process.clone(),
        String::from("[test-thread]"),
        Arc::new(vec![]),
        address_space,
        fs_context,
        signal_actions,
        credentials,
    );

    let task = prepare_task(task);
    process_publication().publish_task(&task);
    task
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
    let _leader_task = publish_test_thread(&child, child_pid);
    assert!(child.threads().contains(&child_pid));

    // Add secondary thread
    let _sibling_task = publish_test_thread(&child, child_pid + 1);
    let threads = child.threads();
    assert!(threads.contains(&(child_pid + 1)));

    // Remove secondary thread
    let is_last = child.exit_thread(child_pid + 1, 0);
    assert!(!is_last); // main thread is still there

    // Remove main thread
    let is_last = child.exit_thread(child_pid, 0);
    assert!(is_last);

    // Test Group Exit
    assert!(!child.is_group_exited());
    child.group_exit();
    assert!(child.is_group_exited());

    // Test Zombie/Exit
    assert!(!child.is_zombie());
    child.exit();
    assert!(child.is_zombie());

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
    p1.exit();
    p1.free();
    p2.exit();
    p2.free();
    p1_child.exit();
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

    mover.exit();
    mover.free();
    leader.exit();
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

    parent.exit();

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
    child.exit();
    child.free();
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

    first.exit();
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

    second.exit();
    second.free();
}

#[def_test(custom, serial)]
fn test_zombie_process_is_not_live_even_if_runtime_still_exists() {
    let (proc, prepared) = build_prepared_test_user_task();
    let publication = process_publication();

    assert!(proc.runtime_ref().is_some());
    publication.publish_process_identity(&proc);
    assert!(
        publication.live_process(proc.pid()).is_ok(),
        "non-zombie process should be live before exit"
    );

    process_exit::finalize_process_exit(&proc);

    assert!(proc.is_zombie());
    assert!(
        proc.runtime_ref().is_some(),
        "runtime may still exist while the owning thread object is not dropped yet"
    );
    assert!(
        publication.live_process(proc.pid()).is_err(),
        "zombie process must stop participating in live-process lookups even before runtime drops"
    );

    drop(prepared);
    wait_reap::reap_zombie_process(&proc);
}

#[def_test(serial)]
fn test_group_exit_prevents_late_thread_exit_from_overwriting_exit_code() {
    let init = ensure_init();
    let proc = init.fork(500);

    let _leader_task = publish_test_thread(&proc, 500);
    let _sibling_task = publish_test_thread(&proc, 501);

    let not_last = proc.exit_thread(500, 11);
    assert!(!not_last, "first exiting thread should not be last");
    assert_eq!(
        proc.exit_code(),
        11,
        "early exiting thread publishes initial exit code"
    );

    proc.group_exit();
    let is_last = proc.exit_thread(501, 22);
    assert!(is_last, "second exiting thread should be the last thread");
    assert_eq!(
        proc.exit_code(),
        11,
        "late exits after group_exit must not overwrite the published group exit code"
    );

    proc.exit();
    proc.free();
}

#[def_test(serial)]
fn test_lifecycle_accumulates_exited_thread_and_child_cpu_time() {
    let init = ensure_init();
    let proc = init.fork(600);

    proc.accumulate_exited_thread_time(11, 22);
    proc.accumulate_exited_thread_time(33, 44);
    proc.accumulate_child_time(55, 66);
    proc.accumulate_child_time(77, 88);

    assert_eq!(proc.exited_thread_time_ns(), (44, 66));
    assert_eq!(proc.child_time_ns(), (132, 154));

    proc.exit();
    proc.free();
}

#[def_test(custom, serial)]
fn test_published_task_lookup_matches_current_user_thread() {
    let task = current().clone();
    let tid = task.as_thread().tid();
    let process = current_user_process();
    let publication = process_publication();

    publication.publish_task(&task);

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
}

#[def_test(custom, serial)]
fn test_current_process_mutation_helpers_preserve_process_boundary() {
    let process = current_user_process();

    let old_umask = process
        .replace_umask(0o077)
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
        .set_exec_metadata(exe_path.clone(), cmdline.clone())
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
        .set_exec_metadata(old_exe_path, old_cmdline)
        .expect("current process must restore its previous exec metadata");
}

#[def_test(custom, serial)]
fn test_prepare_thread_clone_defers_tid_visibility_until_publication() {
    let thread = crate::current_user_thread();
    let process = thread.process().clone();
    let prepared = thread
        .prepare_thread_clone()
        .expect("thread clone should allocate a sibling tid");
    let tid = prepared.tid();

    assert!(
        !process.threads().contains(&tid),
        "prepared sibling tid must stay hidden until task publication"
    );

    let (cloned, task_number) = prepared.into_parts();
    let mut task = TaskInner::new_user(
        || {},
        String::from("prepared-thread"),
        16 * 1024,
        task_number,
    );
    // SAFETY: `cloned` is the freshly prepared thread object for `task` and is installed
    // exactly once before publication or activation.
    *task.task_ext_mut() = Some(unsafe { ktask::KTaskExt::from_impl(cloned) });
    let published = prepare_user_task(task).publish();

    let is_last = process.exit_thread(tid, 0);
    assert!(
        !is_last,
        "published sibling thread removal must not tear down the whole process"
    );
    drop(published);
}

#[def_test(custom, serial)]
fn test_process_owns_tid_requires_published_task_binding() {
    let (process, _prepared) = build_prepared_test_user_task();

    assert!(
        !scheduler::process_owns_tid(&process, process.pid()),
        "prepared thread identity must stay hidden until publication installs the process-owned \
         task binding"
    );
}

#[def_test(custom, serial)]
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

    let published = prepare_user_task(prepared).publish();

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
    publication.unpublish_process(process.pid());
    process.exit();
    process.free();
    publication.cleanup();
}

#[def_test(custom, serial)]
fn test_publish_user_task_exposes_handle_before_activation() {
    let parent = current_user_process();
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

    let published = publish_user_task(prepared);

    let published_task = publication
        .task(process.pid())
        .expect("published task should resolve through the task registry");
    assert!(
        Arc::ptr_eq(published.task(), &published_task),
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

#[def_test(serial)]
fn test_prepare_user_task_rejects_mismatched_task_and_thread_identity() {
    let task_number = kidentity::allocate_root_pid_handle().expect("task identity should allocate");
    let mismatched_number =
        kidentity::allocate_root_pid_handle().expect("mismatched identity should allocate");
    let mut task = TaskInner::new_user(
        || {},
        String::from("mismatched-user-thread"),
        16 * 1024,
        task_number.clone(),
    );
    let process = Process::new_init_with_task_number(task_number);
    let mut aspace = memspace::MmSpace::new_user_empty().expect("user mmspace should allocate");
    ksignal::map_signal_trampoline(&mut aspace).expect("signal trampoline should map");

    let thread = build_process_thread(
        process,
        mismatched_number,
        String::from("[mismatch-test]"),
        Arc::new(vec![]),
        Arc::new(ksync::Mutex::new(aspace)),
        fs_context::copy_init_fs_struct(),
        Arc::new(ksync::spin::SpinNoIrq::new(
            ksignal::api::SignalActions::default(),
        )),
        Credentials::root(),
    );

    // SAFETY: This intentionally installs a mismatched thread payload to verify that
    // prepare_user_task rejects inconsistent task/thread identities before publication.
    *task.task_ext_mut() = Some(unsafe { ktask::KTaskExt::from_impl(thread) });
    let task = prepare_task(task);
    assert!(
        !task_identity_matches_thread(&task),
        "publication must reject inconsistent task/thread identities before visibility changes"
    );
}

#[def_test(custom, serial)]
fn test_zombie_process_stays_published_until_reaped() {
    let (process, prepared) = build_prepared_test_user_task();
    let child_pid = process.pid();
    let publication = process_publication();

    assert!(
        publication.published_process(child_pid).is_err(),
        "prepared child must not be visible before explicit publication"
    );

    let published = prepare_user_task(prepared).publish();

    let published_before_exit = publication
        .published_process(child_pid)
        .expect("published child must resolve before exit");
    assert!(
        Arc::ptr_eq(&published_before_exit, &process),
        "published registry must expose the stable child process identity"
    );

    process.exit();
    assert!(process.is_zombie(), "exited child must become zombie");
    assert!(
        publication.published_process(child_pid).is_ok(),
        "zombie child must stay published until an explicit reap/removal step"
    );

    process.free();
    publication.unpublish_process(child_pid);
    assert!(
        publication.published_process(child_pid).is_err(),
        "reaped child must disappear from the published PID registry"
    );

    drop(published);
    publication.cleanup();
}

#[def_test(custom, serial)]
fn test_procfs_visibility_requires_representative_task() {
    let (process, prepared) = build_prepared_test_user_task();
    let pid = process.pid();
    let publication = process_publication();
    let published = prepare_user_task(prepared).publish();

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

    process.exit();
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
    publication.unpublish_process(pid);
    publication.cleanup();
}

#[def_test(custom, serial)]
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

    process.exit();
    process.free();
    publication.unpublish_process(process.pid());
    publication.cleanup();
}

#[def_test(custom, serial)]
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

    sibling.exit();
    sibling.free();
    publication.unpublish_process(sibling.pid());
    leader.exit();
    leader.free();
    publication.unpublish_process(leader.pid());
    publication.cleanup();
}
