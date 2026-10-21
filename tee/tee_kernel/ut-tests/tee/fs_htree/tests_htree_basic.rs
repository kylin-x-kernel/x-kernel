// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

//-------- test suites --------
#[unittest::def_test]
pub fn test_iv_offset_matches_hash_size() {
    let iv_offset = offset_of!(TeeFsHtreeNodeImage, iv);
    assert_eq!(iv_offset, TEE_FS_HTREE_HASH_SIZE);
}

#[unittest::def_test]
pub fn test_calc_node_hash() {
    let node = HtreeNode::default();
    let meta = TeeFsHtreeMeta::default();
    let mut digest = [0u8; TEE_FS_HTREE_HASH_SIZE];

    let res = calc_node_hash_with_type(TEE_ALG_SHA256, &node, Some(&meta), &mut digest);
    assert!(res.is_ok());

    // the same asn sha256([0u8, 42]);
    let hex_string = hex::encode(digest);
    assert_eq!(
        hex_string,
        "094c4931fdb2f2af417c9e0322a9716006e8211fe9017f671ac6e3251300acca"
    );
    debug!("digest = {:02x?}", digest);
}

#[unittest::def_test]
pub fn test_construct_tree_with_children() {
    let root_node_image = TeeFsHtreeNodeImage::default();
    let left_node_image = TeeFsHtreeNodeImage::default();
    let right_node_image = TeeFsHtreeNodeImage::default();

    let mut root = HtreeNode::new(0, root_node_image);
    let left = HtreeNode::new(1, left_node_image);
    let right = HtreeNode::new(2, right_node_image);

    let meta = TeeFsHtreeMeta::default();

    HtreeNode::set_left(&mut root, left);
    HtreeNode::set_right(&mut root, right);

    let mut digest = [0u8; TEE_FS_HTREE_HASH_SIZE];

    let res = calc_node_hash_with_type(TEE_ALG_SHA256, &root, Some(&meta), &mut digest);
    assert!(res.is_ok());

    // the same asn sha256([0u8, 42+32+32]);
    let hex_string = hex::encode(digest);
    assert_eq!(
        hex_string,
        "34dbd6bf55d0d075d666181d9278b8387482a8b5804e44e1ddaafe6876dadc15"
    );
    debug!("digest = {:02x?}", digest);
}

#[unittest::def_test]
pub fn test_field_offsets_and_relevant_slice() {
    let node = TeeFsHtreeNodeImage {
        hash: [0xAA; TEE_FS_HTREE_HASH_SIZE],
        iv: [0xBB; TEE_FS_HTREE_IV_SIZE],
        tag: [0xCC; TEE_FS_HTREE_TAG_SIZE],
        flags: 0x1234,
    };

    // let all_bytes = bytemuck::bytes_of(&node);
    let all_bytes: &[u8] = node.as_bytes();
    let iv_offset = offset_of!(TeeFsHtreeNodeImage, iv);
    let flags_offset = offset_of!(TeeFsHtreeNodeImage, flags);
    let flags_size = core::mem::size_of::<u16>();

    let relevant = &all_bytes[iv_offset..flags_offset + flags_size];

    assert_eq!(iv_offset, TEE_FS_HTREE_HASH_SIZE);
    assert_eq!(
        flags_offset,
        TEE_FS_HTREE_HASH_SIZE + TEE_FS_HTREE_IV_SIZE + TEE_FS_HTREE_TAG_SIZE
    );
    assert_eq!(flags_size, 2);
    assert_eq!(
        relevant.len(),
        TEE_FS_HTREE_IV_SIZE + TEE_FS_HTREE_TAG_SIZE + 2
    );

    assert_eq!(&relevant[..16], &[0xBB; 16]); // iv
    assert_eq!(&relevant[16..32], &[0xCC; 16]); // tag
    assert_eq!(&relevant[32..34], &0x1234u16.to_le_bytes()); // flags（小端）
}

#[unittest::def_test]
pub fn test_htree_structure_and_access() {
    // 创建根节点
    let mut root = HtreeNode::new(
        0,
        TeeFsHtreeNodeImage {
            hash: [0x00; TEE_FS_HTREE_HASH_SIZE],
            iv: [0x01; TEE_FS_HTREE_IV_SIZE],
            tag: [0x02; TEE_FS_HTREE_TAG_SIZE],
            flags: 0,
        },
    );

    // 创建左子节点
    let left = HtreeNode::new(
        1,
        TeeFsHtreeNodeImage {
            hash: [0x11; TEE_FS_HTREE_HASH_SIZE],
            iv: [0x12; TEE_FS_HTREE_IV_SIZE],
            tag: [0x13; TEE_FS_HTREE_TAG_SIZE],
            flags: 1,
        },
    );

    // 创建右子节点
    let right = HtreeNode::new(
        2,
        TeeFsHtreeNodeImage {
            hash: [0x21; TEE_FS_HTREE_HASH_SIZE],
            iv: [0x22; TEE_FS_HTREE_IV_SIZE],
            tag: [0x23; TEE_FS_HTREE_TAG_SIZE],
            flags: 2,
        },
    );

    HtreeNode::set_left(&mut root, left);
    HtreeNode::set_right(&mut root, right);

    {
        assert!(root.left.is_some());
        assert!(root.right.is_some());

        let left_ref = root.left.as_ref().unwrap();
        assert_eq!(left_ref.id, 1);

        let right_ref = root.right.as_ref().unwrap();
        assert_eq!(right_ref.id, 2);
    }

    {
        let left_ref = root.left.as_ref().unwrap();
        assert_eq!(left_ref.parent_id, Some(root.id));

        let right_ref = root.right.as_ref().unwrap();
        assert_eq!(right_ref.parent_id, Some(root.id));
    }

    {
        let left_mut = root.left.get_mut().unwrap();
        left_mut.dirty = true;
        assert!(left_mut.dirty);
    }
}

#[unittest::def_test]
pub fn test_init_root_node_sets_fields_and_hash() {
    let mut ht = TeeFsHtree {
        root: HtreeNode::new(0, TeeFsHtreeNodeImage::default()),
        data: TeeFsHtreeData::default(),
        storage: Box::new(TeeFsFdAux::new()),
    };
    let result = init_root_node(&mut ht);

    assert!(result.is_ok());
    assert_eq!(ht.root.id, 1);
    assert!(ht.root.dirty);
    let hex_string = hex::encode(ht.root.node.hash);
    assert_eq!(
        hex_string,
        "094c4931fdb2f2af417c9e0322a9716006e8211fe9017f671ac6e3251300acca"
    );
}

#[unittest::def_test]
pub fn test_calc_and_verify_tree() {
    // 1. Create a sample tree
    let node_image_root = TeeFsHtreeNodeImage::default();
    let node_image_l = TeeFsHtreeNodeImage::default();
    let node_image_r = TeeFsHtreeNodeImage::default();
    let node_image_ll = TeeFsHtreeNodeImage::default();
    let node_image_lr = TeeFsHtreeNodeImage::default();

    let mut root = HtreeNode::new(1, node_image_root);
    let mut left_child = HtreeNode::new(2, node_image_l);
    let right_child = HtreeNode::new(3, node_image_r);
    let left_left_child = HtreeNode::new(4, node_image_ll);
    let left_right_child = HtreeNode::new(5, node_image_lr);

    HtreeNode::set_left(&mut left_child, left_left_child);
    HtreeNode::set_right(&mut left_child, left_right_child);
    HtreeNode::set_left(&mut root, left_child);
    HtreeNode::set_right(&mut root, right_child);
    // Create the TeeFsHtree structure
    let mut ht = TeeFsHtree {
        root,
        data: TeeFsHtreeData::default(),
        storage: Box::new(TeeFsFdAux::new()),
    };
    debug!("Verify tree completed.");
    let calc_result = calc_tree(&mut ht);
    assert!(
        calc_result.is_ok(),
        "Calc tree failed: {:?}",
        calc_result.unwrap_err()
    );

    // 3. Verify tree hashes
    let verify_result = verify_tree(&ht);
    assert!(
        verify_result.is_ok(),
        "Verify tree failed: {:?}",
        verify_result.unwrap_err()
    );

    let _print_result = print_tree_hash(&ht);
    assert!(
        verify_result.is_ok(),
        "Verify tree failed: {:?}",
        verify_result.unwrap_err()
    );
}

#[unittest::def_test]
pub fn test_verify_node_after_calc() {
    let mut ht = TeeFsHtree::default();
    ht.data.imeta.meta.length = 100;

    let mut node = HtreeNode::new(1, TeeFsHtreeNodeImage::default());

    // 先计算节点哈希
    let storage = ht.storage.as_ref();
    let calc_result = calc_node(&mut node, &ht.data, storage);
    assert!(calc_result.is_ok());

    // 再验证节点
    let verify_result = verify_node(&node, &ht.data);
    assert!(verify_result.is_ok());
}

#[unittest::def_test]
pub fn test_verify_node_without_calc() {
    let ht = TeeFsHtree::default();
    let node = HtreeNode::new(1, TeeFsHtreeNodeImage::default());

    // 不计算直接验证（应该失败）
    assert!(verify_node(&node, &ht.data).is_err());
}

#[unittest::def_test]
pub fn test_verify_node_with_parent_after_calc() {
    let ht = TeeFsHtree::default();
    let mut child = HtreeNode::new(2, TeeFsHtreeNodeImage::default());
    child.parent_id = Some(1);

    // 先计算子节点哈希
    let storage = ht.storage.as_ref();
    let calc_result = calc_node(&mut child, &ht.data, storage);
    assert!(calc_result.is_ok());

    // 再验证子节点
    let verify_result = verify_node(&child, &ht.data);
    assert!(verify_result.is_ok());
}

#[unittest::def_test]
pub fn test_update_verify_root() {
    let mut ht = TeeFsHtree::default();
    ht.data.uuid = TEE_UUID {
        timeLow: 1,
        timeMid: 2,
        timeHiAndVersion: 3,
        clockSeqAndNode: [4; 8],
    };
    ht.data.imeta.meta.length = 100;
    ht.data.imeta.max_node_id = 5;

    // 生成 fek
    tee_fs_fek_crypt(
        Some(&ht.data.uuid),
        TEE_OperationMode::TEE_MODE_ENCRYPT,
        Some(&ht.data.fek),
        TEE_FS_KM_FEK_SIZE,
        Some(&mut ht.data.head.enc_fek),
    )
    .unwrap();

    // 更新根节点
    update_root(&mut ht).unwrap();
    assert_eq!(ht.data.head.counter, 1);

    // 验证根节点
    verify_root(&mut ht).unwrap();

    // 验证解密后的数据
    assert_eq!(ht.data.imeta.meta.length, 100);
    assert_eq!(ht.data.imeta.max_node_id, 5);
}

#[unittest::def_test]
pub fn test_update_root_multiple_times() {
    let mut ht = TeeFsHtree::default();
    ht.data.uuid = TEE_UUID {
        timeLow: 1,
        timeMid: 2,
        timeHiAndVersion: 3,
        clockSeqAndNode: [4; 8],
    };
    ht.data.imeta.meta.length = 200;
    ht.data.imeta.max_node_id = 10;

    tee_fs_fek_crypt(
        Some(&ht.data.uuid),
        TEE_OperationMode::TEE_MODE_ENCRYPT,
        Some(&ht.data.fek),
        TEE_FS_KM_FEK_SIZE,
        Some(&mut ht.data.head.enc_fek),
    )
    .unwrap();

    // 多次更新
    update_root(&mut ht).unwrap();
    assert_eq!(ht.data.head.counter, 1);

    update_root(&mut ht).unwrap();
    assert_eq!(ht.data.head.counter, 2);

    update_root(&mut ht).unwrap();
    assert_eq!(ht.data.head.counter, 3);

    verify_root(&mut ht).unwrap();
}

#[unittest::def_test]
pub fn test_verify_root_without_update() {
    let mut ht = TeeFsHtree::default();
    ht.data.uuid = TEE_UUID {
        timeLow: 1,
        timeMid: 2,
        timeHiAndVersion: 3,
        clockSeqAndNode: [4; 8],
    };
    ht.data.head.imeta = [0x00; TEE_FS_HTREE_IMETA_SIZE]; // 未加密数据

    assert!(verify_root(&mut ht).is_err());
}
