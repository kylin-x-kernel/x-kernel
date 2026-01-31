//! Unit tests for kprocess

#![cfg(unittest)]

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use crate::{Process, process::INIT_PROC};

fn ensure_init() -> Arc<Process> {
    if let Some(p) = INIT_PROC.get() {
        return p.clone();
    }
    // Assume pid 1 is fine for init in tests
    Process::new_init(1)
}

#[def_test]
fn test_process_lifecycle() {
    let init = ensure_init();
    assert_eq!(init.pid(), 1);

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
    child.add_thread(child_pid);
    assert!(child.threads().contains(&child_pid));

    // Add secondary thread
    child.add_thread(child_pid + 1);
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

#[def_test]
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
