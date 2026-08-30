// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use core::fmt::Write;

use kcgroup::Cgroup;
use kvfs::{CommandFile, Permission, SimpleFile, SimpleFileOperation, SimpleFs, VfsError};

use crate::{command, control, state::CgroupFsState};

pub(crate) fn membership_file(
    fs: Arc<SimpleFs>,
    cgroup: Arc<Cgroup>,
    state: Weak<CgroupFsState>,
) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => {
                let _operation = cgroup.begin_operation()?;
                let mut output = String::new();
                for process_id in kprocess::cgroup_member_process_ids(&cgroup) {
                    writeln!(output, "{process_id}").expect("writing to a String cannot fail");
                }
                Ok(Some(output))
            }
            SimpleFileOperation::Write { file, data } => {
                let _operation = cgroup.begin_operation()?;
                let state = state.upgrade().ok_or(VfsError::NoSuchDevice)?;
                let process_id = command::parse_command(data)?
                    .parse::<u32>()
                    .map_err(|_| VfsError::InvalidInput)?;
                kprocess::migrate_cgroup_process(process_id, &cgroup, |source, destination| {
                    if !source.is_descendant_of(state.view_root())
                        || !destination.is_descendant_of(state.view_root())
                    {
                        return Err(VfsError::NotFound);
                    }
                    let common = source.common_ancestor(destination)?;
                    let common_node = state.node(common)?;
                    let procs_inode = common_node
                        .file("cgroup.procs")
                        .ok_or(VfsError::PermissionDenied)?;
                    procs_inode.permission(Permission::MAY_WRITE, file.cred())
                })?;
                Ok(None)
            }
        }),
    )
}
