// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};
use intrusive_collections::{intrusive_adapter, KeyAdapter};
use unittest::def_test;

use crate::fair_tree::{DeadlineKey, FairLink, FairTree, FairTreeAdapter};

struct TestTask {
    link: FairLink,
    key: DeadlineKey,
    vruntime: i64,
}

intrusive_adapter!(TestAdapter = Arc<TestTask>: TestTask { link => FairLink });

impl<'a> KeyAdapter<'a> for TestAdapter {
    type Key = DeadlineKey;
    fn get_key(&self, task: &'a TestTask) -> DeadlineKey { task.key }
}

// SAFETY: vruntime is an immutable field read without callbacks or panics.
// Tests never modify a linked key or vruntime.
unsafe impl FairTreeAdapter for TestAdapter {
    fn vruntime(&self, task: &TestTask) -> i64 { task.vruntime }
}

#[def_test]
fn augmented_rb_tree_preserves_selection_ownership_and_nonempty_drop() {
    let tasks: Vec<_> = (0..127).map(|id| Arc::new(TestTask {
        link: FairLink::new(), key: (id % 11, id as u64), vruntime: id % 7 - 3,
    })).collect();
    let mut tree = FairTree::new(TestAdapter::new());
    for task in &tasks { tree.insert(task.clone()); }
    assert!(tree.valid_for_test());
    for id in (0..tasks.len()).step_by(2) {
        let removed = tree.remove(tasks[id].key).unwrap();
        assert!(Arc::ptr_eq(&removed, &tasks[id]));
        assert!(tree.valid_for_test());
        drop(removed);
        assert_eq!(Arc::strong_count(&tasks[id]), 1);
    }
    for avg in -4..=4 {
        let expected = (1..tasks.len()).step_by(2).filter(|id| tasks[*id].vruntime <= avg)
            .map(|id| tasks[id].key).min();
        let (chosen, visits) = tree.search_for_test(avg);
        assert_eq!(chosen, expected);
        assert!(visits <= tree.height_for_test());
    }
    drop(tree);
    assert!(tasks.iter().all(|task| !task.link.is_linked() && Arc::strong_count(task) == 1));
}
