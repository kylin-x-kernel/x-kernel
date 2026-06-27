// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use ktty::tty;
use kvfs::{DeviceId, NodeType};
use kvfs_simple::{DirMapping, SimpleDir, SimpleFs};

use super::pts::{Ptmx, PtsDir};
use crate::DeviceFile;

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "tty",
        DeviceFile::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 0),
            alloc::sync::Arc::new(tty::CurrentTty),
        ),
    );
    root.add(
        "console",
        DeviceFile::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 1),
            (**tty::N_TTY).clone(),
        ),
    );
    root.add(
        "ptmx",
        DeviceFile::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 2),
            alloc::sync::Arc::new(Ptmx(fs.clone())),
        ),
    );
    root.add(
        "pts",
        SimpleDir::new_maker(fs, alloc::sync::Arc::new(PtsDir)),
    );
}
