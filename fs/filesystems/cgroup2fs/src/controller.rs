// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, sync::Arc, vec::Vec};

use kcgroup::Cgroup;
use kvfs::{CommandFile, SimpleFile, SimpleFileOperation, SimpleFs, VfsError};

use crate::{command, control, pids};

struct ControllerAdapter {
    name: &'static str,
    is_available: fn(&Cgroup) -> bool,
    is_active: fn(&Cgroup) -> bool,
    set_subtree_enabled: fn(&Cgroup, bool) -> kvfs::VfsResult<()>,
}

const CONTROLLERS: &[ControllerAdapter] = &[ControllerAdapter {
    name: "pids",
    is_available: Cgroup::has_available_pids_controller,
    is_active: Cgroup::pids_subtree_enabled,
    set_subtree_enabled: pids::set_subtree_enabled,
}];

pub(crate) fn controllers_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        let mut available = Vec::new();
        for controller in CONTROLLERS {
            if (controller.is_available)(&cgroup) {
                available.push(controller.name);
            }
        }
        Ok(format!("{}\n", available.join(" ")))
    })
}

pub(crate) fn subtree_control_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => {
                let _operation = cgroup.begin_operation()?;
                let mut enabled = Vec::new();
                for controller in CONTROLLERS {
                    if (controller.is_active)(&cgroup) {
                        enabled.push(controller.name);
                    }
                }
                Ok(Some(format!("{}\n", enabled.join(" "))))
            }
            SimpleFileOperation::Write { data, .. } => {
                let _operation = cgroup.begin_operation()?;
                let operations = command::parse_subtree_control(data)?;
                let mut resolved = Vec::with_capacity(operations.len());
                for operation in operations {
                    let controller = CONTROLLERS
                        .iter()
                        .find(|controller| controller.name == operation.name)
                        .ok_or(VfsError::InvalidInput)?;
                    resolved.push((controller, operation.is_enabled));
                }
                for (controller, is_enabled) in resolved {
                    (controller.set_subtree_enabled)(&cgroup, is_enabled)?;
                }
                Ok(None)
            }
        }),
    )
}
