// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem construction context.

use alloc::sync::Arc;

use crate::{FileSystemType, Path, SuperBlock, SuperBlockFlags, VfsResult};

/// One filesystem-construction request.
///
/// This is the one-shot subset of Linux `struct fs_context` needed by the
/// current mount API. Like Linux, the context contains transaction state, not
/// the calling process's `fs_struct`; KVFS passes pathname state explicitly to
/// [`FsContext::get_tree`] because it has no ambient `current` dependency.
pub struct FsContext<'a> {
    fs_type: &'static FileSystemType,
    source: Option<&'a str>,
    data: Option<&'a [u8]>,
    sb_flags: SuperBlockFlags,
    cred: &'a kcred::Cred,
}

impl<'a> FsContext<'a> {
    /// Creates a filesystem context from a validated mount request.
    pub const fn new(
        fs_type: &'static FileSystemType,
        source: Option<&'a str>,
        data: Option<&'a [u8]>,
        sb_flags: SuperBlockFlags,
        cred: &'a kcred::Cred,
    ) -> Self {
        Self {
            fs_type,
            source,
            data,
            sb_flags,
            cred,
        }
    }

    /// Runs this context's filesystem `get_tree` operation in a pathname
    /// lookup environment.
    ///
    /// Linux obtains these paths implicitly from `current->fs`. KVFS receives
    /// the caller's stable `fs_struct` snapshot explicitly so the VFS crate
    /// does not depend on process-global execution context.
    pub fn get_tree(&self, lookup_root: &Path, lookup_pwd: &Path) -> VfsResult<Arc<SuperBlock>> {
        self.fs_type.get_tree(self, lookup_root, lookup_pwd)
    }

    /// Returns the filesystem type selected for this request.
    pub const fn fs_type(&self) -> &'static FileSystemType {
        self.fs_type
    }

    /// Returns the source name supplied to `mount(2)`.
    pub const fn source(&self) -> Option<&'a str> {
        self.source
    }

    /// Returns bounded, kernel-owned filesystem-specific mount data.
    ///
    /// The opaque byte slice is borrowed for the synchronous `get_tree`
    /// operation. A filesystem must parse the representation it supports and
    /// copy any state that remains live after mounting.
    pub const fn data(&self) -> Option<&'a [u8]> {
        self.data
    }

    /// Returns the proposed VFS superblock flags.
    pub const fn sb_flags(&self) -> SuperBlockFlags {
        self.sb_flags
    }

    /// Returns the credentials used for source-path lookup.
    pub const fn cred(&self) -> &'a kcred::Cred {
        self.cred
    }
}
