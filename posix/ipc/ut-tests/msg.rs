// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::def_test;

use super::*;

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
