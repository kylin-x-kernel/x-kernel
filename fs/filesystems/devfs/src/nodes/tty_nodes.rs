// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use ktty::tty;
use kvfs::{DeviceId, DirMapping, SimpleDir, SimpleFs};

use super::pts::{Ptmx, PtsDir};
use crate::{DeviceFile, add_device_entry};

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    // Reserve both numeric ranges and enforce EIO for aliases before publishing PTMX.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: devfs root construction \
                  registers the Unix98 PTY ranges once and has no error return"
    )]
    tty::register_pty_drivers().expect("Unix98 drivers register once");
    match tty::register_fixed_vt_devices() {
        Ok(devices) => {
            for registration in devices {
                add_device_entry(
                    root,
                    registration.name(),
                    DeviceFile::from_registration(fs.clone(), &registration),
                );
            }
        }
        Err(error) => warn!("devtmpfs: cannot register fixed VTs: {error:?}"),
    }
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
