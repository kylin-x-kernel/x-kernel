// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//-------- test framework import --------
//-------- local tests import --------
use alloc::{collections::VecDeque, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{TestResult, assert, assert_eq};

use super::*;

#[derive(Debug, Default)]
struct StorageCalls {
    read_final: AtomicUsize,
    write_init: AtomicUsize,
    write_final: AtomicUsize,
    drops: AtomicUsize,
}

#[derive(Debug)]
struct RejectWriteStorage(Arc<StorageCalls>);

impl Drop for RejectWriteStorage {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::Relaxed);
    }
}

impl TeeFsHtreeStorageOps for RejectWriteStorage {
    fn block_size(&self) -> usize {
        4096
    }

    fn rpc_read_init(&self) -> TeeResult {
        Ok(())
    }

    fn rpc_read_final(
        &self,
        _typ: TeeFsHtreeType,
        _idx: usize,
        _vers: u8,
        data: &mut [u8],
    ) -> TeeResult<usize> {
        self.0.read_final.fetch_add(1, Ordering::Relaxed);
        data.fill(0);
        Ok(data.len())
    }

    fn rpc_write_init(&self) -> TeeResult {
        self.0.write_init.fetch_add(1, Ordering::Relaxed);
        Err(TEE_ERROR_GENERIC)
    }

    fn rpc_write_final(
        &self,
        _typ: TeeFsHtreeType,
        _idx: usize,
        _vers: u8,
        _data: &[u8],
    ) -> TeeResult<usize> {
        self.0.write_final.fetch_add(1, Ordering::Relaxed);
        Err(TEE_ERROR_GENERIC)
    }
}

fn tree_with_storage(calls: Arc<StorageCalls>) -> TeeResult<TeeFsHtree> {
    let storage = KBox::new_with_flags(RejectWriteStorage(calls), AllocFlags::NOWAIT)
        .map_err(|error| u32::from(TeeError::from(error)))?;
    let mut tree = TeeFsHtree::new(storage);
    tree.root.id = 1;
    tree.data.imeta.max_node_id = 1;
    Ok(tree)
}

#[unittest::def_test]
fn growth_failure_at_each_owner_preserves_tree_and_retry() {
    for fail_at in 0..6 {
        let mut tree = tree_with_storage(Arc::default()).unwrap();
        get_node(&mut tree, true, 3).unwrap();
        tree.root.dirty = true;
        tree.root.node.flags = 7;
        tree.data.dirty = true;
        let child_address = core::ptr::from_ref(find_node(&mut tree, 2).unwrap());
        let before = alloc::format!("{tree:?}");
        let mut attempts = 0;
        let mut permissions = true;
        let result = HtreeGrowth::prepare_with(&mut tree, true, 9, |node, flags| {
            permissions &= flags == AllocFlags::NOWAIT;
            let index = attempts;
            attempts += 1;
            if index == fail_at {
                Err(TEE_ERROR_OUT_OF_MEMORY)
            } else {
                reserve_htree_node(node, flags)
            }
        });
        assert!(matches!(result, Err(TEE_ERROR_OUT_OF_MEMORY)));
        drop(result);
        assert_eq!(attempts, fail_at + 1);
        assert!(permissions);
        assert_eq!(alloc::format!("{tree:?}"), before);
        assert_eq!(
            core::ptr::from_ref(find_node(&mut tree, 2).unwrap()),
            child_address
        );
        assert_eq!(get_node(&mut tree, true, 9).unwrap().id, 9);
        assert_eq!(tree.data.imeta.max_node_id, 9);
        for node_id in 2..=9 {
            assert_eq!(
                find_node(&mut tree, node_id).unwrap().parent_id,
                Some(node_id >> 1)
            );
        }
    }
}

#[unittest::def_test]
fn existing_and_no_create_lookups_skip_reservation() {
    let mut tree = tree_with_storage(Arc::default()).unwrap();
    let mut attempts = 0;
    let growth = HtreeGrowth::prepare_with(&mut tree, true, 1, |_, _| {
        attempts += 1;
        Err(TEE_ERROR_OUT_OF_MEMORY)
    })
    .unwrap();
    assert_eq!(growth.commit().id, 1);
    let result = HtreeGrowth::prepare_with(&mut tree, false, 2, |_, _| {
        attempts += 1;
        Err(TEE_ERROR_OUT_OF_MEMORY)
    });
    assert!(matches!(result, Err(TEE_ERROR_GENERIC)));
    drop(result);
    assert_eq!(attempts, 0);
    assert_eq!(tree.data.imeta.max_node_id, 1);
}

#[unittest::def_test]
fn prepared_growth_can_be_cancelled_then_truncated_and_regrown() {
    let mut tree = tree_with_storage(Arc::default()).unwrap();
    let before = alloc::format!("{tree:?}");
    drop(HtreeGrowth::prepare_with(&mut tree, true, 2048, reserve_htree_node).unwrap());
    assert_eq!(alloc::format!("{tree:?}"), before);
    get_node(&mut tree, true, 15).unwrap();
    tee_fs_htree_truncate(&mut tree, 3).unwrap();
    assert_eq!(tree.data.imeta.max_node_id, 4);
    for node_id in 5..=15 {
        assert!(find_node(&mut tree, node_id).is_none());
    }
    get_node(&mut tree, true, 15).unwrap();
    assert_eq!(tree.data.imeta.max_node_id, 15);
}

#[unittest::def_test]
fn write_owner_failure_precedes_rpc_and_preserves_error() {
    let calls = Arc::new(StorageCalls::default());
    let mut tree = tree_with_storage(calls.clone()).unwrap();
    let before = alloc::format!("{tree:?}");
    let result =
        write_block_with_reservation(&mut tree, 4, &[0; 16], |_, _| Err(TEE_ERROR_OUT_OF_MEMORY));
    assert_eq!(result, Err(TEE_ERROR_OUT_OF_MEMORY));
    assert_eq!(calls.write_init.load(Ordering::Relaxed), 0);
    assert_eq!(calls.write_final.load(Ordering::Relaxed), 0);
    assert_eq!(alloc::format!("{tree:?}"), before);
}

#[unittest::def_test]
fn write_init_failure_discards_prepared_nodes() {
    let calls = Arc::new(StorageCalls::default());
    let mut tree = tree_with_storage(calls.clone()).unwrap();
    let before = alloc::format!("{tree:?}");
    assert_eq!(
        tee_fs_htree_write_block(&mut tree, 4, &[0; 16]),
        Err(TEE_ERROR_GENERIC)
    );
    assert_eq!(calls.write_init.load(Ordering::Relaxed), 1);
    assert_eq!(calls.write_final.load(Ordering::Relaxed), 0);
    assert_eq!(alloc::format!("{tree:?}"), before);
    assert_eq!(get_node(&mut tree, true, 5).unwrap().id, 5);
}

#[unittest::def_test]
fn invalid_node_ids_fail_before_reservation() {
    let mut tree = tree_with_storage(Arc::default()).unwrap();
    for node_id in [0, usize::MAX] {
        let mut attempts = 0;
        let result = HtreeGrowth::prepare_with(&mut tree, true, node_id, |_, _| {
            attempts += 1;
            Err(TEE_ERROR_OUT_OF_MEMORY)
        });
        assert!(matches!(result, Err(TEE_ERROR_BAD_PARAMETERS)));
        drop(result);
        assert_eq!(attempts, 0);
    }
    assert_eq!(
        block_num_to_node_id(usize::MAX),
        Err(TEE_ERROR_BAD_PARAMETERS)
    );
    assert_eq!(
        block_num_to_node_id(u32::MAX as usize),
        Err(TEE_ERROR_BAD_PARAMETERS)
    );
    assert_eq!(validate_node_id((u32::MAX - 1) as usize), Ok(u32::MAX - 1));
    assert_eq!(validate_node_id(1), Ok(1));
}

#[unittest::def_test]
fn commit_preserves_prepared_addresses_and_existing_sparse_nodes() {
    let mut tree = tree_with_storage(Arc::default()).unwrap();
    let mut child = reserve_htree_node(
        HtreeNode::new(2, TeeFsHtreeNodeImage::default()),
        AllocFlags::NOWAIT,
    )
    .unwrap();
    HtreeNode::set_left(
        &mut child,
        reserve_htree_node(
            HtreeNode::new(4, TeeFsHtreeNodeImage::default()),
            AllocFlags::NOWAIT,
        )
        .unwrap(),
    );
    HtreeNode::set_left(&mut tree.root, child);
    tree.data.imeta.max_node_id = 4;
    let existing = core::ptr::from_ref(find_node(&mut tree, 4).unwrap());
    let mut prepared = Vec::new();
    let growth = HtreeGrowth::prepare_with(&mut tree, true, 7, |node, flags| {
        let owner = reserve_htree_node(node, flags)?;
        prepared.push((owner.id, core::ptr::from_ref(&*owner)));
        Ok(owner)
    })
    .unwrap();
    growth.commit();
    assert_eq!(prepared.len(), 4);
    for (node_id, address) in prepared {
        let node = find_node(&mut tree, node_id).unwrap();
        assert_eq!(core::ptr::from_ref(node), address);
        assert_eq!(node.parent_id, Some(node_id >> 1));
    }
    assert_eq!(
        core::ptr::from_ref(find_node(&mut tree, 4).unwrap()),
        existing
    );
    assert_eq!(tree.data.imeta.max_node_id, 7);
    assert!(!tree.data.dirty);
}

#[unittest::def_test]
fn load_owner_failure_preserves_error_and_unpublished_tree_is_released() {
    for fail_at in 0..3 {
        let calls = Arc::new(StorageCalls::default());
        let mut tree = tree_with_storage(calls.clone()).unwrap();
        tree.data.imeta.max_node_id = 4;
        let mut attempts = 0;
        let result = init_tree_with_reservation(&mut tree, |node, flags| {
            let index = attempts;
            attempts += 1;
            if index == fail_at {
                Err(TEE_ERROR_OUT_OF_MEMORY)
            } else {
                reserve_htree_node(node, flags)
            }
        });
        assert_eq!(result, Err(TEE_ERROR_OUT_OF_MEMORY));
        assert_eq!(attempts, fail_at + 1);
        assert_eq!(calls.read_final.load(Ordering::Relaxed), fail_at + 1);
        assert!(find_node(&mut tree, fail_at + 2).is_none());
        drop(tree);
        assert_eq!(calls.drops.load(Ordering::Relaxed), 1);
    }
}

// 辅助函数：构建一个只有根节点的树 (ID=1)
fn build_root_only_tree() -> TeeFsHtree {
    let root_node = HtreeNode::new(1, TeeFsHtreeNodeImage::default());
    TeeFsHtree {
        root: root_node,
        data: TeeFsHtreeData::default(),
        storage: super::tests_htree_basic::test_storage(),
    }
}

// 辅助函数：构建一个部分树结构
// 树结构:
//         1
//        / \
//       2   3
//      /
//     4
fn build_partial_tree() -> TeeFsHtree {
    let mut root_node = HtreeNode::new(1, TeeFsHtreeNodeImage::default());
    let mut node2 = HtreeNode::new(2, TeeFsHtreeNodeImage::default());
    let node3 = HtreeNode::new(3, TeeFsHtreeNodeImage::default());
    let node4 = HtreeNode::new(4, TeeFsHtreeNodeImage::default());

    HtreeNode::set_left(
        &mut node2,
        reserve_htree_node(node4, AllocFlags::NOWAIT).unwrap(),
    );
    HtreeNode::set_left(
        &mut root_node,
        reserve_htree_node(node2, AllocFlags::NOWAIT).unwrap(),
    );
    HtreeNode::set_right(
        &mut root_node,
        reserve_htree_node(node3, AllocFlags::NOWAIT).unwrap(),
    );

    let mut ht_data = TeeFsHtreeData::default();
    ht_data.imeta.max_node_id = 4; // 根据实际最大ID设置
    TeeFsHtree {
        root: root_node,
        data: ht_data,
        storage: super::tests_htree_basic::test_storage(),
    }
}

// 辅助函数：深度优先遍历，验证树结构和节点ID
// 注意：这个验证函数会假设树是按预期构建的。如果 get_node 的逻辑有缺陷，
// 这个验证可能会失败，或者更糟的是，误认为正确。
fn assert_tree_structure(
    ht: &TeeFsHtree,
    expected_ids: &[usize], // 期望按层序遍历的节点ID
    expected_max_node_id: u32,
) -> TestResult {
    let mut actual_ids = Vec::new();
    let mut q: VecDeque<&HtreeNode> = VecDeque::new();
    q.push_back(&ht.root);

    while let Some(node) = q.pop_front() {
        actual_ids.push(node.id);

        if let Some(left_child) = node.left.get_ref() {
            q.push_back(left_child);
        }
        if let Some(right_child) = node.right.get_ref() {
            q.push_back(right_child);
        }
    }
    assert_eq!(actual_ids, expected_ids);
    assert_eq!(ht.data.imeta.max_node_id, expected_max_node_id);

    TestResult::Ok
}

// 测试场景 1: 查找已存在的根节点 (create = false)
#[unittest::def_test]
pub fn test_get_node_root_exists_no_create() {
    let mut ht = build_root_only_tree();
    let initial_max_id = ht.data.imeta.max_node_id;

    let result = get_node(&mut ht, false, 1);
    assert!(result.is_ok());
    let node = result.unwrap();
    assert_eq!(node.id, 1);
    assert_eq!(ht.data.imeta.max_node_id, initial_max_id); // max_node_id 不应改变
    assert_tree_structure(&ht, &[1], initial_max_id); // 树结构不变
}

// 测试场景 2: 查找已存在的叶子节点 (create = false)
#[unittest::def_test]
pub fn test_get_node_leaf_exists_no_create() {
    let mut ht = build_partial_tree(); // 包含 1, 2, 3, 4
    let initial_max_id = ht.data.imeta.max_node_id;

    let result = get_node(&mut ht, false, 4);
    assert!(result.is_ok());
    let node = result.unwrap();
    assert_eq!(node.id, 4);
    assert_eq!(ht.data.imeta.max_node_id, initial_max_id);
    assert_tree_structure(&ht, &[1, 2, 3, 4], initial_max_id);
}

// 测试场景 3: 查找已存在的中间节点 (create = false)
#[unittest::def_test]
pub fn test_get_node_intermediate_exists_no_create() {
    let mut ht = build_partial_tree(); // 包含 1, 2, 3, 4
    let initial_max_id = ht.data.imeta.max_node_id;

    let node = get_node(&mut ht, false, 2).unwrap();
    assert_eq!(node.id, 2);
    assert_eq!(ht.data.imeta.max_node_id, initial_max_id);
    assert_tree_structure(&ht, &[1, 2, 3, 4], initial_max_id);
}
// // 测试场景 4: 查找不存在的节点 (create = false)
#[unittest::def_test]
pub fn test_get_node_not_exists_no_create() {
    let mut ht = build_root_only_tree(); // 只有根节点 1
    let initial_max_id = ht.data.imeta.max_node_id;

    let result = get_node(&mut ht, false, 5); // 5 不存在
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err, TEE_ERROR_GENERIC);
    assert_eq!(ht.data.imeta.max_node_id, initial_max_id); // max_node_id 不应改变
    assert_tree_structure(&ht, &[1], initial_max_id); // 树结构不变
}
// 测试场景 5: 查找已存在的节点 (create = true) - 不应有副作用
#[unittest::def_test]
pub fn test_get_node_exists_with_create_no_side_effects() {
    let mut ht = build_partial_tree(); // 包含 1, 2, 3, 4
    let initial_max_id = ht.data.imeta.max_node_id;

    let result = get_node(&mut ht, true, 3); // 3 已经存在
    assert!(result.is_ok());
    let node = result.unwrap();
    assert_eq!(node.id, 3);
    assert_eq!(ht.data.imeta.max_node_id, initial_max_id); // max_node_id 不应改变
    assert_tree_structure(&ht, &[1, 2, 3, 4], initial_max_id); // 树结构不变
}
// 测试场景 6: 创建一个需要补齐路径上的一个节点的场景 (左子节点)
// 初始: 1-2-4。 目标: 5 (2的右子节点)
// `get_node` 的 `for n in current_id..=node_id` 循环: `for n in 4..=5`
// 预期 `find_closest_node(ht, 5)` 返回 2。
// 循环会处理 `n=4` (已存在), `n=5` (不存在，创建)。
#[unittest::def_test]
pub fn test_get_node_create_single_missing_left_child() {
    let mut ht = build_partial_tree(); // 包含 1, 2, 3, 4
    // 确保 ID=5 (2的右子) 不存在
    assert!(ht.root.left.as_ref().unwrap().right.is_none());

    let result = get_node(&mut ht, true, 5); // 路径 1 -> 2 -> 5
    assert!(result.is_ok());

    let (node_id, parent_id) = {
        let node = result.unwrap();
        let id = node.id;
        let parent_id = node.parent_id;
        (id, parent_id)
    };
    assert_eq!(node_id, 5);

    // 验证 max_node_id 是否更新
    assert_eq!(ht.data.imeta.max_node_id, 5);

    // 验证树结构 (注意：根据您的 `get_node` 实现，这里可能需要调整期望的树结构)
    // 期望是: 1-2-3-4-5 (5是2的右子)
    assert_tree_structure(&ht, &[1, 2, 3, 4, 5], 5); // 期望 5 在树中

    // 额外验证连接
    let node2_from_tree = ht.root.left.get_ref().unwrap();
    assert!(node2_from_tree.right.is_some());
    assert_eq!(node2_from_tree.right.as_ref().unwrap().id, 5);

    assert_eq!(parent_id, Some(2));
}
// 测试场景 7: 创建一个需要补齐路径上的一个节点的场景 (右子节点)
// 初始: 1-2-3。 目标: 6 (3的左子节点)
// `get_node` 的 `for n in current_id..=node_id` 循环: `for n in 3..=6`
// 预期 `find_closest_node(ht, 6)` 返回 3。
// 循环会处理 `n=3` (已存在), `n=4,5` (不存在，但不在路径上), `n=6` (不存在，创建)。
#[unittest::def_test]
pub fn test_get_node_create_single_missing_right_child() {
    let mut ht = build_partial_tree(); // 包含 1, 2, 3, 4
    // 确保 ID=6 (3的左子) 不存在
    assert!(ht.root.right.get_ref().unwrap().left.is_none());

    let result = get_node(&mut ht, true, 6); // 路径 1 -> 3 -> 6
    assert!(result.is_ok());
    let (node_id, parent_id) = {
        let node = result.unwrap();
        let id = node.id;
        let parent_id = node.parent_id;
        (id, parent_id)
    };
    assert_eq!(node_id, 6);

    assert_eq!(ht.data.imeta.max_node_id, 6);

    // 注意：根据 `for n in current_id..=node_id` 的行为，
    // 可能会创建额外的节点。这个测试的预期行为将基于 `get_node` 的实际实现。
    // 对于这个版本，`n=4,5` 会被处理。
    // `find_closest_node(ht, 4)` 可能返回 2 (如果 4 存在) 或 1 (如果 4 不存在)
    // `find_closest_node(ht, 5)` 可能返回 2 (如果 5 不存在) 或 1 (如果 5 不存在)
    // 预期ID序列可能变长: 1, 2, 3, 4, [5], 6.
    // 如果 4 存在，那么 5 将被创建并连接到 2 的右侧。
    // 如果 4 不存在，则 4 将被创建并连接到 2 的左侧。
    // 鉴于 `build_partial_tree` 包含 4，所以 5 会被创建。
    // 期望的结构: 1, 2, 3, 4, 5 (2的右), 6 (3的左)
    assert_tree_structure(&ht, &[1, 2, 3, 4, 5, 6], 6);

    // 额外验证连接
    let node3_from_tree = ht.root.right.get_ref().unwrap();
    assert!(node3_from_tree.left.is_some());
    assert_eq!(node3_from_tree.left.as_ref().unwrap().id, 6);
    assert_eq!(parent_id, Some(3));
}
// 测试场景 8: 创建多个中间缺失节点 (所有在路径上，但非连续ID)
// 初始: 只有 1。 目标: 4 (路径: 1 -> 2 -> 4)
// `get_node` 的 `for n in current_id..=node_id` 循环: `for n in 1..=4`
// 预期会创建 2 和 4，但也会不必要地处理 3。
#[unittest::def_test]
pub fn test_get_node_create_multiple_missing_nodes_linear_ids() {
    let mut ht = build_root_only_tree(); // 只有根节点 1
    let result = get_node(&mut ht, true, 4); // 路径 1 -> 2 -> 4
    assert!(result.is_ok());
    let created_node_arc = result.unwrap();
    assert_eq!(created_node_arc.id, 4);

    assert_eq!(ht.data.imeta.max_node_id, 4);

    // 预期会创建 2, 3, 4。 3 是 1 的右子。
    // 结构: 1(root) -> 2(L), 3(R) -> 4(2L)
    assert_tree_structure(&ht, &[1, 2, 3, 4], 4);

    // 额外验证连接
    let root = ht.root;
    assert!(root.left.is_some());
    assert_eq!(root.left.as_ref().unwrap().id, 2);
    assert!(root.right.is_some()); // ID=3 应该被创建
    assert_eq!(root.right.as_ref().unwrap().id, 3);

    let node2_from_tree = root.left.get_ref().unwrap();
    assert!(node2_from_tree.left.is_some());
    assert_eq!(node2_from_tree.left.as_ref().unwrap().id, 4);
}
// 测试场景 9: 创建更深层次的节点，验证 max_node_id 更新
// 初始: 1,2,3,4。 目标: 8 (路径: 1 -> 2 -> 4 -> 8)
// `get_node` 的 `for n in current_id..=node_id` 循环: `for n in 4..=8`
// 预期会处理 4, 5, 6, 7, 8。
// 5, 6, 7 不在路径上。
#[unittest::def_test]
pub fn test_get_node_create_deeper_node_and_max_id_update() {
    let mut ht = build_partial_tree(); // 包含 1, 2, 3, 4
    let _initial_max_id = ht.data.imeta.max_node_id; // 初始为 4

    let result = get_node(&mut ht, true, 8); // 路径 1 -> 2 -> 4 -> 8
    assert!(result.is_ok());
    let created_node_arc = result.unwrap();
    assert_eq!(created_node_arc.id, 8);

    // max_node_id 应该更新到 8
    assert_eq!(ht.data.imeta.max_node_id, 8);

    // 验证树结构：这会非常复杂，因为 5, 6, 7 都会被不必要地创建和连接。
    // 预期 ID 序列：1, 2, 3, 4, 5(2的右), 6(3的左), 7(3的右), 8(4的左)
    assert_tree_structure(&ht, &[1, 2, 3, 4, 5, 6, 7, 8], 8);

    // 额外验证路径上的连接
    let node4_from_tree = ht.root.left.get_ref().unwrap().left.get_ref().unwrap();
    assert!(node4_from_tree.left.is_some());
    assert_eq!(node4_from_tree.left.get_ref().unwrap().id, 8);
}
