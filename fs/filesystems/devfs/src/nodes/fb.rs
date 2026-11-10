// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/fb0` registration; the fbdev ABI and file operations live in
//! [`fbdevice`].

use alloc::sync::Arc;

use fbdevice::FrameBuffer;
use kvfs::{DeviceId, DirMapping, SimpleFs};

use crate::{DeviceFile, add_device_entry};

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    if !fbdevice::fb_available() {
        return;
    }
    let fb0 = DeviceFile::new_character(
        fs.clone(),
        DeviceId::new(29, 0),
        Arc::new(FrameBuffer::new()),
    );
    add_device_entry(root, "fb0", fb0);
}
