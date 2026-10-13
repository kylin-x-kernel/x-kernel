// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::task::Wake;
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Waker},
};

use kpoll::{IoEvents, PollRegistrations, Pollable};
use kvfs::{InodeOperations, NodeFlags, NodePermission, NodeType, Umode, VfsInodeInit};
use unittest::def_test;

use super::*;
use crate::port::TtyWrite;

struct Inode;
impl InodeOperations for Inode {}

#[derive(Default)]
struct HangupWake(AtomicBool);

impl Wake for HangupWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

struct TestPtyRange(DeviceId);

impl Drop for TestPtyRange {
    fn drop(&mut self) {
        drop(kvfs::cdev_del(self.0, UNIX98_PTY_MINOR_COUNT));
    }
}

#[def_test(serial)]
fn pty_slave_registration_conflict_rolls_back_only_its_master() {
    // Dedicated test majors avoid unregistering the live Unix98 drivers.
    let master = DeviceId::new(243, 0);
    let slave = DeviceId::new(244, 0);
    let occupied: Arc<dyn DeviceFileOps> = Arc::new(UnboundPty);
    kvfs::cdev_add(slave, UNIX98_PTY_MINOR_COUNT, occupied.clone()).unwrap();
    let _slave_cleanup = TestPtyRange(slave);
    let _master_cleanup = TestPtyRange(master);

    assert_eq!(
        register_pty_ranges(master, slave),
        Err(KError::ResourceBusy)
    );
    assert!(kvfs::cdev_del(master, UNIX98_PTY_MINOR_COUNT).is_none());
    let retained = kvfs::cdev_del(slave, UNIX98_PTY_MINOR_COUNT).unwrap();
    assert!(Arc::ptr_eq(&retained, &occupied));

    // The failed attempt must not prevent either range being registered later.
    assert_eq!(register_pty_ranges(master, slave), Ok(()));
    assert!(kvfs::cdev_del(master, UNIX98_PTY_MINOR_COUNT).is_some());
    assert!(kvfs::cdev_del(slave, UNIX98_PTY_MINOR_COUNT).is_some());
}

fn slave_inode() -> Arc<VfsInode> {
    VfsInode::new_file_with_inode_operations(
        Arc::new(()),
        Arc::new(Inode),
        NodeFlags::NON_CACHEABLE,
        VfsInodeInit::new(
            2,
            0,
            Umode::new(NodeType::CharacterDevice, NodePermission::default()),
        ),
    )
}

#[def_test(serial)]
fn pty_unpublished_reservations_count_and_rollback() {
    let before = pty_allocated_count();
    let pair = PtyPair::allocate().unwrap();
    let released = pair.number;
    // An unpublished pair consumes capacity even though no slave is visible.
    assert_eq!(pty_allocated_count(), before + 1);
    assert!(pty_slave_inode(released).is_none());
    drop(pair);
    assert_eq!(pty_allocated_count(), before);
    let replacement = PtyPair::allocate().unwrap();
    assert_eq!(replacement.number, released);
    drop(replacement);
    assert_eq!(pty_allocated_count(), before);
}

#[def_test(serial)]
fn pty_reservations_exhaust_and_reuse_capacity() {
    let before = pty_allocated_count();
    let mut reservations = Vec::new();
    while let Ok(number) = PtyNumber::reserve() {
        reservations.push(number);
    }
    assert_eq!(reservations.len(), pty_limit() - before);
    assert_eq!(pty_allocated_count(), pty_limit());
    assert_eq!(PtyNumber::reserve().err(), Some(KError::StorageFull));
    let released = reservations.pop().unwrap().0;
    assert_eq!(pty_allocated_count(), pty_limit() - 1);
    let replacement = PtyNumber::reserve().unwrap();
    assert_eq!(replacement.0, released);
    drop(replacement);
    drop(reservations);
    assert_eq!(pty_allocated_count(), before);
}

#[def_test(serial)]
fn pty_master_close_hides_node_but_slaves_retain_number() {
    let before = pty_allocated_count();
    let pair = PtyPair::allocate().unwrap();
    let number = pair.number;
    assert!(pair.acquire_open(TtyKind::Terminal).is_err());
    let master = pair.acquire_open(TtyKind::PtyMaster).unwrap();
    let inode = slave_inode();
    pair.publish_slave(inode.clone()).unwrap();
    assert!(Arc::ptr_eq(&pty_slave_inode(number).unwrap(), &inode));
    let slave = pair.acquire_open(TtyKind::Terminal).unwrap();
    let extra_slave = pair.acquire_open(TtyKind::Terminal).unwrap();
    drop(master);
    assert!(pty_slave_inode(number).is_none());
    assert!(!pty_slave_numbers().contains(&number));
    assert!(pair.acquire_open(TtyKind::Terminal).is_err());
    if pty_allocated_count() < pty_limit() {
        let other_pair = PtyPair::allocate().unwrap();
        assert_ne!(other_pair.number, number);
        drop(other_pair);
    } else {
        assert_eq!(PtyPair::allocate().err(), Some(KError::StorageFull));
    }
    drop(slave);
    assert_eq!(pty_allocated_count(), before + 1);
    drop(extra_slave);
    assert_eq!(pty_allocated_count(), before);

    // Keeping the old pair or inode alive does not retain its number, and a
    // successor does not restore the old pair's admission.
    let successor = PtyPair::allocate().unwrap();
    assert_eq!(successor.number, number);
    assert!(pair.acquire_open(TtyKind::PtyMaster).is_err());
    assert!(pair.acquire_open(TtyKind::Terminal).is_err());
}

#[def_test(serial)]
fn pty_last_slave_close_does_not_retire_live_master() {
    let before = pty_allocated_count();
    let pair = PtyPair::allocate().unwrap();
    let master = pair.acquire_open(TtyKind::PtyMaster).unwrap();
    let inode = slave_inode();
    pair.publish_slave(inode.clone()).unwrap();
    drop(pair.acquire_open(TtyKind::Terminal).unwrap());
    assert!(Arc::ptr_eq(&pty_slave_inode(pair.number).unwrap(), &inode));
    drop(pair.acquire_open(TtyKind::Terminal).unwrap());
    drop(master);
    assert!(pty_slave_inode(pair.number).is_none());
    assert_eq!(pty_allocated_count(), before);
}

#[def_test(user, serial)]
fn pty_master_close_preserves_unprocessed_input_before_read_eio() {
    let before = pty_allocated_count();
    let pair = PtyPair::allocate().unwrap();
    let master_open = pair.acquire_open(TtyKind::PtyMaster).unwrap();
    pair.publish_slave(slave_inode()).unwrap();
    let slave_open = pair.acquire_open(TtyKind::Terminal).unwrap();
    let (master, slave, work) = {
        let state = pair.state.lock();
        let resources = state.resources.as_ref().unwrap();
        (
            resources.master.clone(),
            resources.slave.clone(),
            resources.input_work.clone(),
        )
    };
    // Drain initial callbacks and disable this pair's asynchronous processing.
    // Accepted bytes must remain in ingress until the post-close read drives it.
    work.shutdown();

    // TTY device methods use the file's status flags, not its inode or contents.
    // A separate read-only directory file avoids changing live TTY registrations.
    let root = kprocess::current_fs_context().lock().root().clone();
    let file = kvfs::dentry_open(root, 0, kprocess::current_cred()).unwrap();
    let mut data = alloc::vec![b'a'; 700];
    data.push(b'\n');
    assert_eq!(
        DeviceFileOps::write(&*master, &file, &data, 0),
        Ok(data.len())
    );
    assert_eq!(master.writer.write_room(), 4096 - data.len());
    assert!(!slave.input.lock().poll_read());

    let wake = Arc::new(HangupWake::default());
    let waker = Waker::from(wake.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    Pollable::register(&*slave, &mut registrations.context(&context), IoEvents::IN).unwrap();
    assert!(!wake.0.load(Ordering::Relaxed));
    drop(master_open);
    assert!(wake.0.load(Ordering::Relaxed));
    assert!(Pollable::poll(&*slave).contains(IoEvents::ERR | IoEvents::HUP));
    assert_eq!(
        DeviceFileOps::write(&*slave, &file, b"reply", 0),
        Err(KError::Io)
    );

    // A private foreground group forces the background branch without moving
    // the calling process or altering its real session's controlling terminal.
    let foreground = kprocess::ProcessGroup::new_isolated_for_test();
    let session = foreground.session();
    let job_control = &slave.terminal.job_control;
    assert!(job_control.ensure_session(&session).unwrap());
    job_control.set_foreground(&foreground).unwrap();
    assert!(!job_control.current_in_foreground());
    let room = master.writer.write_room();
    for is_nonblocking in [true, false] {
        file.set_nonblocking(is_nonblocking);
        let mut bytes = [0; 31];
        assert_eq!(
            DeviceFileOps::read(&*slave, &file, &mut bytes, 0),
            Err(KError::Io)
        );
        assert_eq!(master.writer.write_room(), room);
        assert!(!slave.input.lock().poll_read());
    }
    assert!(job_control.clear_session_if_matches(&session));
    assert!(job_control.current_in_foreground());

    file.set_nonblocking(true);
    let mut received = Vec::new();
    while received.len() < data.len() {
        let mut bytes = [0; 31];
        let count = DeviceFileOps::read(&*slave, &file, &mut bytes, 0).unwrap();
        assert!(count != 0);
        received.extend_from_slice(&bytes[..count]);
        // Exercise both file modes while canonical delivery still exceeds
        // cooked capacity and must resume across successive reads.
        file.set_nonblocking(!file.is_nonblocking());
    }
    assert_eq!(received, data);
    for is_nonblocking in [true, false] {
        file.set_nonblocking(is_nonblocking);
        let mut bytes = [0; 31];
        assert_eq!(
            DeviceFileOps::read(&*slave, &file, &mut bytes, 0),
            Err(KError::Io)
        );
    }
    drop(registrations);
    drop(slave_open);
    assert_eq!(pty_allocated_count(), before);
}

#[def_test(serial)]
fn pty_slave_open_racing_master_close_never_resurrects_pair() {
    let before = pty_allocated_count();
    for _ in 0..32 {
        let pair = PtyPair::allocate().unwrap();
        let master = pair.acquire_open(TtyKind::PtyMaster).unwrap();
        pair.publish_slave(slave_inode()).unwrap();
        let start = Arc::new(ksync::Semaphore::new(0));
        let close_start = start.clone();
        let closer = ktask::spawn(move || {
            close_start.acquire();
            drop(master);
        });
        start.release();
        let slave = pair.acquire_open(TtyKind::Terminal);
        closer.join();
        assert!(pty_slave_inode(pair.number).is_none());
        assert!(pair.acquire_open(TtyKind::Terminal).is_err());
        if slave.is_ok() {
            assert_eq!(pty_allocated_count(), before + 1);
        }
        drop(slave);
        assert_eq!(pty_allocated_count(), before);
    }
}
