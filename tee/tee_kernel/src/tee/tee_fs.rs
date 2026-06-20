// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_raw_sys::TEE_OBJECT_ID_MAX_LEN;

use super::tee_ree_fs::TeeFsFd;

pub const TEE_FS_NAME_MAX: usize = 350;

/// GP: `typedef struct tee_file_handle` (REE file descriptor)
pub type TeeFileHandle = TeeFsFd;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
/// GP: `struct tee_fs_dirent`
pub struct TeeFsDirent {
    pub oid: [u8; TEE_OBJECT_ID_MAX_LEN as _],
    pub oid_len: u32,
}

impl Default for TeeFsDirent {
    fn default() -> Self {
        Self {
            oid: [0; TEE_OBJECT_ID_MAX_LEN as _],
            oid_len: 0,
        }
    }
}
// pub type TeeFileHandle = TeeFsFd;
