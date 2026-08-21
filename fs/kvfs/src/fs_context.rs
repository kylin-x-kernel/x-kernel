// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem construction context.

use alloc::{boxed::Box, sync::Arc};
use core::any::Any;

use crate::{
    FileSystemType, FsContextOperations, Path, SuperBlock, SuperBlockFlags, VfsError, VfsResult,
};

/// Purpose of one filesystem-context transaction.
///
/// This mirrors Linux `enum fs_context_purpose` for the operations currently
/// implemented by KVFS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsContextPurpose {
    /// Construct a new superblock or find a shareable existing one.
    Mount,
    /// Reconfigure an existing superblock.
    Reconfigure,
}

/// One filesystem-construction request.
///
/// This is the one-shot subset of Linux `struct fs_context` needed by the
/// current mount API. Like Linux, the context contains transaction state, not
/// the calling process's `fs_struct`; KVFS passes pathname state explicitly to
/// [`FsContext::get_tree`] because it has no ambient `current` dependency.
pub struct FsContext<'a> {
    fs_type: &'static FileSystemType,
    purpose: FsContextPurpose,
    source: Option<&'a str>,
    data: Option<&'a [u8]>,
    sb_flags: SuperBlockFlags,
    sb_flags_mask: SuperBlockFlags,
    cred: &'a kcred::Cred,
    target_super_block: Option<&'a SuperBlock>,
    operations: Option<&'static FsContextOperations>,
    fs_private: Option<Box<dyn Any + Send + Sync>>,
    proposed_super_private: Option<Box<dyn Any + Send + Sync>>,
}

impl<'a> FsContext<'a> {
    /// Creates a filesystem context from a validated mount request.
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem rejects context initialization.
    pub fn new(
        fs_type: &'static FileSystemType,
        source: Option<&'a str>,
        data: Option<&'a [u8]>,
        sb_flags: SuperBlockFlags,
        cred: &'a kcred::Cred,
    ) -> VfsResult<Self> {
        Self::initialize(Self {
            fs_type,
            purpose: FsContextPurpose::Mount,
            source,
            data,
            sb_flags,
            sb_flags_mask: SuperBlockFlags::empty(),
            cred,
            target_super_block: None,
            operations: None,
            fs_private: None,
            proposed_super_private: None,
        })
    }

    /// Creates a transaction that proposes changes to an existing superblock.
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem rejects context initialization.
    pub fn new_reconfigure(
        super_block: &'a SuperBlock,
        source: Option<&'a str>,
        data: Option<&'a [u8]>,
        sb_flags: SuperBlockFlags,
        sb_flags_mask: SuperBlockFlags,
        cred: &'a kcred::Cred,
    ) -> VfsResult<Self> {
        Self::initialize(Self {
            fs_type: super_block.file_system_type(),
            purpose: FsContextPurpose::Reconfigure,
            source,
            data,
            sb_flags,
            sb_flags_mask,
            cred,
            target_super_block: Some(super_block),
            operations: None,
            fs_private: None,
            proposed_super_private: None,
        })
    }

    /// Runs this context's filesystem `get_tree` operation in a pathname
    /// lookup environment.
    ///
    /// Linux obtains these paths implicitly from `current->fs`. KVFS receives
    /// the caller's stable `fs_struct` snapshot explicitly so the VFS crate
    /// does not depend on process-global execution context.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::InvalidInput`] for a reconfigure-purpose context.
    /// Otherwise propagates validation, lookup, device, or filesystem
    /// construction errors from the shared context operation table.
    pub fn get_tree(
        &mut self,
        lookup_root: &Path,
        lookup_pwd: &Path,
    ) -> VfsResult<Arc<SuperBlock>> {
        if self.purpose != FsContextPurpose::Mount {
            return Err(VfsError::InvalidInput);
        }
        self.operations()?.get_tree(self, lookup_root, lookup_pwd)
    }

    /// Returns the filesystem type selected for this request.
    pub const fn fs_type(&self) -> &'static FileSystemType {
        self.fs_type
    }

    /// Returns whether this context mounts or reconfigures a filesystem.
    pub const fn purpose(&self) -> FsContextPurpose {
        self.purpose
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

    /// Returns the superblock flags changed by this transaction.
    pub const fn sb_flags_mask(&self) -> SuperBlockFlags {
        self.sb_flags_mask
    }

    /// Returns the credentials used for source-path lookup.
    pub const fn cred(&self) -> &'a kcred::Cred {
        self.cred
    }

    /// Returns the superblock targeted by a reconfigure transaction.
    ///
    /// This is the direct owner reached through Linux `fs_context::root->d_sb`.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::InvalidInput`] for a mount-purpose context.
    pub fn super_block(&self) -> VfsResult<&'a SuperBlock> {
        self.target_super_block.ok_or(VfsError::InvalidInput)
    }

    /// Installs the immutable operation table selected by the filesystem type.
    ///
    /// This is the object-oriented equivalent of
    /// `file_system_type::init_fs_context` assigning Linux `fs_context::ops`.
    pub fn set_operations(&mut self, operations: &'static FsContextOperations) {
        self.operations = Some(operations);
    }

    /// Installs parsed filesystem-private transaction state.
    pub fn set_private<T>(&mut self, private: T)
    where
        T: Any + Send + Sync,
    {
        self.fs_private = Some(Box::new(private));
    }

    /// Borrows parsed filesystem-private transaction state.
    ///
    /// # Errors
    ///
    /// Returns [`crate::VfsError::InvalidInput`] if no private state of type
    /// `T` was installed by the filesystem context initializer or parser.
    pub fn private<T>(&self) -> VfsResult<&T>
    where
        T: Any + Send + Sync,
    {
        self.fs_private
            .as_deref()
            .and_then(|private| private.downcast_ref::<T>())
            .ok_or(crate::VfsError::InvalidInput)
    }

    /// Installs proposed superblock-private state for this transaction.
    ///
    /// This corresponds to Linux `fs_context::s_fs_info`, independently of
    /// parsed option state in `fs_private`.
    pub fn set_super_private<T>(&mut self, private: T)
    where
        T: Any + Send + Sync,
    {
        self.proposed_super_private = Some(Box::new(private));
    }

    /// Borrows the proposed superblock-private state.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::InvalidInput`] if no proposed state of type `T`
    /// was installed for this transaction.
    pub fn super_private<T>(&self) -> VfsResult<&T>
    where
        T: Any + Send + Sync,
    {
        self.proposed_super_private
            .as_deref()
            .and_then(|private| private.downcast_ref::<T>())
            .ok_or(VfsError::InvalidInput)
    }

    /// Validates and applies this transaction to an existing superblock.
    pub(crate) fn reconfigure(&mut self) -> VfsResult<()> {
        self.operations()?.reconfigure(self)
    }

    fn operations(&self) -> VfsResult<&'static FsContextOperations> {
        self.operations.ok_or(VfsError::InvalidInput)
    }

    fn initialize(mut context: Self) -> VfsResult<Self> {
        context.fs_type.init_context(&mut context)?;
        if context.operations.is_none() {
            return Err(VfsError::InvalidInput);
        }
        Ok(context)
    }
}
