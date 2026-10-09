// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kvfs::{InodeOperations, NodeFlags, NodePermission, NodeType, Umode, VfsInodeInit};
use unittest::def_test;

use super::*;

struct Inode;
impl InodeOperations for Inode {}

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
