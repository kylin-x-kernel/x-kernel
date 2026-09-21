// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/dri/` — DRM device directory registration.

use alloc::sync::Arc;

use kvfs::{DirMapping, SimpleDir, SimpleFs};

use crate::dynamic::DynamicDeviceDir;

/// Register the dynamic `/dev/dri` directory.
pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "dri",
        SimpleDir::new_maker(fs, Arc::new(DynamicDeviceDir::drm())),
    );
}
