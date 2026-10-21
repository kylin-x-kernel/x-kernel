// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::assert_eq;

use super::*;

// 辅助函数：构建一个测试树
// 树结构：
//       1 (root)
//      /  \
//     2    3
//    / \    /
//   4   5  6
fn build_test_tree() -> TeeFsHtree {
    let mut root_node = HtreeNode::new(1, TeeFsHtreeNodeImage::default()); // ID 1 (level 1)
    let mut node2 = HtreeNode::new(2, TeeFsHtreeNodeImage::default()); // ID 2 (level 2) - Left of 1
    let mut node3 = HtreeNode::new(3, TeeFsHtreeNodeImage::default()); // ID 3 (level 2) - Right of 1
    let node4 = HtreeNode::new(4, TeeFsHtreeNodeImage::default()); // ID 4 (level 3) - Left of 2
    let node5 = HtreeNode::new(5, TeeFsHtreeNodeImage::default()); // ID 5 (level 3) - Right of 2
    let node6 = HtreeNode::new(6, TeeFsHtreeNodeImage::default()); // ID 6 (level 3) - Left of 3

    HtreeNode::set_left(&mut node3, node6); // 节点 3 的右子树不存在
    HtreeNode::set_left(&mut node2, node4);
    HtreeNode::set_right(&mut node2, node5);
    HtreeNode::set_left(&mut root_node, node2);
    HtreeNode::set_right(&mut root_node, node3);

    TeeFsHtree {
        root: root_node,
        data: TeeFsHtreeData::default(),
        storage: Box::new(TeeFsFdAux::new()),
    }
}

#[unittest::def_test]
pub fn test_find_closest_node_exact_match() {
    let mut ht = build_test_tree();

    // 查找存在的节点：根节点
    let found = find_closest_node(&mut ht, 1);
    assert_eq!(found.id, 1, "应找到根节点 ID 1");
    let found = find_node(&mut ht, 1).unwrap();
    assert_eq!(found.id, 1, "应找到根节点 ID 1");
    let found = find_node(&mut ht, 100).is_some();
    assert_eq!(found, false, "应找到根节点 ID 1");

    // 查找存在的节点：左子节点
    let found = find_closest_node(&mut ht, 2);
    assert_eq!(found.id, 2, "应找到节点 ID 2");

    // 查找存在的节点：右子节点
    let found = find_closest_node(&mut ht, 3);
    assert_eq!(found.id, 3, "应找到节点 ID 3");

    // 查找存在的叶子节点
    let found = find_closest_node(&mut ht, 4);
    assert_eq!(found.id, 4, "应找到叶子节点 ID 4");

    let found = find_closest_node(&mut ht, 5);
    assert_eq!(found.id, 5, "应找到叶子节点 ID 5");

    let found = find_closest_node(&mut ht, 6);
    assert_eq!(found.id, 6, "应找到叶子节点 ID 6");
}

#[unittest::def_test]
pub fn test_find_closest_node_non_existent_path_left() {
    let mut ht = build_test_tree();

    // 查找一个不存在但其父节点存在的节点：
    // 目标 ID 7 (111) -> 路径: 1(root) -> 3(right) -> 7(right of 3)
    // 节点 3 (ID 3) 有左子节点 (ID 6)，但没有右子节点。
    // find_closest_node 应该找到节点 3。
    let found = find_closest_node(&mut ht, 7);
    assert_eq!(found.id, 3, "目标 ID 7 不存在，应返回最近的父节点 ID 3");

    let found = find_closest_node(&mut ht, 9);
    assert_eq!(found.id, 4, "目标 ID 9 不存在，应返回最近的父节点 ID 4");

    let found = find_closest_node(&mut ht, 16);
    assert_eq!(found.id, 4, "目标 ID 16 不存在，应返回最近的父节点 ID 4");
}

#[unittest::def_test]
pub fn test_find_closest_node_non_existent_path_deep() {
    let mut ht = build_test_tree();

    // 查找一个更深层级，且路径完全不存在的节点：
    // 目标 ID 10 (1010) -> 路径: 1(root) -> 2(left) -> 4(left) -> 8(left of 4) -> 16(left of 8) -> etc.
    // 我们的树在 ID 4 之后没有子节点。
    // 步骤：
    // 1. `node_id_to_level(10)` 是 4。
    // 2. 第一步：`bit_idx = 4 - (0+1) - 1 = 2`。`(10 >> 2) & 1 = (2)&1 = 0` -> 走左子树，到 `node2` (ID 2)。
    // 3. 第二步：`bit_idx = 4 - (1+1) - 1 = 1`。`(10 >> 1) & 1 = (5)&1 = 1` -> 走右子树，到 `node5` (ID 5)。
    // 4. 第三步：`bit_idx = 4 - (2+1) - 1 = 0`。`(10 >> 0) & 1 = (10)&1 = 0` -> 走左子树。`node5` 没有左子节点。
    // 所以，应该返回 `node5` (ID 5)。
    let found = find_closest_node(&mut ht, 10);
    assert_eq!(
        found.id, 5,
        "目标 ID 10 不存在，应返回路径上最近的节点 ID 5"
    );

    // 查找 ID 15 (1111)
    // 1. `node_id_to_level(15)` 是 4。
    // 2. `bit_idx = 2`. `(15 >> 2) & 1 = (3)&1 = 1` -> 走右子树，到 `node3` (ID 3)。
    // 3. `bit_idx = 1`. `(15 >> 1) & 1 = (7)&1 = 1` -> 走右子树。`node3` 没有右子节点。
    // 所以，应该返回 `node3` (ID 3)。
    let found = find_closest_node(&mut ht, 15);
    assert_eq!(
        found.id, 3,
        "目标 ID 15 不存在，应返回路径上最近的节点 ID 3"
    );
}

#[unittest::def_test]
#[should_panic]
pub fn test_find_closest_node_target_is_zero() {
    let mut ht = build_test_tree();
    // 如果目标 ID 是 0，根据逻辑应返回根节点。
    let found = find_closest_node(&mut ht, 1);
    assert_eq!(found.id, 1, "目标 ID 0 应返回根节点 ID 1");
}

#[unittest::def_test]
pub fn test_find_closest_node_empty_tree_or_only_root() {
    // 测试只有根节点的树
    let root_node = HtreeNode::new(1, TeeFsHtreeNodeImage::default());
    let mut ht = TeeFsHtree {
        root: root_node,
        data: TeeFsHtreeData::default(),
        storage: Box::new(TeeFsFdAux::new()),
    };

    // 查找根节点
    let found = find_closest_node(&mut ht, 1);
    assert_eq!(found.id, 1, "只有根节点的树，查找根节点");

    // 查找不存在的子节点 (例如 ID 2，左子节点)
    let found = find_closest_node(&mut ht, 2);
    assert_eq!(
        found.id, 1,
        "只有根节点的树，查找不存在的子节点应返回根节点"
    );

    // 查找不存在的子节点 (例如 ID 3，右子节点)
    let found = find_closest_node(&mut ht, 3);
    assert_eq!(
        found.id, 1,
        "只有根节点的树，查找不存在的子节点应返回根节点"
    );
}
