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

use kvfs::{
    CommandFile, Dentry, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFile, SimpleFileOperation,
    SimpleFs, VfsError,
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
    fn child_names<'a>(&'a self) -> kvfs::VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(Box::new(["trace", "events"].into_iter().map(Cow::Borrowed)))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> kvfs::VfsResult<Dentry> {
        match name {
            "trace" => lookup.file(
                name,
                SimpleFile::new_regular(self.fs.clone(), || Ok(ktracing::dump_trace_records())),
            ),
            "events" => Ok(lookup.dir(
                name,
                SimpleDir::new_maker(
                    self.fs.clone(),
                    Arc::new(TracingEventsDir {
                        fs: self.fs.clone(),
                    }),
                ),
            )),
            _ => Err(VfsError::NotFound),
        }
    }
}

impl SimpleDirOps for TracingEventsDir {
    fn child_names<'a>(&'a self) -> kvfs::VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(Box::new(
            ktracing::subsystem_names().into_iter().map(Cow::Owned),
        ))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> kvfs::VfsResult<Dentry> {
        if !ktracing::subsystem_names().iter().any(|it| it == name) {
            return Err(VfsError::NotFound);
        }
        Ok(lookup.dir(
            name,
            SimpleDir::new_maker(
                self.fs.clone(),
                Arc::new(TracingSubsystemDir {
                    fs: self.fs.clone(),
                    subsystem: name.to_string(),
                }),
            ),
        ))
    }
}

impl SimpleDirOps for TracingSubsystemDir {
    fn child_names<'a>(&'a self) -> kvfs::VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(Box::new(
            ktracing::event_names(&self.subsystem)
                .into_iter()
                .map(Cow::Owned),
        ))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> kvfs::VfsResult<Dentry> {
        if !ktracing::event_names(&self.subsystem)
            .iter()
            .any(|it| it == name)
        {
            return Err(VfsError::NotFound);
        }
        Ok(lookup.dir(
            name,
            SimpleDir::new_maker(
                self.fs.clone(),
                Arc::new(TracingEventDir {
                    fs: self.fs.clone(),
                    subsystem: self.subsystem.clone(),
                    event: name.to_string(),
                }),
            ),
        ))
    }
}

impl SimpleDirOps for TracingEventDir {
    fn child_names<'a>(&'a self) -> kvfs::VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(Box::new(
            ["enable", "format", "id"].into_iter().map(Cow::Borrowed),
        ))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> kvfs::VfsResult<Dentry> {
        match name {
            "enable" => {
                let subsystem = self.subsystem.clone();
                let event = self.event.clone();
                lookup.file(
                    name,
                    SimpleFile::new_regular(
                        self.fs.clone(),
                        CommandFile::new(move |req| match req {
                            SimpleFileOperation::Read => {
                                ktracing::event_enable_state(&subsystem, &event)
                                    .map(Some)
                                    .ok_or(VfsError::NotFound)
                            }
                            SimpleFileOperation::Write { data, .. } => {
                                if ktracing::write_event_enable(&subsystem, &event, data) {
                                    Ok(None)
                                } else {
                                    Err(VfsError::InvalidInput)
                                }
                            }
                        }),
                    ),
                )
            }
            "format" => {
                let subsystem = self.subsystem.clone();
                let event = self.event.clone();
                lookup.file(
                    name,
                    SimpleFile::new_regular(self.fs.clone(), move || {
                        ktracing::event_format(&subsystem, &event).ok_or(VfsError::NotFound)
                    }),
                )
            }
            "id" => {
                let subsystem = self.subsystem.clone();
                let event = self.event.clone();
                lookup.file(
                    name,
                    SimpleFile::new_regular(self.fs.clone(), move || {
                        ktracing::event_id(&subsystem, &event).ok_or(VfsError::NotFound)
                    }),
                )
            }
            _ => Err(VfsError::NotFound),
        }
    }
}

pub(crate) fn add_root_entries(root: &mut kvfs::DirMapping, fs: Arc<SimpleFs>) {
    root.add_dir(
        "tracing",
        SimpleDir::new_maker(fs.clone(), Arc::new(TracingDir { fs })),
    );
}
