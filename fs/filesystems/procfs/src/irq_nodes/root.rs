// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{borrow::Cow, format, sync::Arc};

use kvfs::VfsResult;
use kvfs_simple::{DirMapping, SimpleFile, SimpleFs};

fn read_proc_interrupts() -> VfsResult<Cow<'static, [u8]>> {
    Ok(Cow::Owned(
        format!("0: {}\n", kcore::irq_stats::irq_cnt()).into_bytes(),
    ))
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "interrupts",
        SimpleFile::new_regular(fs, read_proc_interrupts),
    );
}
