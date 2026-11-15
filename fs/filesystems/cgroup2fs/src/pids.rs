// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::ToString, sync::Arc, vec::Vec};

use kcgroup::Cgroup;
use kvfs::{CommandFile, SimpleFile, SimpleFileOperation, SimpleFs, VfsError};

use crate::{command, control};

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

fn events_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        Ok(format!("max {}\n", cgroup.pids_max_hits()?))
    })
}

pub(crate) fn files(
    fs: Arc<SimpleFs>,
    cgroup: Arc<Cgroup>,
) -> Vec<(&'static str, Arc<SimpleFile>)> {
    alloc::vec![
        ("pids.current", current_file(fs.clone(), cgroup.clone())),
        ("pids.max", max_file(fs.clone(), cgroup.clone())),
        ("pids.events", events_file(fs, cgroup)),
    ]
}
