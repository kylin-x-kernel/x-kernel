// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use kcred::{CAP_SYS_ADMIN, CapSet, CapSets, Cred};
use unittest::def_test;
use unittest_support::TestUserBuffer;

use super::*;

use crate::{has_ipc_control_permission, has_ipc_permission, has_ipc_permission_in};
use kcred::{UserNamespace, initial_user_namespace};

fn perm(uid: u32, gid: u32, cuid: u32, cgid: u32, mode: u32) -> IpcPerm {
    IpcPerm {
        key: 0,
        uid,
        gid,
        cuid,
        cgid,
        mode,
        seq: 0,
        pad: 0,
        abi_pad: 0,
        unused0: 0,
        unused1: 0,
    }
}

#[def_test]
fn ipc_set_rejects_unrepresentable_owner_ids() {
    assert_eq!(
        validate_set_perm(&perm(INVALID_UID, 100, 1000, 100, 0o600)),
        Err(KError::from(LinuxError::EINVAL))
    );
    assert_eq!(
        validate_set_perm(&perm(1000, INVALID_GID, 1000, 100, 0o600)),
        Err(KError::from(LinuxError::EINVAL))
    );
    assert_eq!(
        validate_set_perm(&perm(1000, 100, 1000, 100, 0o600)),
        Ok(())
    );
}

#[def_test]
fn ipc_set_copies_the_user_buffer_before_queue_lookup_or_authority() {
    // Linux `ksys_msgctl()` copies `msqid_ds` before it resolves the queue or
    // checks `ipcctl_obtain_check()`. A null buffer must therefore be `EFAULT`
    // even for a missing queue and an unprivileged caller, not `EINVAL` or
    // `EPERM`.
    assert_eq!(
        sys_msgctl(0, IPC_SET, UserPtr::<u8>::from(0)),
        Err(KError::BadAddress)
    );
    // The negative-ID check precedes the switch in `ksys_msgctl()`, so it wins
    // over the `IPC_SET` copy as well.
    assert_eq!(
        sys_msgctl(-1, IPC_SET, UserPtr::<u8>::from(0)),
        Err(KError::from(LinuxError::EINVAL))
    );
}

#[def_test]
fn test_message_queue_preserves_fifo_order_for_plain_receive() {
    let mut queue = MessageQueue::new(1, 0o600, 1, 0, 0);
    queue.enqueue_message(2, b"first".to_vec()).unwrap();
    queue.enqueue_message(1, b"second".to_vec()).unwrap();

    let (index, mtype, data) = queue.find_first_message().expect("first queued message");
    assert_eq!(index, 0);
    assert_eq!(mtype, 2);
    assert_eq!(data, b"first");
}

#[def_test]
fn test_message_queue_type_selection_matches_linux_basics() {
    let mut queue = MessageQueue::new(2, 0o600, 1, 0, 0);
    queue.enqueue_message(5, b"type5-a".to_vec()).unwrap();
    queue.enqueue_message(2, b"type2".to_vec()).unwrap();
    queue.enqueue_message(5, b"type5-b".to_vec()).unwrap();
    queue.enqueue_message(3, b"type3".to_vec()).unwrap();

    let (_, exact_type, exact_data) = queue.find_message_by_type(5).expect("type 5");
    assert_eq!(exact_type, 5);
    assert_eq!(exact_data, b"type5-a");

    let (_, except_type, except_data) = queue.find_message_not_equal(5).expect("type != 5");
    assert_eq!(except_type, 2);
    assert_eq!(except_data, b"type2");

    let (_, le_type, le_data) = queue.find_message_less_equal(4).expect("type <= 4");
    assert_eq!(le_type, 2);
    assert_eq!(le_data, b"type2");
}

#[def_test]
fn test_message_queue_remove_updates_accounting() {
    let mut queue = MessageQueue::new(3, 0o600, 1, 0, 0);
    queue.enqueue_message(1, vec![1, 2, 3]).unwrap();
    queue.enqueue_message(2, vec![4, 5]).unwrap();

    assert_eq!(queue.get_total_message_count(), 2);
    assert_eq!(queue.total_bytes, 5);
    assert_eq!(queue.msqid_ds.msg_qnum, 2);

    let removed = queue.remove_message_by_index(0).expect("remove first");
    assert_eq!(removed.mtype, 1);
    assert_eq!(removed.data, vec![1, 2, 3]);
    assert_eq!(queue.get_total_message_count(), 1);
    assert_eq!(queue.total_bytes, 2);
    assert_eq!(queue.msqid_ds.msg_qnum, 1);
    assert_eq!(queue.msqid_ds.msg_cbytes, 2);
}

#[def_test]
fn ipc_permission_uses_creator_and_owner_ids_for_the_owner_class() {
    let owner = Cred::new(1000, 100);
    let creator = Cred::new(2000, 200);
    let other = Cred::new(3000, 300);

    assert!(has_ipc_permission(
        &perm(1000, 200, 2000, 200, 0o400),
        &owner,
        IPC_READ
    ));
    assert!(!has_ipc_permission(
        &perm(1000, 200, 2000, 200, 0o400),
        &owner,
        IPC_WRITE
    ));
    assert!(has_ipc_permission(
        &perm(1000, 200, 2000, 200, 0o200),
        &creator,
        IPC_WRITE
    ));
    assert!(!has_ipc_permission(
        &perm(1000, 200, 2000, 200, 0o200),
        &creator,
        IPC_READ
    ));
    assert!(!has_ipc_permission(
        &perm(1000, 200, 2000, 200, 0o600),
        &other,
        IPC_READ
    ));
}

#[def_test]
fn ipc_permission_uses_creator_and_owner_groups_and_supplementary_groups() {
    let mut cred = Cred::new(1000, 100);
    cred.set_supplementary_groups(vec![200]);

    // The creator GID selects the group class through a supplementary group.
    assert!(has_ipc_permission(
        &perm(3000, 400, 4000, 200, 0o040),
        &cred,
        IPC_READ
    ));
    assert!(!has_ipc_permission(
        &perm(3000, 400, 4000, 200, 0o040),
        &cred,
        IPC_WRITE
    ));

    // The current owner GID does too when it is the filesystem GID.
    let owner_group = Cred::new(1000, 400);
    assert!(has_ipc_permission(
        &perm(3000, 400, 4000, 500, 0o020),
        &owner_group,
        IPC_WRITE
    ));
    assert!(!has_ipc_permission(
        &perm(3000, 400, 4000, 500, 0o020),
        &owner_group,
        IPC_READ
    ));
}

#[def_test]
fn removed_queue_reports_permission_before_eidrm() {
    let mut queue = MessageQueue::new(1, 0o600, 1, 1000, 100);
    queue.mark_removed = true;
    let other = Cred::new(2000, 200);

    // Linux runs `ipcperms()` before the `ipc_valid_object()` race check, so
    // an unprivileged caller sees EACCES even though the queue is removed.
    for (flag, allowed_mode) in [(IPC_READ, 0o444), (IPC_WRITE, 0o222)] {
        assert_eq!(
            check_queue_access(&queue, &other, flag),
            Err(KError::from(LinuxError::EACCES))
        );

        queue.msqid_ds.msg_perm.mode = allowed_mode;
        assert_eq!(
            check_queue_access(&queue, &other, flag),
            Err(KError::from(LinuxError::EIDRM))
        );
        queue.msqid_ds.msg_perm.mode = 0o600;
    }
}

#[def_test]
fn msgsnd_copies_user_message_before_queue_lookup_or_removal() {
    let msqid = 0x7fff_0a16;
    {
        let mut queue = MessageQueue::new(1, 0o666, 1, 1000, 100);
        queue.mark_removed = true;
        MSG_MANAGER
            .lock()
            .insert_msqid_queues(msqid, Arc::new(Mutex::new(queue)));
    }

    // Linux `ksys_msgsnd()` reads and validates the user message before it
    // resolves the queue or observes `IPC_RMID`, so a null buffer is EFAULT
    // even when the queue is already removed. The type-header copy also
    // precedes the `MSGMAX` check.
    let removed = sys_msgsnd(msqid, UserConstPtr::<msgbuf>::from(0), 1, 0);
    let oversized = sys_msgsnd(msqid, UserConstPtr::<msgbuf>::from(0), MSGMAX + 1, 0);
    MSG_MANAGER.lock().remove_msqid(msqid);

    assert_eq!(removed, Err(KError::BadAddress));
    assert_eq!(oversized, Err(KError::BadAddress));
}

#[def_test(user)]
fn msgsnd_rejects_negative_queue_id_before_copying_the_payload() {
    // `do_msgsnd()` checks the negative ID together with the size, after
    // `ksys_msgsnd()` copies the type header and before `load_msg()`. Give the
    // call a readable header and a payload that crosses the mapped page, so a
    // payload-first implementation reports EFAULT instead of EINVAL.
    let message = TestUserBuffer::new(4096).expect("message buffer");
    message.write_u64(1).expect("message type");
    let msgp = UserConstPtr::<msgbuf>::from(message.as_user_ptr::<u8>() as usize);

    assert_eq!(
        sys_msgsnd(-1, msgp, 4096, 0),
        Err(KError::from(LinuxError::EINVAL))
    );
}

#[def_test]
fn ipc_permission_folds_raw_operation_flags_like_linux() {
    let cred = Cred::new(1000, 100);
    let owner_read_only = perm(1000, 100, 1000, 100, 0o400);

    assert!(has_ipc_permission(&owner_read_only, &cred, IPC_READ));
    assert!(!has_ipc_permission(&owner_read_only, &cred, IPC_WRITE));
    // msgget(0666) requires both read and write, not just one of the bits.
    assert!(!has_ipc_permission(&owner_read_only, &cred, 0o666));
    assert!(has_ipc_permission(
        &perm(1000, 100, 1000, 100, 0o600),
        &cred,
        0o666
    ));
}

#[def_test]
fn ipc_permission_uses_cap_ipc_owner_instead_of_uid_zero() {
    let denied = perm(1000, 100, 2000, 200, 0o600);

    // UID zero without CAP_IPC_OWNER has no IPC override.
    let mut uid_zero = Cred::root();
    uid_zero
        .set_capabilities(CapSets::new(CapSet::EMPTY, CapSet::FULL, CapSet::EMPTY))
        .unwrap();
    assert!(!has_ipc_permission(&denied, &uid_zero, IPC_READ));

    let mut ipc_owner = unprivileged_with_caps();
    ipc_owner
        .set_capabilities(CapSets::new(
            CapSet::from_bits(1 << kcred::CAP_IPC_OWNER),
            CapSet::FULL,
            CapSet::EMPTY,
        ))
        .unwrap();
    assert!(has_ipc_permission(&denied, &ipc_owner, IPC_WRITE));
}

#[def_test]
fn ipc_owner_capability_is_scoped_to_the_queue_namespace() {
    let child = UserNamespace::new_child(&initial_user_namespace(), 1000, 1000, false).unwrap();
    let queue = MessageQueue::new_in(1, 0o600, 1, 1000, 1000, child.clone());
    let child_root = Cred::new(0, 0).with_user_namespace(child);
    assert!(has_ipc_permission_in(
        &queue.msqid_ds.msg_perm,
        &child_root,
        IPC_READ,
        queue.owner_ns()
    ));

    // An unprivileged initial-namespace caller has no CAP_IPC_OWNER over the
    // child-owned queue, while initial root can administer it through the
    // ancestor chain.
    assert!(!has_ipc_permission_in(
        &queue.msqid_ds.msg_perm,
        &Cred::new(2000, 2000),
        IPC_READ,
        queue.owner_ns()
    ));
    assert!(has_ipc_permission_in(
        &queue.msqid_ds.msg_perm,
        &Cred::root(),
        IPC_READ,
        queue.owner_ns()
    ));
}

fn unprivileged_with_caps() -> Cred {
    let mut cred = Cred::root();
    cred.set_securebits(0x4).unwrap();
    cred.set_resuid(Some(3000), Some(3000), Some(3000)).unwrap();
    cred.set_resgid(Some(300), Some(300), Some(300)).unwrap();
    cred
}

#[def_test]
fn ipc_control_uses_owner_creator_or_cap_sys_admin() {
    let queue = perm(1000, 100, 2000, 200, 0o600);

    assert!(has_ipc_control_permission(&queue, &Cred::new(1000, 100)));
    assert!(has_ipc_control_permission(&queue, &Cred::new(2000, 200)));
    assert!(!has_ipc_control_permission(&queue, &Cred::new(3000, 300)));

    let mut sys_admin = unprivileged_with_caps();
    sys_admin
        .set_capabilities(CapSets::new(
            CapSet::from_bits(1 << CAP_SYS_ADMIN),
            CapSet::FULL,
            CapSet::EMPTY,
        ))
        .unwrap();
    assert!(has_ipc_control_permission(&queue, &sys_admin));

    let mut ipc_owner = unprivileged_with_caps();
    ipc_owner
        .set_capabilities(CapSets::new(
            CapSet::from_bits(1 << kcred::CAP_IPC_OWNER),
            CapSet::FULL,
            CapSet::EMPTY,
        ))
        .unwrap();
    assert!(!has_ipc_control_permission(&queue, &ipc_owner));
}
