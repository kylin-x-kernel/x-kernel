// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs::{NodePermission, Path, VfsFile, dentry_open};
use memfs::shmem;

const O_RDWR: u32 = 2;

pub(crate) fn anonymous_location(name: &str) -> Path {
    shmem::create_kernel_file(
        name,
        NodePermission::from_bits_truncate(0o600),
        kcred::initial_cred(),
    )
    .map(|file| file.into_path())
    .expect("create anonymous file")
}

pub(crate) fn open_test_file(location: Path, flags: u32) -> Arc<VfsFile> {
    dentry_open(location, flags, kcred::initial_cred()).expect("open page-cache file")
}

pub(crate) fn page_cache_file(name: &str) -> Arc<VfsFile> {
    open_test_file(anonymous_location(name), O_RDWR)
}
