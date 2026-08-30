// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::ToString, sync::Arc};

use kcgroup::Cgroup;
use kvfs::{CommandFile, SimpleFile, SimpleFileOperation, SimpleFs, VfsError, VfsResult};

use crate::{command, control};

pub(crate) fn set_subtree_enabled(cgroup: &Cgroup, is_enabled: bool) -> VfsResult<()> {
    cgroup.set_pids_subtree_enabled(is_enabled)?;
    Ok(())
}

pub(crate) fn current_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        Ok(format!("{}\n", cgroup.pids_current()?))
    })
}

pub(crate) fn max_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => {
                let _operation = cgroup.begin_operation()?;
                Ok(Some(match cgroup.pids_max()? {
                    Some(limit) => format!("{limit}\n"),
                    None => "max\n".to_string(),
                }))
            }
            SimpleFileOperation::Write { data, .. } => {
                let _operation = cgroup.begin_operation()?;
                let value = command::parse_command(data)?;
                let limit = if value == "max" {
                    None
                } else {
                    Some(value.parse::<usize>().map_err(|_| VfsError::InvalidInput)?)
                };
                cgroup.set_pids_max(limit)?;
                Ok(None)
            }
        }),
    )
}
