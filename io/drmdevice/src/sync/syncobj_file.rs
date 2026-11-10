// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The syncobj object descriptor (`syncobj_file_fops`).
//!
//! `DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD` without `EXPORT_SYNC_FILE` hands out a
//! descriptor for the syncobj *object*, not for its fence. The difference
//! matters: importing that descriptor with `SYNCOBJ_FD_TO_HANDLE` names the same
//! object, so a signal performed through one handle is visible through every
//! handle the object was imported to, and the object stays alive while any
//! descriptor for it is open. A sync_file, by contrast, is an immutable
//! snapshot.
//!
//! The file has no operations of its own: Linux's `drm_syncobj_file_fops` has
//! only `release`, and everything a client does with the object it does
//! through the DRM device's ioctls.

use alloc::sync::Arc;

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile, VfsResult};

use crate::model::syncobj::Syncobj;

/// The operations of a syncobj object descriptor.
struct SyncobjFileOps;

impl FileOperations for SyncobjFileOps {}

/// Creates a descriptor that names `object`.
pub(crate) fn new_syncobj_file(object: Arc<Syncobj>, cred: &Arc<Cred>) -> VfsResult<Arc<VfsFile>> {
    AnonInodeFs::global().get_file(
        "syncobj_file",
        Arc::new(SyncobjFileOps),
        object,
        FMode::READ,
        OpenFlags::empty(),
        cred.clone(),
    )
}

/// The syncobj object behind a descriptor, if it is a syncobj descriptor.
pub(crate) fn object_of(file: &VfsFile) -> Option<Arc<Syncobj>> {
    file.private_data_get::<Syncobj>()
}
