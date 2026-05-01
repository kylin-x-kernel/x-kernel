// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/proc/tracing` — trace pipe and tracefs-style `events` layout backed by `ktracing`.

use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
};

use fs_ng_vfs::VfsError;
use kcore::vfs::{
    DirMaker, NodeOpsMux, RwFile, SimpleDir, SimpleDirOps, SimpleFile, SimpleFileOperation,
    SimpleFs,
};

struct TracingDir {
    fs: Arc<SimpleFs>,
}

struct TracingEventsDir {
    fs: Arc<SimpleFs>,
}

struct TracingSubsystemDir {
    fs: Arc<SimpleFs>,
    subsystem: String,
}

struct TracingEventDir {
    fs: Arc<SimpleFs>,
    subsystem: String,
    event: String,
}

impl SimpleDirOps for TracingDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(["trace", "events"].into_iter().map(Cow::Borrowed))
    }

    fn lookup_child(&self, name: &str) -> fs_ng_vfs::VfsResult<NodeOpsMux> {
        Ok(match name {
            "trace" => {
                SimpleFile::new_regular(self.fs.clone(), || Ok(ktracing::dump_trace_records()))
                    .into()
            }
            "events" => SimpleDir::new_maker(
                self.fs.clone(),
                Arc::new(TracingEventsDir {
                    fs: self.fs.clone(),
                }),
            )
            .into(),
            _ => return Err(VfsError::NotFound),
        })
    }
}

impl SimpleDirOps for TracingEventsDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(ktracing::subsystem_names().into_iter().map(Cow::Owned))
    }

    fn lookup_child(&self, name: &str) -> fs_ng_vfs::VfsResult<NodeOpsMux> {
        if !ktracing::subsystem_names().iter().any(|it| it == name) {
            return Err(VfsError::NotFound);
        }
        Ok(SimpleDir::new_maker(
            self.fs.clone(),
            Arc::new(TracingSubsystemDir {
                fs: self.fs.clone(),
                subsystem: name.to_string(),
            }),
        )
        .into())
    }
}

impl SimpleDirOps for TracingSubsystemDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(
            ktracing::event_names(&self.subsystem)
                .into_iter()
                .map(Cow::Owned),
        )
    }

    fn lookup_child(&self, name: &str) -> fs_ng_vfs::VfsResult<NodeOpsMux> {
        if !ktracing::event_names(&self.subsystem)
            .iter()
            .any(|it| it == name)
        {
            return Err(VfsError::NotFound);
        }
        Ok(SimpleDir::new_maker(
            self.fs.clone(),
            Arc::new(TracingEventDir {
                fs: self.fs.clone(),
                subsystem: self.subsystem.clone(),
                event: name.to_string(),
            }),
        )
        .into())
    }
}

impl SimpleDirOps for TracingEventDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(["enable", "format", "id"].into_iter().map(Cow::Borrowed))
    }

    fn lookup_child(&self, name: &str) -> fs_ng_vfs::VfsResult<NodeOpsMux> {
        Ok(match name {
            "enable" => {
                let subsystem = self.subsystem.clone();
                let event = self.event.clone();
                SimpleFile::new_regular(
                    self.fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            ktracing::event_enable_state(&subsystem, &event)
                                .map(Some)
                                .ok_or(VfsError::NotFound)
                        }
                        SimpleFileOperation::Write(data) => {
                            if ktracing::write_event_enable(&subsystem, &event, data) {
                                Ok(None)
                            } else {
                                Err(VfsError::InvalidInput)
                            }
                        }
                    }),
                )
                .into()
            }
            "format" => {
                let subsystem = self.subsystem.clone();
                let event = self.event.clone();
                SimpleFile::new_regular(self.fs.clone(), move || {
                    ktracing::event_format(&subsystem, &event).ok_or(VfsError::NotFound)
                })
                .into()
            }
            "id" => {
                let subsystem = self.subsystem.clone();
                let event = self.event.clone();
                SimpleFile::new_regular(self.fs.clone(), move || {
                    ktracing::event_id(&subsystem, &event).ok_or(VfsError::NotFound)
                })
                .into()
            }
            _ => return Err(VfsError::NotFound),
        })
    }
}

/// [`DirMaker`] for the `tracing` directory under procfs (`/proc/tracing`).
pub fn tracing_dir_maker(fs: Arc<SimpleFs>) -> DirMaker {
    SimpleDir::new_maker(fs.clone(), Arc::new(TracingDir { fs }))
}
