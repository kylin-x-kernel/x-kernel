// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use ktty::tty;
use kvfs::{DeviceId, DirMapping, SimpleDir, SimpleFs};

use super::pts::{Ptmx, PtsDir};
use crate::{DeviceFile, add_device_entry};

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    add_device_entry(
        root,
        "tty",
        DeviceFile::new_character(
            fs.clone(),
            DeviceId::new(5, 0),
            alloc::sync::Arc::new(tty::CurrentTty),
        ),
    );
    add_device_entry(
        root,
        "console",
        DeviceFile::new_character(fs.clone(), DeviceId::new(5, 1), tty::N_TTY.clone()),
    );
    add_device_entry(
        root,
        "ptmx",
        DeviceFile::new_character(
            fs.clone(),
            DeviceId::new(5, 2),
            alloc::sync::Arc::new(Ptmx(fs.clone())),
        ),
    );
    root.add(
        "pts",
        SimpleDir::new_maker(fs, alloc::sync::Arc::new(PtsDir)),
    );
}
