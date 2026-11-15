// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, sync::Arc, vec::Vec};

use kcgroup::{Cgroup, ControllerKind};
use kvfs::{CommandFile, SimpleFile, SimpleFileOperation, SimpleFs, VfsError};

use crate::{command, control, cpu, cpuset, memory, pids};

type ControllerFiles = Vec<(&'static str, Arc<SimpleFile>)>;

/// One controller exposed by cgroup2fs.
///
/// `kind` is the single identity of the controller: availability, live state,
/// delegation flags, and `cgroup.subtree_control` handling all derive from the
/// generic [`Cgroup`] API. `files` only materializes the kind-specific control
/// files. Visibility is re-checked on every lookup, so a controller declared
/// here stays hidden until it is live on the cgroup.
pub(crate) struct ControllerAdapter {
    pub(crate) kind: ControllerKind,
    pub(crate) files: fn(Arc<SimpleFs>, Arc<Cgroup>) -> ControllerFiles,
}

impl ControllerAdapter {
    fn name(&self) -> &'static str {
        self.kind.name()
    }
}

const CONTROLLERS: &[ControllerAdapter] = &[
    ControllerAdapter {
        kind: ControllerKind::Pids,
        files: pids::files,
    },
    ControllerAdapter {
        kind: ControllerKind::Cpu,
        files: cpu::files,
    },
    ControllerAdapter {
        kind: ControllerKind::Cpuset,
        files: cpuset::files,
    },
    ControllerAdapter {
        kind: ControllerKind::Memory,
        files: memory::files,
    },
];

/// Returns the controller adapters in `cgroup.controllers` order.
pub(crate) fn adapters() -> &'static [ControllerAdapter] {
    CONTROLLERS
}

pub(crate) fn controllers_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        let mut available = Vec::new();
        for controller in CONTROLLERS {
            if cgroup.has_available_controller(controller.kind) {
                available.push(controller.name());
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
                    if cgroup.controller_subtree_enabled(controller.kind) {
                        enabled.push(controller.name());
                    }
                }
                Ok(Some(format!("{}\n", enabled.join(" "))))
            }
            SimpleFileOperation::Write { data, .. } => {
                let _operation = cgroup.begin_operation()?;
                let operations = command::parse_subtree_control(data)?;
                let mut resolved = Vec::with_capacity(operations.len());
                for operation in operations {
                    // cgroup v2 reports a controller that is absent from this
                    // cgroup as ENOENT, not EINVAL: the name is well formed,
                    // the requested capability is simply not there.
                    let kind =
                        ControllerKind::from_name(operation.name).ok_or(VfsError::InvalidInput)?;
                    if !CONTROLLERS.iter().any(|controller| controller.kind == kind) {
                        return Err(VfsError::NotFound);
                    }
                    // Linux applies duplicates last-token-wins. Dedup before
                    // mutation so a disable/enable pair cannot tear down and
                    // rebuild child controller state.
                    if let Some(existing) =
                        resolved.iter_mut().find(|(existing, _)| *existing == kind)
                    {
                        existing.1 = operation.is_enabled;
                    } else {
                        resolved.push((kind, operation.is_enabled));
                    }
                }
                cgroup.set_subtree_controls(&resolved)?;
                Ok(None)
            }
        }),
    )
}
