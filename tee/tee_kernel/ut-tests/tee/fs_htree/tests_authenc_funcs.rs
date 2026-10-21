// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::string::String;

use unittest::{assert, assert_eq, assert_ne};

use super::*;

#[unittest::def_test]
pub fn test_authenc_functions() {
    // 1. 创建测试用的 TeeFsHtree
    let mut ht = TeeFsHtree::default();

    // 初始化必要的字段
    ht.data.fek = [0x01; TEE_FS_HTREE_FEK_SIZE];
    ht.data.head.enc_fek = [0x02; TEE_FS_HTREE_FEK_SIZE];
    ht.data.head.counter = 12345;
    ht.data.head.iv = [0x03; TEE_FS_HTREE_IV_SIZE];

    // 设置根节点的哈希
    ht.root.node.hash = [0x04; TEE_FS_HTREE_HASH_SIZE];

    // 2. 创建测试数据
    let test_data = b"Hello, TEE World!.";
    let mut plaintext = [0u8; 64];
    plaintext[..test_data.len()].copy_from_slice(test_data);

    // 3. 测试加密流程
    // 创建节点镜像用于加密
    let mut node_image = TeeFsHtreeNodeImage {
        iv: [0x05; TEE_FS_HTREE_IV_SIZE],
        ..Default::default()
    };

    // 初始化加密上下文
    let encrypt_cipher = authenc_init(
        TEE_OperationMode::TEE_MODE_ENCRYPT,
        &mut ht,
        Some(&mut node_image),
        plaintext.len(),
        None,
    )
    .expect("Failed to initialize encryption cipher");

    // 执行加密
    let mut ciphertext = [0u8; 64];
    let mut tag = [0u8; TEE_FS_HTREE_TAG_SIZE];

    authenc_encrypt_final(encrypt_cipher, &mut tag, &plaintext, &mut ciphertext)
        .expect("Encryption failed");

    // 4. 测试解密流程
    // 初始化解密上下文
    let decrypt_cipher = authenc_init(
        TEE_OperationMode::TEE_MODE_DECRYPT,
        &mut ht,
        Some(&mut node_image),
        ciphertext.len(),
        None,
    )
    .expect("Failed to initialize decryption cipher");

    // 执行解密
    let mut decrypted = [0u8; 64];

    authenc_decrypt_final(decrypt_cipher, &tag, &ciphertext, &mut decrypted)
        .expect("Decryption failed");

    // 5. 验证结果
    assert_eq!(&plaintext[..test_data.len()], test_data);
    assert_eq!(&decrypted[..test_data.len()], test_data);

    // 验证加密和解密的数据不同（加密成功）
    assert_ne!(
        &plaintext[..test_data.len()],
        &ciphertext[..test_data.len()]
    );

    debug!("Encryption/Decryption test passed!");
    debug!(
        "Original: {:?}",
        String::from_utf8_lossy(&plaintext[..test_data.len()])
    );
    debug!(
        "Decrypted: {:?}",
        String::from_utf8_lossy(&decrypted[..test_data.len()])
    );
}

#[unittest::def_test]
pub fn test_authenc_roundtrip_no_node_image() {
    let mut ht = TeeFsHtree::default();
    ht.data.fek = [0x11; TEE_FS_HTREE_FEK_SIZE];
    ht.data.head.enc_fek = [0x22; TEE_FS_HTREE_FEK_SIZE];
    ht.data.head.counter = 1;
    ht.data.head.iv = [0x33; TEE_FS_HTREE_IV_SIZE];
    ht.root.node.hash = [0x44; TEE_FS_HTREE_HASH_SIZE];

    let msg = b"roundtrip-no-node-image";
    let mut plain = [0u8; 32];
    plain[..msg.len()].copy_from_slice(msg);

    let enc = authenc_init(
        TEE_OperationMode::TEE_MODE_ENCRYPT,
        &mut ht,
        None,
        msg.len(),
        None,
    )
    .unwrap();
    let mut crypt = [0u8; 32];
    let mut tag = [0u8; TEE_FS_HTREE_TAG_SIZE];
    authenc_encrypt_final(enc, &mut tag, &plain[..msg.len()], &mut crypt[..msg.len()]).unwrap();

    assert_ne!(&plain[..msg.len()], &crypt[..msg.len()]);

    let dec = authenc_init(
        TEE_OperationMode::TEE_MODE_DECRYPT,
        &mut ht,
        None,
        msg.len(),
        None,
    )
    .unwrap();
    let mut out = [0u8; 32];
    authenc_decrypt_final(dec, &tag, &crypt[..msg.len()], &mut out[..msg.len()]).unwrap();

    assert_eq!(&out[..msg.len()], msg);
}

#[unittest::def_test]
pub fn test_authenc_empty_data() {
    let mut ht = TeeFsHtree::default();
    ht.data.fek = [0x01; TEE_FS_HTREE_FEK_SIZE];
    ht.data.head.enc_fek = [0x02; TEE_FS_HTREE_FEK_SIZE];
    ht.data.head.counter = 1;
    ht.data.head.iv = [0x03; TEE_FS_HTREE_IV_SIZE];
    ht.root.node.hash = [0x04; TEE_FS_HTREE_HASH_SIZE];

    let mut node_image = TeeFsHtreeNodeImage {
        iv: [0x05; TEE_FS_HTREE_IV_SIZE],
        ..Default::default()
    };

    let enc = authenc_init(
        TEE_OperationMode::TEE_MODE_ENCRYPT,
        &mut ht,
        Some(&mut node_image),
        0,
        None,
    )
    .unwrap();
    let mut crypt = [0u8; 0];
    let mut tag = [0u8; TEE_FS_HTREE_TAG_SIZE];
    authenc_encrypt_final(enc, &mut tag, &[], &mut crypt).unwrap();

    let dec = authenc_init(
        TEE_OperationMode::TEE_MODE_DECRYPT,
        &mut ht,
        Some(&mut node_image),
        0,
        None,
    )
    .unwrap();
    let mut out = [0u8; 0];
    authenc_decrypt_final(dec, &tag, &[], &mut out).unwrap();
}

#[unittest::def_test]
pub fn test_authenc_wrong_tag() {
    let mut ht = TeeFsHtree::default();
    ht.data.fek = [0x01; TEE_FS_HTREE_FEK_SIZE];
    ht.data.head.enc_fek = [0x02; TEE_FS_HTREE_FEK_SIZE];
    ht.data.head.counter = 1;
    ht.data.head.iv = [0x03; TEE_FS_HTREE_IV_SIZE];
    ht.root.node.hash = [0x04; TEE_FS_HTREE_HASH_SIZE];

    let mut node_image = TeeFsHtreeNodeImage {
        iv: [0x05; TEE_FS_HTREE_IV_SIZE],
        ..Default::default()
    };

    let enc = authenc_init(
        TEE_OperationMode::TEE_MODE_ENCRYPT,
        &mut ht,
        Some(&mut node_image),
        4,
        None,
    )
    .unwrap();
    let mut crypt = [0u8; 4];
    let mut tag = [0u8; TEE_FS_HTREE_TAG_SIZE];
    authenc_encrypt_final(enc, &mut tag, b"test", &mut crypt).unwrap();

    let dec = authenc_init(
        TEE_OperationMode::TEE_MODE_DECRYPT,
        &mut ht,
        Some(&mut node_image),
        4,
        None,
    )
    .unwrap();
    let mut out = [0u8; 4];
    tag[0] ^= 1;
    assert!(authenc_decrypt_final(dec, &tag, &crypt, &mut out).is_err());
}
