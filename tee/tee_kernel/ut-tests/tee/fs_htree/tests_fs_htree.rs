// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;
use crate::tee::common::file_ops::{FS_MODE_644, FS_OFLAG_DEFAULT, FileVariant};

#[unittest::def_test(user)]
fn test_tee_fs_htree_open() {
    let fd = FileVariant::open("/tmp/test_fs_htree_open.bin", FS_OFLAG_DEFAULT, FS_MODE_644)
        .unwrap();

    let uuid = TEE_UUID {
        timeLow: 1,
        timeMid: 2,
        timeHiAndVersion: 3,
        clockSeqAndNode: [4; 8],
    };
    let mut hash = [0u8; TEE_FS_HTREE_HASH_SIZE];

    let fd_back = fd;
    info!("fd_back: {:?}", fd_back);
    // 先创建文件 (create = true)
    let ht_create = tee_fs_htree_open(
        Box::new(TeeFsFdAux { fd }),
        true,
        Some(&mut hash),
        Some(&uuid),
    )
    .unwrap();

    assert_eq!(ht_create.data.uuid, uuid);
    assert!(!ht_create.data.dirty);

    // 再打开文件 (create = false)
    let ht_open = tee_fs_htree_open(
        Box::new(TeeFsFdAux { fd: fd_back }),
        false,
        Some(&mut hash),
        Some(&uuid),
    )
    .unwrap();

    assert_eq!(ht_open.data.uuid, uuid);
    assert!(!ht_open.data.dirty);
}
