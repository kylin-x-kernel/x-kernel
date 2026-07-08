// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Entry-owned bootstrap orchestration helpers.

pub(crate) fn init_virtual_filesystems() {
    fs_boot::mount_virtual_filesystems();
}

pub(crate) fn init_alarm_runtime() {
    info!("Initialize alarm...");
    kprocess::init_timer_runtime();
}
