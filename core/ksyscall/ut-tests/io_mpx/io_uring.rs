// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kasyncio::abi::{
    IORING_ENTER_GETEVENTS, IORING_FEAT_SINGLE_MMAP, IORING_OFF_SQES, IORING_OFF_SQ_RING,
    IORING_OP_NOP, IORING_OP_READ, IoUringCqe, IoUringParams, IoUringSqe,
};
use kerrno::{KError, LinuxError};
use linux_raw_sys::general::{MAP_SHARED, PROT_READ, PROT_WRITE};
use posix_mm::{sys_mmap, sys_munmap};
use posix_types::{UserConstPtr, UserPtr};
use unittest::def_test;
use unittest_support::TestUserValue;

use super::*;

fn setup_ring(entries: u32) -> (i32, IoUringParams, usize, usize, usize) {
    let params = TestUserValue::<IoUringParams>::from_value(IoUringParams::default())
        .expect("params user value");
    let fd = sys_io_uring_setup(entries, UserPtr::from(params.as_user_ptr() as usize))
        .expect("io_uring_setup must succeed") as i32;
    let value = params.read();
    assert_eq!(value.sq_entries, entries);
    assert_eq!(value.cq_entries, entries);
    assert_ne!(value.features & IORING_FEAT_SINGLE_MMAP, 0);

    let sq_len = value.sq_off.array as usize + value.sq_entries as usize * 4;
    let cq_len = value.cq_off.cqes as usize + value.cq_entries as usize * 16;
    let ring_len = memaddr::align_up_4k(sq_len.max(cq_len));
    let ring_va = sys_mmap(
        0,
        ring_len,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        IORING_OFF_SQ_RING as i64,
    )
    .expect("SQ/CQ ring mmap") as usize;
    let sqe_va = sys_mmap(
        0,
        entries as usize * 64,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        IORING_OFF_SQES as i64,
    )
    .expect("SQE mmap") as usize;
    (fd, value, ring_va, sqe_va, ring_len)
}

#[def_test(user, serial)]
fn setup_enter_publishes_the_submitted_completion() {
    let (fd, params, ring_va, sqe_va, ring_len) = setup_ring(8);

    UserPtr::<u32>::from(ring_va + params.sq_off.array as usize)
        .write_vm(0)
        .expect("write SQ array index");
    UserPtr::from(sqe_va)
        .write_vm(IoUringSqe {
            opcode: IORING_OP_NOP,
            user_data: 0x5A5A_1234,
            ..IoUringSqe::default()
        })
        .expect("write SQE");
    UserPtr::<u32>::from(ring_va + params.sq_off.tail as usize)
        .write_vm(1)
        .expect("publish SQ tail");

    let submitted = sys_io_uring_enter(
        fd,
        1,
        1,
        IORING_ENTER_GETEVENTS,
        UserConstPtr::default(),
        0,
    )
    .expect("io_uring_enter must succeed");
    assert_eq!(submitted, 1);

    let cqe: IoUringCqe = UserPtr::from(ring_va + params.cq_off.cqes as usize)
        .read_vm()
        .expect("read CQE");
    assert_eq!(cqe.user_data, 0x5A5A_1234);
    assert_eq!(cqe.res, 0);

    UserPtr::<u32>::from(ring_va + params.cq_off.head as usize)
        .write_vm(1)
        .expect("consume CQE");
    sys_munmap(sqe_va, 8 * 64).expect("munmap SQEs");
    sys_munmap(ring_va, ring_len).expect("munmap ring");
    posix_fs::sys_close(fd).expect("close");
}

#[def_test(user, serial)]
fn setup_and_enter_reject_invalid_input() {
    let mut params = TestUserValue::<IoUringParams>::from_value(IoUringParams::default())
        .expect("params user value");
    let ptr = UserPtr::from(params.as_user_ptr() as usize);

    assert_eq!(
        sys_io_uring_setup(0, ptr),
        Err(KError::InvalidInput),
        "zero entry counts are rejected"
    );
    assert_eq!(
        sys_io_uring_setup(u32::MAX, ptr),
        Err(KError::InvalidInput),
        "oversized entry counts are rejected"
    );

    let mut bad_flags = IoUringParams {
        flags: 1,
        ..IoUringParams::default()
    };
    params.write(bad_flags);
    assert_eq!(
        sys_io_uring_setup(8, ptr),
        Err(KError::InvalidInput),
        "unknown setup flags are rejected"
    );
    bad_flags.flags = 0;
    params.write(bad_flags);

    let reserved = IoUringParams {
        resv: [1, 0, 0],
        ..IoUringParams::default()
    };
    params.write(reserved);
    assert_eq!(
        sys_io_uring_setup(8, ptr),
        Err(KError::InvalidInput),
        "nonzero reserved params are rejected"
    );
    params.write(IoUringParams::default());

    let (fd, _params, ring_va, sqe_va, ring_len) = setup_ring(8);
    assert_eq!(
        sys_io_uring_enter(fd, 0, 0, 1 << 7, UserConstPtr::default(), 0),
        Err(KError::InvalidInput),
        "unknown enter flags are rejected"
    );
    sys_munmap(sqe_va, 8 * 64).expect("munmap SQEs");
    sys_munmap(ring_va, ring_len).expect("munmap ring");
    posix_fs::sys_close(fd).expect("close");
}

#[def_test(user, serial)]
fn out_of_range_array_index_is_dropped_without_a_cqe() {
    let (fd, params, ring_va, sqe_va, ring_len) = setup_ring(8);

    // An array entry pointing past the SQE array is dropped, like Linux's
    // `io_get_sqe`: no CQE and SQ_DROPPED advances.
    UserPtr::<u32>::from(ring_va + params.sq_off.array as usize)
        .write_vm(8)
        .expect("invalid SQ array index");
    UserPtr::<u32>::from(ring_va + params.sq_off.tail as usize)
        .write_vm(1)
        .expect("publish tail");
    assert_eq!(
        sys_io_uring_enter(fd, 1, 0, 0, UserConstPtr::default(), 0).expect("enter drops"),
        0
    );
    let dropped: u32 = UserPtr::from(ring_va + params.sq_off.dropped as usize)
        .read_vm()
        .expect("SQ dropped");
    assert_eq!(dropped, 1);
    let cq_tail: u32 = UserPtr::from(ring_va + params.cq_off.tail as usize)
        .read_vm()
        .expect("CQ tail");
    assert_eq!(cq_tail, 0, "a dropped entry must not produce a CQE");

    // The following entry is still consumable.
    UserPtr::<u32>::from(ring_va + params.sq_off.array as usize + 4)
        .write_vm(0)
        .expect("SQ array index");
    UserPtr::from(sqe_va)
        .write_vm(IoUringSqe {
            opcode: IORING_OP_NOP,
            user_data: 0x4444,
            ..IoUringSqe::default()
        })
        .expect("SQE");
    UserPtr::<u32>::from(ring_va + params.sq_off.tail as usize)
        .write_vm(2)
        .expect("publish tail");
    assert_eq!(
        sys_io_uring_enter(fd, 1, 1, IORING_ENTER_GETEVENTS, UserConstPtr::default(), 0)
            .expect("enter after drop"),
        1
    );
    let cqe: IoUringCqe = UserPtr::from(ring_va + params.cq_off.cqes as usize)
        .read_vm()
        .expect("read CQE");
    assert_eq!(cqe.user_data, 0x4444);

    sys_munmap(sqe_va, 8 * 64).expect("munmap SQEs");
    sys_munmap(ring_va, ring_len).expect("munmap ring");
    posix_fs::sys_close(fd).expect("close");
}

#[def_test(user, serial)]
fn short_submission_clamps_to_the_published_entries() {
    let (fd, params, ring_va, sqe_va, ring_len) = setup_ring(8);

    UserPtr::<u32>::from(ring_va + params.sq_off.array as usize)
        .write_vm(0)
        .expect("SQ array index");
    UserPtr::from(sqe_va)
        .write_vm(IoUringSqe {
            opcode: IORING_OP_NOP,
            user_data: 0x5555,
            ..IoUringSqe::default()
        })
        .expect("SQE");
    UserPtr::<u32>::from(ring_va + params.sq_off.tail as usize)
        .write_vm(1)
        .expect("publish one entry");

    // Linux truncates to the published tail; it must not fail the call.
    assert_eq!(
        sys_io_uring_enter(fd, 64, 1, IORING_ENTER_GETEVENTS, UserConstPtr::default(), 0)
            .expect("short submission"),
        1
    );
    let cqe: IoUringCqe = UserPtr::from(ring_va + params.cq_off.cqes as usize)
        .read_vm()
        .expect("read CQE");
    assert_eq!(cqe.user_data, 0x5555);
    assert_eq!(cqe.res, 0);

    sys_munmap(sqe_va, 8 * 64).expect("munmap SQEs");
    sys_munmap(ring_va, ring_len).expect("munmap ring");
    posix_fs::sys_close(fd).expect("close");
}

#[def_test(user, serial)]
fn unsupported_opcode_completes_with_einval_and_does_not_wedge_the_ring() {
    let (fd, params, ring_va, sqe_va, ring_len) = setup_ring(8);
    let array = ring_va + params.sq_off.array as usize;

    // SQE 0: READ is not executable in phase 1; it must complete with EINVAL
    // instead of a fake success.
    UserPtr::<u32>::from(array).write_vm(0).expect("array[0]");
    UserPtr::from(sqe_va)
        .write_vm(IoUringSqe {
            opcode: IORING_OP_READ,
            user_data: 0x1111,
            ..IoUringSqe::default()
        })
        .expect("sqe 0");
    UserPtr::<u32>::from(ring_va + params.sq_off.tail as usize)
        .write_vm(1)
        .expect("publish tail");
    assert_eq!(
        sys_io_uring_enter(fd, 1, 0, 0, UserConstPtr::default(), 0).expect("enter"),
        1
    );
    let cqe: IoUringCqe = UserPtr::from(ring_va + params.cq_off.cqes as usize)
        .read_vm()
        .expect("read rejection CQE");
    assert_eq!(cqe.user_data, 0x1111);
    assert_eq!(cqe.res, -LinuxError::EINVAL.into_raw());

    // The next SQE must still run: the rejection advanced the SQ head.
    UserPtr::<u32>::from(ring_va + params.cq_off.head as usize)
        .write_vm(1)
        .expect("consume CQE");
    UserPtr::<u32>::from(array + 4).write_vm(1).expect("array[1]");
    UserPtr::from(sqe_va + 64)
        .write_vm(IoUringSqe {
            opcode: IORING_OP_NOP,
            user_data: 0x2222,
            ..IoUringSqe::default()
        })
        .expect("sqe 1");
    UserPtr::<u32>::from(ring_va + params.sq_off.tail as usize)
        .write_vm(2)
        .expect("publish tail");
    assert_eq!(
        sys_io_uring_enter(fd, 1, 1, IORING_ENTER_GETEVENTS, UserConstPtr::default(), 0)
            .expect("enter after rejection"),
        1
    );
    let cqe: IoUringCqe = UserPtr::from(ring_va + params.cq_off.cqes as usize + 16)
        .read_vm()
        .expect("read NOP CQE");
    assert_eq!(cqe.user_data, 0x2222);
    assert_eq!(cqe.res, 0);

    sys_munmap(sqe_va, 8 * 64).expect("munmap SQEs");
    sys_munmap(ring_va, ring_len).expect("munmap ring");
    posix_fs::sys_close(fd).expect("close");
}

#[def_test(user, serial)]
fn enter_clamps_min_complete_to_the_cq_depth() {
    let (fd, params, ring_va, sqe_va, ring_len) = setup_ring(8);
    let array = ring_va + params.sq_off.array as usize;

    for index in 0..8u32 {
        UserPtr::<u32>::from(array + index as usize * 4)
            .write_vm(index)
            .expect("SQ array index");
        UserPtr::from(sqe_va + index as usize * 64)
            .write_vm(IoUringSqe {
                opcode: IORING_OP_NOP,
                user_data: index as u64,
                ..IoUringSqe::default()
            })
            .expect("SQE");
    }
    UserPtr::<u32>::from(ring_va + params.sq_off.tail as usize)
        .write_vm(8)
        .expect("publish tail");

    // Linux clamps a wait target above the CQ depth instead of failing.
    assert_eq!(
        sys_io_uring_enter(fd, 8, 9, IORING_ENTER_GETEVENTS, UserConstPtr::default(), 0)
            .expect("clamped enter"),
        8
    );
    let cq_tail: u32 = UserPtr::from(ring_va + params.cq_off.tail as usize)
        .read_vm()
        .expect("CQ tail");
    assert_eq!(cq_tail, 8, "every accepted NOP must complete");

    sys_munmap(sqe_va, 8 * 64).expect("munmap SQEs");
    sys_munmap(ring_va, ring_len).expect("munmap ring");
    posix_fs::sys_close(fd).expect("close");
}

#[def_test(user, serial)]
fn enter_counts_cqes_already_visible_in_the_ring() {
    let (fd, params, ring_va, sqe_va, ring_len) = setup_ring(8);

    UserPtr::<u32>::from(ring_va + params.sq_off.array as usize)
        .write_vm(0)
        .expect("SQ array index");
    UserPtr::from(sqe_va)
        .write_vm(IoUringSqe {
            opcode: IORING_OP_NOP,
            user_data: 0x3333,
            ..IoUringSqe::default()
        })
        .expect("SQE");
    UserPtr::<u32>::from(ring_va + params.sq_off.tail as usize)
        .write_vm(1)
        .expect("publish tail");
    assert_eq!(
        sys_io_uring_enter(fd, 1, 0, 0, UserConstPtr::default(), 0).expect("submit"),
        1
    );

    // Leave the CQE in the shared ring and wait for it again: an already
    // visible completion satisfies min_complete without blocking.
    assert_eq!(
        sys_io_uring_enter(fd, 0, 1, IORING_ENTER_GETEVENTS, UserConstPtr::default(), 0)
            .expect("wait sees the visible CQE"),
        0
    );
    let cqe: IoUringCqe = UserPtr::from(ring_va + params.cq_off.cqes as usize)
        .read_vm()
        .expect("read CQE");
    assert_eq!(cqe.user_data, 0x3333);

    sys_munmap(sqe_va, 8 * 64).expect("munmap SQEs");
    sys_munmap(ring_va, ring_len).expect("munmap ring");
    posix_fs::sys_close(fd).expect("close");
}
