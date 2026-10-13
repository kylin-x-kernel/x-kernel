// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;
use unittest::def_test;

use super::IpiEventQueue;
use crate::event::Callback;

#[def_test]
fn test_queue_empty_pop() {
    let mut queue = IpiEventQueue::new();
    assert!(queue.pop_one().is_none());
}

#[def_test]
fn test_queue_fifo() {
    let mut queue = IpiEventQueue::new();
    queue.push(LogicalCpuId::new(1), Callback::new(|| {}));
    queue.push(LogicalCpuId::new(2), Callback::new(|| {}));
    let (src1, _) = queue.pop_one().unwrap();
    let (src2, _) = queue.pop_one().unwrap();
    assert_eq!(src1, LogicalCpuId::new(1));
    assert_eq!(src2, LogicalCpuId::new(2));
}

#[def_test]
fn test_queue_reuse() {
    let mut queue = IpiEventQueue::new();
    queue.push(LogicalCpuId::new(3), Callback::new(|| {}));
    assert!(queue.pop_one().is_some());
    queue.push(LogicalCpuId::new(4), Callback::new(|| {}));
    let (src, _) = queue.pop_one().unwrap();
    assert_eq!(src, LogicalCpuId::new(4));
}
