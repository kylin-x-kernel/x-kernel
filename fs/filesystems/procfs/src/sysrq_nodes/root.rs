// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{borrow::Cow, sync::Arc};

use kvfs::{
    CommandFile, DirMapping, SimpleFile, SimpleFileOperation, SimpleFs, VfsError, VfsResult,
};

fn sysrq_trigger_read() -> VfsResult<Cow<'static, [u8]>> {
    Ok(Cow::Borrowed(
        b"Write a SysRq command character, e.g. 't' to dump task backtraces.\n",
    ))
}

fn sysrq_trigger_write(data: &[u8]) -> VfsResult<()> {
    let Some(cmd) = data
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
    else {
        return Ok(());
    };

    match cmd {
        b't' => {
            ktask::snapshot::trigger("/proc/sysrq-trigger");
            Ok(())
        }
        _ => Err(VfsError::InvalidInput),
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "sysrq-trigger",
        SimpleFile::new_regular(
            fs,
            CommandFile::new(|op| match op {
                SimpleFileOperation::Read => sysrq_trigger_read().map(Some),
                SimpleFileOperation::Write { data, .. } => {
                    sysrq_trigger_write(data)?;
                    Ok(None)
                }
            }),
        ),
    );
}
