// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs::{DirMapping, SimpleFs};

pub(crate) fn add_root_entries(_root: &mut DirMapping, _fs: Arc<SimpleFs>) {
    // /proc/interrupts stub — will be replaced with per-IRQ statistics from khal.
}
