// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use crate::{
    ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, Ext4SbInfo, FilesystemBlock, PhysicalBlock,
    UnsupportedKind,
    disk::{checksum, codec, inode as disk_inode, xattr as disk_xattr},
    inode::{Ext4Inode, Ext4InodeMetadata, Ext4Timestamp, update_inode_ctime_bytes},
    jbd2::{JournalCredits, JournalHandle},
    superblock::replace_metadata_access_bytes,
};

const XATTR_INODE_UPDATE_CREDITS: u32 = 1;
const XATTR_EXTERNAL_REWRITE_CREDITS: u32 = 1;
const XATTR_EXTERNAL_ALLOC_CREDITS: u32 = 8;
const XATTR_EXTERNAL_RELEASE_CREDITS: u32 = 8;
const XATTR_EXTERNAL_SHARED_REFCOUNT_CREDITS: u32 = 1;

pub(crate) fn external_xattr_eviction_credits(inode: &Ext4Inode) -> u32 {
    if inode.file_acl_block() == 0 {
        0
    } else {
        XATTR_INODE_UPDATE_CREDITS + XATTR_EXTERNAL_RELEASE_CREDITS
    }
}

/// Ext4 extended-attribute namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ext4XattrNamespace {
    /// `user.*` namespace.
    User,
    /// `system.posix_acl_access`.
    PosixAclAccess,
    /// `system.posix_acl_default`.
    PosixAclDefault,
    /// `trusted.*` namespace.
    Trusted,
    /// Lustre-private namespace.
    Lustre,
    /// `security.*` namespace.
    Security,
    /// `system.*` namespace.
    System,
    /// RichACL namespace.
    RichAcl,
    /// Encryption namespace.
    Encryption,
    /// Hurd-private namespace.
    Hurd,
    /// A namespace not interpreted by this stage.
    Unknown(u8),
}

/// Existence requirement for an extended-attribute update.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Ext4XattrSetMode {
    /// Create a missing attribute or replace an existing value.
    #[default]
    CreateOrReplace,
    /// Create a new attribute and fail if it already exists.
    Create,
    /// Replace an existing attribute and fail if it is absent.
    Replace,
    /// Require both creation and replacement semantics.
    ///
    /// This matches Linux when both `XATTR_CREATE` and `XATTR_REPLACE` are
    /// supplied: an existing attribute fails with `EEXIST`, while a missing
    /// attribute fails with `ENODATA`.
    CreateAndReplace,
}

impl Ext4XattrNamespace {
    const fn from_index(index: u8) -> Self {
        match index {
            1 => Self::User,
            2 => Self::PosixAclAccess,
            3 => Self::PosixAclDefault,
            4 => Self::Trusted,
            5 => Self::Lustre,
            6 => Self::Security,
            7 => Self::System,
            8 => Self::RichAcl,
            9 => Self::Encryption,
            10 => Self::Hurd,
            value => Self::Unknown(value),
        }
    }

    const fn index(self) -> u8 {
        match self {
            Self::User => 1,
            Self::PosixAclAccess => 2,
            Self::PosixAclDefault => 3,
            Self::Trusted => 4,
            Self::Lustre => 5,
            Self::Security => 6,
            Self::System => 7,
            Self::RichAcl => 8,
            Self::Encryption => 9,
            Self::Hurd => 10,
            Self::Unknown(index) => index,
        }
    }
}

/// One decoded ext4 extended attribute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ext4Xattr {
    namespace: Ext4XattrNamespace,
    name: Vec<u8>,
    value: Vec<u8>,
}

impl Ext4Xattr {
    /// Returns the xattr namespace.
    pub const fn namespace(&self) -> Ext4XattrNamespace {
        self.namespace
    }

    /// Returns the raw xattr name suffix without the namespace prefix.
    pub fn name_bytes(&self) -> &[u8] {
        &self.name
    }

    /// Returns the raw xattr value bytes.
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

/// A borrowed ext4 extended-attribute name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4XattrNameRef<'a> {
    namespace: Ext4XattrNamespace,
    name: &'a [u8],
}

impl<'a> Ext4XattrNameRef<'a> {
    /// Returns the xattr namespace.
    pub const fn namespace(self) -> Ext4XattrNamespace {
        self.namespace
    }

    /// Returns the raw xattr name suffix without the namespace prefix.
    pub const fn name_bytes(self) -> &'a [u8] {
        self.name
    }
}

/// Receives borrowed xattr names while KExt4 walks inode metadata.
///
/// Implementations must consume each borrowed name before `emit` returns.
/// Disk-format and I/O failures are reported by [`Ext4SbInfo::list_xattrs`].
pub trait Ext4XattrNameSink {
    /// Consumes one xattr name without taking ownership of its bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the consumer cannot accept the name.
    fn emit(&mut self, name: Ext4XattrNameRef<'_>) -> Ext4Result<()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedXattrEntry<'a> {
    namespace: Ext4XattrNamespace,
    name: &'a [u8],
    value_offset: usize,
    value_size: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum XattrEntryOrder {
    Unchecked,
    RequireSorted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum XattrStorageLayout {
    Empty,
    Inline,
    External { shared: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum XattrMutation {
    Changed,
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct XattrStoragePlan {
    inline: Vec<Ext4Xattr>,
    external: Vec<Ext4Xattr>,
}

impl XattrStoragePlan {
    fn layout(&self) -> XattrStorageLayout {
        if !self.external.is_empty() {
            XattrStorageLayout::External { shared: false }
        } else {
            xattr_inline_layout(&self.inline)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExternalXattrBlockAction {
    None,
    Reuse,
    Rewrite,
    Allocate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InodeXattrStoragePlan {
    extra_isize: u16,
    storage: XattrStoragePlan,
    external_block_action: ExternalXattrBlockAction,
}

enum XattrMutationPlan {
    Unchanged,
    Changed {
        credits: JournalCredits,
        storage: InodeXattrStoragePlan,
    },
}

enum XattrMutationRequest<'a> {
    Set {
        namespace: Ext4XattrNamespace,
        name: &'a [u8],
        value: &'a [u8],
        mode: Ext4XattrSetMode,
    },
    Remove {
        namespace: Ext4XattrNamespace,
        name: &'a [u8],
    },
}

enum PlannedXattrMutation {
    Unchanged,
    Changed(InodeXattrStoragePlan),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExtraIsizeLayoutCandidate {
    extra_isize: u16,
    inline_capacity: usize,
}

impl Ext4SbInfo {
    /// Reads all supported extended attributes stored on an inode.
    pub(crate) fn read_xattrs(&self, inode: &Ext4Inode) -> Ext4Result<Vec<Ext4Xattr>> {
        let mut xattrs = Vec::new();
        self.read_inline_xattrs(inode, &mut xattrs)?;
        self.read_external_xattrs(inode, &mut xattrs)?;
        Ok(xattrs)
    }

    /// Walks supported extended-attribute names without materializing values.
    ///
    /// Names borrow from validated inode snapshots or external-block metadata
    /// and remain valid only for the duration of each sink call.
    pub(crate) fn list_xattrs(
        &self,
        inode: &Ext4Inode,
        sink: &mut dyn Ext4XattrNameSink,
    ) -> Ext4Result<()> {
        let inline_xattrs = inode.inline_xattr_bytes();
        list_inline_xattr_names_from_bytes(&inline_xattrs, sink)?;
        self.list_external_xattr_names(inode, sink)
    }

    /// Reads one extended attribute by namespace and raw name suffix.
    pub(crate) fn get_xattr(
        &self,
        inode: &Ext4Inode,
        namespace: Ext4XattrNamespace,
        name: &[u8],
    ) -> Ext4Result<Option<Vec<u8>>> {
        Ok(self
            .read_xattrs(inode)?
            .into_iter()
            .find(|xattr| xattr.namespace == namespace && xattr.name == name)
            .map(|xattr| xattr.value))
    }

    /// Sets or replaces an extended attribute.
    ///
    /// This R9 baseline supports the common `user`, `trusted`, and `security`
    /// namespaces, plus opaque POSIX ACL xattr storage. The updated xattr set
    /// may be split between the inode body and one external xattr block, with
    /// refcount and checksum maintenance for the external part.
    #[cfg(unittest)]
    pub(crate) fn set_xattr(
        &mut self,
        inode: &Ext4Inode,
        namespace: Ext4XattrNamespace,
        name: &[u8],
        value: &[u8],
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        self.set_xattr_with_mode(
            inode,
            namespace,
            name,
            value,
            Ext4XattrSetMode::CreateOrReplace,
            timestamp,
        )
    }

    /// Sets an extended attribute with an atomic existence requirement.
    pub(crate) fn set_xattr_with_mode(
        &mut self,
        inode: &Ext4Inode,
        namespace: Ext4XattrNamespace,
        name: &[u8],
        value: &[u8],
        mode: Ext4XattrSetMode,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        validate_settable_xattr(namespace, name)?;
        self.validate_inode_timestamp_update(inode, timestamp)?;
        let XattrMutationPlan::Changed { credits, storage } = self.xattr_mutation_plan(
            inode,
            timestamp,
            XattrMutationRequest::Set {
                namespace,
                name,
                value,
                mode,
            },
        )?
        else {
            return Ok(());
        };
        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )?;
        let mut handle = journal.begin(credits)?;
        let result = self.update_xattr_in_transaction(inode, timestamp, &mut handle, storage);
        self.complete_metadata_mutation(handle, result)
    }

    /// Removes an extended attribute.
    pub(crate) fn remove_xattr(
        &mut self,
        inode: &Ext4Inode,
        namespace: Ext4XattrNamespace,
        name: &[u8],
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        validate_settable_xattr(namespace, name)?;
        self.validate_inode_timestamp_update(inode, timestamp)?;
        let XattrMutationPlan::Changed { credits, storage } = self.xattr_mutation_plan(
            inode,
            timestamp,
            XattrMutationRequest::Remove { namespace, name },
        )?
        else {
            return Ok(());
        };
        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )?;
        let mut handle = journal.begin(credits)?;
        let result = self.update_xattr_in_transaction(inode, timestamp, &mut handle, storage);
        self.complete_metadata_mutation(handle, result)?;
        inode.enable_extra_isize_expansion();
        Ok(())
    }

    fn update_xattr_in_transaction(
        &mut self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
        plan: InodeXattrStoragePlan,
    ) -> Ext4Result<()> {
        self.apply_xattr_storage_plan(inode, plan, None, Some(timestamp), handle)
    }

    fn apply_xattr_storage_plan(
        &mut self,
        inode: &Ext4Inode,
        plan: InodeXattrStoragePlan,
        preallocated_external_block: Option<FilesystemBlock>,
        timestamp: Option<Ext4Timestamp>,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let old_external_block = inode.file_acl_block();
        let new_external_block = match plan.external_block_action {
            ExternalXattrBlockAction::None => {
                if !plan.storage.external.is_empty() || preallocated_external_block.is_some() {
                    return Err(Ext4Error::InvalidJournalTransaction);
                }
                None
            }
            ExternalXattrBlockAction::Reuse => {
                if plan.storage.external.is_empty()
                    || old_external_block == 0
                    || preallocated_external_block.is_some()
                {
                    return Err(Ext4Error::InvalidJournalTransaction);
                }
                Some(FilesystemBlock::new(old_external_block))
            }
            ExternalXattrBlockAction::Rewrite => {
                if plan.storage.external.is_empty()
                    || old_external_block == 0
                    || preallocated_external_block.is_some()
                {
                    return Err(Ext4Error::InvalidJournalTransaction);
                }
                let block = FilesystemBlock::new(old_external_block);
                self.write_external_xattr_block(block, &plan.storage.external, handle)?;
                Some(block)
            }
            ExternalXattrBlockAction::Allocate => {
                if plan.storage.external.is_empty() {
                    return Err(Ext4Error::InvalidJournalTransaction);
                }
                let block = if let Some(block) = preallocated_external_block {
                    self.write_external_xattr_block(block, &plan.storage.external, handle)?;
                    block
                } else {
                    self.create_external_xattr_block(&plan.storage.external, handle)?
                };
                Some(block)
            }
        };
        if old_external_block != 0
            && new_external_block.is_none_or(|block| block.get() != old_external_block)
        {
            self.drop_external_xattr_block(inode, old_external_block, handle)?;
        }

        let (inode_table_block, inode_table_bytes, updated_inode) =
            self.prepare_xattr_inode_update(inode, &plan, new_external_block, timestamp)?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    fn xattr_mutation_plan(
        &self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        request: XattrMutationRequest<'_>,
    ) -> Ext4Result<XattrMutationPlan> {
        let old_external_block = inode.file_acl_block();
        let mut inline_xattrs = Vec::new();
        self.read_inline_xattrs(inode, &mut inline_xattrs)?;
        sort_xattrs(&mut inline_xattrs);
        let old_inline_layout = xattr_inline_layout(&inline_xattrs);
        let mut old_external_xattrs = Vec::new();
        let old_external_refcount =
            self.read_external_xattrs_with_refcount(inode, &mut old_external_xattrs)?;
        sort_xattrs(&mut old_external_xattrs);
        let old_layout = old_external_refcount.map_or(old_inline_layout, |refcount| {
            XattrStorageLayout::External {
                shared: refcount > 1,
            }
        });
        let inode_size = usize::from(self.superblock().inode_size());
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let current_extra_isize = inode.extra_isize();
        let inline_capacity =
            extra_isize_layout_candidate(inode_size, current_extra_isize)?.inline_capacity;
        let free_blocks_count = self.free_blocks_count();
        let plan = match request {
            XattrMutationRequest::Set {
                namespace,
                name,
                value,
                mode,
            } => match plan_xattr_set_storage(
                &inline_xattrs,
                &old_external_xattrs,
                namespace,
                name,
                value,
                mode,
                current_extra_isize,
                inline_capacity,
                block_size,
                old_layout,
                free_blocks_count,
            )? {
                PlannedXattrMutation::Unchanged => return Ok(XattrMutationPlan::Unchanged),
                PlannedXattrMutation::Changed(plan) => plan,
            },
            XattrMutationRequest::Remove { namespace, name } => plan_xattr_remove_storage(
                &inline_xattrs,
                &old_external_xattrs,
                namespace,
                name,
                current_extra_isize,
                inline_capacity,
                block_size,
                old_layout,
                free_blocks_count,
            )?,
        };
        let new_layout = plan.storage.layout();
        let planned_external_block = (!plan.storage.external.is_empty())
            .then_some(FilesystemBlock::new(old_external_block.max(1)));
        self.prepare_xattr_inode_update(inode, &plan, planned_external_block, Some(timestamp))?;
        Ok(XattrMutationPlan::Changed {
            credits: JournalCredits::new(xattr_mutation_credit_count(
                old_layout,
                new_layout,
                plan.external_block_action,
            )),
            storage: plan,
        })
    }

    /// Best-effort expansion of an existing inode's extra fields.
    ///
    /// Linux performs this step when an inode is dirtied. KExt4 follows the
    /// same fallback rule: try `want_extra_isize`, then `min_extra_isize`, and
    /// keep the current size if neither layout can preserve every xattr.
    /// Layout and allocation-space failures leave ordinary inode metadata
    /// updates available; device, checksum, and corruption errors still
    /// propagate because they invalidate the transaction's integrity.
    pub(crate) fn try_expand_inode_extra_isize(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if inode.is_extra_isize_expansion_disabled() {
            return Ok(());
        }
        let current_extra_isize = inode.extra_isize();
        let desired_extra_isize = self.superblock().want_extra_isize();
        if current_extra_isize >= desired_extra_isize {
            return Ok(());
        }

        let mut inline_xattrs = Vec::new();
        self.read_inline_xattrs(inode, &mut inline_xattrs)?;
        let mut external_xattrs = Vec::new();
        let external_refcount =
            self.read_external_xattrs_with_refcount(inode, &mut external_xattrs)?;
        let old_layout = external_refcount.map_or_else(
            || xattr_inline_layout(&inline_xattrs),
            |refcount| XattrStorageLayout::External {
                shared: refcount > 1,
            },
        );
        let plan = match self.plan_inode_xattr_expansion(
            inode,
            &inline_xattrs,
            &external_xattrs,
            old_layout,
        ) {
            Ok(plan) => plan,
            Err(Ext4Error::NoSpace) => {
                inode.disable_extra_isize_expansion();
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if plan.extra_isize == current_extra_isize {
            inode.disable_extra_isize_expansion();
            return Ok(());
        }

        let total_credits = xattr_mutation_credit_count(
            old_layout,
            plan.storage.layout(),
            plan.external_block_action,
        );
        let additional_credits = total_credits.saturating_sub(XATTR_INODE_UPDATE_CREDITS);
        if additional_credits != 0 {
            match handle.reserve_more(JournalCredits::new(additional_credits)) {
                Ok(()) => {}
                Err(Ext4Error::InsufficientJournalCredits | Ext4Error::JournalBusy) => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
        let preallocated_external_block =
            if plan.external_block_action == ExternalXattrBlockAction::Allocate {
                match self.allocate_block(None, handle) {
                    Ok(allocation) => Some(FilesystemBlock::new(allocation.block().get())),
                    Err(Ext4Error::NoSpace) => {
                        inode.disable_extra_isize_expansion();
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
        self.apply_xattr_storage_plan(inode, plan, preallocated_external_block, None, handle)
    }

    fn plan_inode_xattr_expansion(
        &self,
        inode: &Ext4Inode,
        inline_xattrs: &[Ext4Xattr],
        external_xattrs: &[Ext4Xattr],
        old_layout: XattrStorageLayout,
    ) -> Ext4Result<InodeXattrStoragePlan> {
        let inode_size = usize::from(self.superblock().inode_size());
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let current_extra_isize = inode.extra_isize();
        let candidates = extra_isize_layout_candidates(
            inode_size,
            current_extra_isize,
            self.superblock().want_extra_isize(),
            self.superblock().min_extra_isize(),
        )?;

        let mut current_external = Vec::from(external_xattrs);
        sort_xattrs(&mut current_external);
        select_extra_isize_storage_plan(candidates, |inline_capacity| {
            let Some(storage) = plan_xattr_expansion_storage(
                inline_xattrs,
                external_xattrs,
                inline_capacity,
                block_size,
            )?
            else {
                return Ok(None);
            };
            let rewrite_external = storage.external != current_external;
            let external_block_action =
                external_xattr_block_action(old_layout, &storage, rewrite_external);
            if external_block_action == ExternalXattrBlockAction::Allocate
                && self.free_blocks_count() == 0
            {
                return Ok(None);
            }
            Ok(Some((storage, external_block_action)))
        })
    }

    fn prepare_xattr_inode_update(
        &self,
        inode: &Ext4Inode,
        plan: &InodeXattrStoragePlan,
        new_external_block: Option<FilesystemBlock>,
        timestamp: Option<Ext4Timestamp>,
    ) -> Ext4Result<(FilesystemBlock, Vec<u8>, Ext4InodeMetadata)> {
        let old_external_block = inode.file_acl_block();
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| {
                let raw = disk_inode::RawInode::decode(inode_bytes)?;
                validate_nonshrinking_extra_isize(raw.extra_isize(), plan.extra_isize)?;
                let old_inline_xattr_offset =
                    inline_xattr_offset(inode_bytes.len(), raw.extra_isize())?;
                inode_bytes
                    .get_mut(old_inline_xattr_offset..)
                    .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
                    .fill(0);
                if inode_bytes.len() >= disk_inode::EXTRA_ISIZE_OFFSET + 2 {
                    put_u16(
                        inode_bytes,
                        disk_inode::EXTRA_ISIZE_OFFSET,
                        plan.extra_isize,
                    )?;
                } else if plan.extra_isize != 0 {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
                }
                let inline_xattr_offset = inline_xattr_offset(inode_bytes.len(), plan.extra_isize)?;
                let inline_xattr_bytes = inode_bytes
                    .get_mut(inline_xattr_offset..)
                    .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
                encode_inline_xattrs(&plan.storage.inline, inline_xattr_bytes)?;
                update_inode_xattr_block_bytes(
                    inode_bytes,
                    &raw,
                    InodeXattrBlockUpdate {
                        file_acl: new_external_block.map_or(0, FilesystemBlock::get),
                        current_blocks: inode.blocks(),
                        had_external_block: old_external_block != 0,
                        has_external_block: new_external_block.is_some(),
                        block_size: self.layout().block_size(),
                        has_64bit: self.superblock().features().has_64bit(),
                    },
                )?;
                if let Some(timestamp) = timestamp {
                    update_inode_ctime_bytes(inode_bytes, timestamp)?;
                }
                Ok(())
            },
        )?;
        Ok((inode_table_block, inode_table_bytes, updated_inode))
    }

    fn create_external_xattr_block(
        &mut self,
        xattrs: &[Ext4Xattr],
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<FilesystemBlock> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        if external_xattr_encoded_len(xattrs)? > block_size {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock));
        }
        let allocation = self.allocate_block(None, handle)?;
        let block = FilesystemBlock::new(allocation.block().get());
        self.write_external_xattr_block(block, xattrs, handle)?;
        Ok(block)
    }

    fn write_external_xattr_block(
        &mut self,
        block: FilesystemBlock,
        xattrs: &[Ext4Xattr],
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        if external_xattr_encoded_len(xattrs)? > block_size {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock));
        }
        let mut bytes = self.read_metadata_block(block)?.as_ref().to_vec();
        encode_external_xattr_block(
            xattrs,
            block.get(),
            self.superblock().checksum_seed(),
            self.superblock().features().has_metadata_checksum(),
            &mut bytes,
        )?;
        let access = self.metadata_io.write_access(block, handle)?;
        replace_metadata_access_bytes(&access, bytes)?;
        Ok(())
    }

    fn drop_external_xattr_block(
        &mut self,
        inode: &Ext4Inode,
        block: u64,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if !self.is_inode_physical_block_valid(inode.number(), block, 1) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
        }
        let buffer = self.read_metadata_block(FilesystemBlock::new(block))?;
        let header = disk_xattr::XattrBlockHeader::decode(buffer.as_ref())?;
        validate_xattr_block_header(header)?;
        self.verify_xattr_block_checksum(inode, block, buffer.as_ref(), header)?;
        if header.refcount() > 1 {
            let mut bytes = buffer.as_ref().to_vec();
            put_u32(&mut bytes, 0x04, header.refcount() - 1)?;
            update_xattr_block_checksum(
                block,
                self.superblock().checksum_seed(),
                self.superblock().features().has_metadata_checksum(),
                &mut bytes,
            )?;
            let access = self
                .metadata_io
                .write_access(FilesystemBlock::new(block), handle)?;
            replace_metadata_access_bytes(&access, bytes)?;
            return Ok(());
        }

        let physical = PhysicalBlock::new(block);
        if self.journal_supports_revoke() {
            self.release_allocated_metadata_block(physical, handle)?;
        } else {
            self.release_allocated_metadata_block_without_revoke(physical, handle)?;
        }
        Ok(())
    }

    pub(crate) fn release_external_xattr_block_for_eviction(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block = inode.file_acl_block();
        if block == 0 {
            return Ok(());
        }

        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let mut inode_table_bytes = self
            .read_metadata_block(inode_table_block)?
            .as_ref()
            .to_vec();
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| {
                let raw = disk_inode::RawInode::decode(inode_bytes)?;
                update_inode_xattr_block_bytes(
                    inode_bytes,
                    &raw,
                    InodeXattrBlockUpdate {
                        file_acl: 0,
                        current_blocks: inode.blocks(),
                        had_external_block: true,
                        has_external_block: false,
                        block_size: self.layout().block_size(),
                        has_64bit: self.superblock().features().has_64bit(),
                    },
                )
            },
        )?;
        self.drop_external_xattr_block(inode, block, handle)?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    fn read_inline_xattrs(&self, inode: &Ext4Inode, output: &mut Vec<Ext4Xattr>) -> Ext4Result<()> {
        output.extend(read_inline_xattrs_from_bytes(&inode.inline_xattr_bytes())?);
        Ok(())
    }

    fn read_external_xattrs(
        &self,
        inode: &Ext4Inode,
        output: &mut Vec<Ext4Xattr>,
    ) -> Ext4Result<()> {
        self.read_external_xattrs_with_refcount(inode, output)
            .map(|_| ())
    }

    fn read_external_xattrs_with_refcount(
        &self,
        inode: &Ext4Inode,
        output: &mut Vec<Ext4Xattr>,
    ) -> Ext4Result<Option<u32>> {
        let block = inode.file_acl_block();
        if block == 0 {
            return Ok(None);
        }
        if !self.is_inode_physical_block_valid(inode.number(), block, 1) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
        }
        let buffer = self.read_metadata_block(FilesystemBlock::new(block))?;
        let bytes = buffer.as_ref();
        let header = disk_xattr::XattrBlockHeader::decode(bytes)?;
        validate_xattr_block_header(header)?;
        self.verify_xattr_block_checksum(inode, block, bytes, header)?;
        collect_xattrs_from_region(
            bytes,
            disk_xattr::XATTR_HEADER_SIZE,
            0,
            XattrEntryOrder::RequireSorted,
            output,
        )?;
        Ok(Some(header.refcount()))
    }

    fn list_external_xattr_names(
        &self,
        inode: &Ext4Inode,
        sink: &mut dyn Ext4XattrNameSink,
    ) -> Ext4Result<()> {
        let block = inode.file_acl_block();
        if block == 0 {
            return Ok(());
        }
        if !self.is_inode_physical_block_valid(inode.number(), block, 1) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
        }
        let buffer = self.read_metadata_block(FilesystemBlock::new(block))?;
        let bytes = buffer.as_ref();
        let header = disk_xattr::XattrBlockHeader::decode(bytes)?;
        validate_xattr_block_header(header)?;
        self.verify_xattr_block_checksum(inode, block, bytes, header)?;
        emit_xattr_names_from_region(
            bytes,
            disk_xattr::XATTR_HEADER_SIZE,
            0,
            XattrEntryOrder::RequireSorted,
            sink,
        )
    }

    fn verify_xattr_block_checksum(
        &self,
        inode: &Ext4Inode,
        block: u64,
        bytes: &[u8],
        header: disk_xattr::XattrBlockHeader,
    ) -> Ext4Result<()> {
        if !self.superblock().features().has_metadata_checksum() {
            return Ok(());
        }
        let before_checksum = bytes
            .get(..disk_xattr::XATTR_BLOCK_CHECKSUM_OFFSET)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let after_checksum = bytes
            .get(disk_xattr::XATTR_BLOCK_CHECKSUM_OFFSET + 4..)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;

        let mut actual = checksum::crc32c(self.superblock().checksum_seed(), &block.to_le_bytes());
        actual = checksum::crc32c(actual, before_checksum);
        actual = checksum::crc32c(actual, &0u32.to_le_bytes());
        actual = checksum::crc32c(actual, after_checksum);
        let expected = header.checksum();
        if actual != expected {
            return Err(Ext4Error::ChecksumMismatch {
                target: ChecksumTarget::XattrBlock {
                    inode: inode.number().get(),
                    block,
                },
                expected,
                actual,
            });
        }
        Ok(())
    }
}

fn validate_settable_xattr(namespace: Ext4XattrNamespace, name: &[u8]) -> Ext4Result<()> {
    if name.len() > u8::MAX as usize || name.contains(&0) {
        return Err(Ext4Error::InvalidName);
    }
    match namespace {
        Ext4XattrNamespace::User | Ext4XattrNamespace::Trusted | Ext4XattrNamespace::Security => {
            if name.is_empty() {
                return Err(Ext4Error::InvalidName);
            }
        }
        Ext4XattrNamespace::PosixAclAccess | Ext4XattrNamespace::PosixAclDefault => {
            if !name.is_empty() {
                return Err(Ext4Error::InvalidName);
            }
        }
        _ => return Err(Ext4Error::InvalidName),
    }
    Ok(())
}

fn inline_xattr_offset(inode_len: usize, extra_isize: u16) -> Ext4Result<usize> {
    let offset = crate::disk::inode::GOOD_OLD_INODE_SIZE
        .checked_add(usize::from(extra_isize))
        .ok_or(Ext4Error::Overflow)?;
    if offset > inode_len {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
    }
    Ok(offset)
}

fn read_inline_xattrs_from_bytes(bytes: &[u8]) -> Ext4Result<Vec<Ext4Xattr>> {
    let mut xattrs = Vec::new();
    if bytes.len() < disk_xattr::XATTR_IBODY_HEADER_SIZE {
        return Ok(xattrs);
    }
    let magic = codec::le_u32(bytes, 0)?;
    if magic == 0 {
        return Ok(xattrs);
    }
    if magic != disk_xattr::XATTR_MAGIC {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
    }
    collect_xattrs_from_region(
        bytes,
        disk_xattr::XATTR_IBODY_HEADER_SIZE,
        disk_xattr::XATTR_IBODY_HEADER_SIZE,
        XattrEntryOrder::Unchecked,
        &mut xattrs,
    )?;
    Ok(xattrs)
}

fn list_inline_xattr_names_from_bytes(
    bytes: &[u8],
    sink: &mut dyn Ext4XattrNameSink,
) -> Ext4Result<()> {
    if bytes.len() < disk_xattr::XATTR_IBODY_HEADER_SIZE {
        return Ok(());
    }
    let magic = codec::le_u32(bytes, 0)?;
    if magic == 0 {
        return Ok(());
    }
    if magic != disk_xattr::XATTR_MAGIC {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
    }
    emit_xattr_names_from_region(
        bytes,
        disk_xattr::XATTR_IBODY_HEADER_SIZE,
        disk_xattr::XATTR_IBODY_HEADER_SIZE,
        XattrEntryOrder::Unchecked,
        sink,
    )
}

#[cfg(unittest)]
fn set_xattr_value(
    xattrs: &mut Vec<Ext4Xattr>,
    namespace: Ext4XattrNamespace,
    name: &[u8],
    value: &[u8],
) -> Ext4Result<()> {
    set_xattr_value_with_mode(
        xattrs,
        namespace,
        name,
        value,
        Ext4XattrSetMode::CreateOrReplace,
    )
    .map(|_| ())
}

fn set_xattr_value_with_mode(
    xattrs: &mut Vec<Ext4Xattr>,
    namespace: Ext4XattrNamespace,
    name: &[u8],
    value: &[u8],
    mode: Ext4XattrSetMode,
) -> Ext4Result<XattrMutation> {
    let existing = xattrs
        .iter()
        .find(|xattr| xattr.namespace == namespace && xattr.name == name);
    let exists = existing.is_some();
    let requires_create = matches!(
        mode,
        Ext4XattrSetMode::Create | Ext4XattrSetMode::CreateAndReplace
    );
    let requires_replace = matches!(
        mode,
        Ext4XattrSetMode::Replace | Ext4XattrSetMode::CreateAndReplace
    );
    if exists && requires_create {
        return Err(Ext4Error::AlreadyExists);
    }
    if !exists && requires_replace {
        return Err(Ext4Error::NotFound);
    }
    if existing.is_some_and(|xattr| xattr.value == value) {
        return Ok(XattrMutation::Unchanged);
    }
    xattrs.retain(|xattr| xattr.namespace != namespace || xattr.name != name);
    xattrs.push(Ext4Xattr {
        namespace,
        name: Vec::from(name),
        value: Vec::from(value),
    });
    Ok(XattrMutation::Changed)
}

#[cfg(unittest)]
fn remove_xattr_value(
    xattrs: &mut Vec<Ext4Xattr>,
    namespace: Ext4XattrNamespace,
    name: &[u8],
) -> Ext4Result<()> {
    let before = xattrs.len();
    xattrs.retain(|xattr| xattr.namespace != namespace || xattr.name != name);
    if xattrs.len() == before {
        return Err(Ext4Error::NotFound);
    }
    Ok(())
}

fn xattr_inline_layout(xattrs: &[Ext4Xattr]) -> XattrStorageLayout {
    if xattrs.is_empty() {
        XattrStorageLayout::Empty
    } else {
        XattrStorageLayout::Inline
    }
}

fn validate_nonshrinking_extra_isize(current: u16, planned: u16) -> Ext4Result<()> {
    if planned < current {
        return Err(Ext4Error::InvalidInodeState);
    }
    debug_assert!(planned >= current);
    Ok(())
}

fn extra_isize_layout_candidates(
    inode_size: usize,
    current_extra_isize: u16,
    wanted_extra_isize: u16,
    minimum_extra_isize: u16,
) -> Ext4Result<[Option<ExtraIsizeLayoutCandidate>; 3]> {
    let desired_extra_isize = current_extra_isize.max(wanted_extra_isize.max(minimum_extra_isize));
    let minimum_extra_isize = current_extra_isize.max(minimum_extra_isize);
    let extra_isizes = [
        desired_extra_isize,
        minimum_extra_isize,
        current_extra_isize,
    ];
    let mut candidates = [None; 3];

    for (index, extra_isize) in extra_isizes.into_iter().enumerate() {
        if extra_isizes[..index].contains(&extra_isize) {
            continue;
        }
        candidates[index] = Some(extra_isize_layout_candidate(inode_size, extra_isize)?);
    }

    Ok(candidates)
}

fn extra_isize_layout_candidate(
    inode_size: usize,
    extra_isize: u16,
) -> Ext4Result<ExtraIsizeLayoutCandidate> {
    let inline_capacity = inode_size
        .checked_sub(disk_inode::GOOD_OLD_INODE_SIZE)
        .and_then(|available| available.checked_sub(usize::from(extra_isize)))
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?;
    Ok(ExtraIsizeLayoutCandidate {
        extra_isize,
        inline_capacity,
    })
}

fn select_extra_isize_storage_plan(
    candidates: [Option<ExtraIsizeLayoutCandidate>; 3],
    mut plan_storage: impl FnMut(
        usize,
    )
        -> Ext4Result<Option<(XattrStoragePlan, ExternalXattrBlockAction)>>,
) -> Ext4Result<InodeXattrStoragePlan> {
    for candidate in candidates.into_iter().flatten() {
        let Some((storage, external_block_action)) = plan_storage(candidate.inline_capacity)?
        else {
            continue;
        };
        return Ok(InodeXattrStoragePlan {
            extra_isize: candidate.extra_isize,
            storage,
            external_block_action,
        });
    }

    Err(Ext4Error::NoSpace)
}

#[allow(clippy::too_many_arguments)]
fn plan_xattr_set_storage(
    inline_xattrs: &[Ext4Xattr],
    old_external_xattrs: &[Ext4Xattr],
    namespace: Ext4XattrNamespace,
    name: &[u8],
    value: &[u8],
    mode: Ext4XattrSetMode,
    extra_isize: u16,
    inline_capacity: usize,
    external_capacity: usize,
    old_layout: XattrStorageLayout,
    free_blocks_count: u64,
) -> Ext4Result<PlannedXattrMutation> {
    let occurrences = inline_xattrs
        .iter()
        .chain(old_external_xattrs)
        .filter(|xattr| xattr_matches(xattr, namespace, name))
        .count();
    if occurrences > 1 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
    }

    let mut updated_xattrs = Vec::from(inline_xattrs);
    updated_xattrs.extend(old_external_xattrs.iter().cloned());
    if set_xattr_value_with_mode(&mut updated_xattrs, namespace, name, value, mode)?
        == XattrMutation::Unchanged
    {
        return Ok(PlannedXattrMutation::Unchanged);
    }
    let updated_xattr = updated_xattrs
        .into_iter()
        .find(|xattr| xattr_matches(xattr, namespace, name))
        .ok_or(Ext4Error::InvalidInodeState)?;

    // Linux first tries the changed entry in the inode body while leaving all
    // unrelated entries in their current regions. Only that entry falls back
    // to the external block when the inode body cannot hold it.
    let mut inline = Vec::from(inline_xattrs);
    inline.retain(|xattr| !xattr_matches(xattr, namespace, name));
    inline.push(updated_xattr.clone());
    let mut external = Vec::from(old_external_xattrs);
    external.retain(|xattr| !xattr_matches(xattr, namespace, name));
    if let Some(plan) = finalize_xattr_storage_plan(
        extra_isize,
        inline,
        external,
        old_external_xattrs,
        inline_capacity,
        external_capacity,
        old_layout,
        free_blocks_count,
    )? {
        return Ok(PlannedXattrMutation::Changed(plan));
    }

    let mut inline = Vec::from(inline_xattrs);
    inline.retain(|xattr| !xattr_matches(xattr, namespace, name));
    let mut external = Vec::from(old_external_xattrs);
    external.retain(|xattr| !xattr_matches(xattr, namespace, name));
    external.push(updated_xattr);
    finalize_xattr_storage_plan(
        extra_isize,
        inline,
        external,
        old_external_xattrs,
        inline_capacity,
        external_capacity,
        old_layout,
        free_blocks_count,
    )?
    .map(PlannedXattrMutation::Changed)
    .ok_or(Ext4Error::NoSpace)
}

#[allow(clippy::too_many_arguments)]
fn plan_xattr_remove_storage(
    inline_xattrs: &[Ext4Xattr],
    old_external_xattrs: &[Ext4Xattr],
    namespace: Ext4XattrNamespace,
    name: &[u8],
    extra_isize: u16,
    inline_capacity: usize,
    external_capacity: usize,
    old_layout: XattrStorageLayout,
    free_blocks_count: u64,
) -> Ext4Result<InodeXattrStoragePlan> {
    let occurrences = inline_xattrs
        .iter()
        .chain(old_external_xattrs)
        .filter(|xattr| xattr_matches(xattr, namespace, name))
        .count();
    match occurrences {
        0 => return Err(Ext4Error::NotFound),
        1 => {}
        _ => return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr)),
    }

    let mut inline = Vec::from(inline_xattrs);
    inline.retain(|xattr| !xattr_matches(xattr, namespace, name));
    let mut external = Vec::from(old_external_xattrs);
    external.retain(|xattr| !xattr_matches(xattr, namespace, name));
    finalize_xattr_storage_plan(
        extra_isize,
        inline,
        external,
        old_external_xattrs,
        inline_capacity,
        external_capacity,
        old_layout,
        free_blocks_count,
    )?
    .ok_or(Ext4Error::NoSpace)
}

#[allow(clippy::too_many_arguments)]
fn finalize_xattr_storage_plan(
    extra_isize: u16,
    mut inline: Vec<Ext4Xattr>,
    mut external: Vec<Ext4Xattr>,
    old_external_xattrs: &[Ext4Xattr],
    inline_capacity: usize,
    external_capacity: usize,
    old_layout: XattrStorageLayout,
    free_blocks_count: u64,
) -> Ext4Result<Option<InodeXattrStoragePlan>> {
    sort_xattrs(&mut inline);
    sort_xattrs(&mut external);
    if inline_xattr_encoded_len(&inline)? > inline_capacity
        || (!external.is_empty() && external_xattr_encoded_len(&external)? > external_capacity)
    {
        return Ok(None);
    }
    let storage = XattrStoragePlan { inline, external };
    let rewrite_external = storage.external != old_external_xattrs;
    let external_block_action = external_xattr_block_action(old_layout, &storage, rewrite_external);
    if external_block_action == ExternalXattrBlockAction::Allocate && free_blocks_count == 0 {
        return Ok(None);
    }
    Ok(Some(InodeXattrStoragePlan {
        extra_isize,
        storage,
        external_block_action,
    }))
}

fn xattr_matches(xattr: &Ext4Xattr, namespace: Ext4XattrNamespace, name: &[u8]) -> bool {
    xattr.namespace == namespace && xattr.name == name
}

fn external_xattr_block_action(
    old_layout: XattrStorageLayout,
    storage: &XattrStoragePlan,
    rewrite_external: bool,
) -> ExternalXattrBlockAction {
    if storage.external.is_empty() {
        ExternalXattrBlockAction::None
    } else if !rewrite_external {
        ExternalXattrBlockAction::Reuse
    } else if old_layout == (XattrStorageLayout::External { shared: false }) {
        ExternalXattrBlockAction::Rewrite
    } else {
        ExternalXattrBlockAction::Allocate
    }
}

fn plan_xattr_expansion_storage(
    inline_xattrs: &[Ext4Xattr],
    external_xattrs: &[Ext4Xattr],
    inline_capacity: usize,
    external_capacity: usize,
) -> Ext4Result<Option<XattrStoragePlan>> {
    let mut inline_xattrs = Vec::from(inline_xattrs);
    let mut external_xattrs = Vec::from(external_xattrs);
    sort_xattrs(&mut inline_xattrs);
    sort_xattrs(&mut external_xattrs);
    if inline_xattr_encoded_len(&inline_xattrs)? <= inline_capacity {
        return Ok(Some(XattrStoragePlan {
            inline: inline_xattrs,
            external: external_xattrs,
        }));
    }

    while inline_xattr_encoded_len(&inline_xattrs)? > inline_capacity {
        let inline_used = inline_xattr_encoded_len(&inline_xattrs)?;
        let external_used = external_xattr_encoded_len(&external_xattrs)?;
        let Some(external_free) = external_capacity.checked_sub(external_used) else {
            return Ok(None);
        };
        let mut covering_entry = None;
        let mut fallback_entry = None;
        for (index, xattr) in inline_xattrs.iter().enumerate() {
            if !xattr_can_move_to_external(xattr) {
                continue;
            }
            let entry_size = xattr_encoded_entry_len(xattr)?;
            if entry_size > external_free {
                continue;
            }
            let remaining_inline = if inline_xattrs.len() == 1 {
                0
            } else {
                inline_used
                    .checked_sub(entry_size)
                    .ok_or(Ext4Error::InvalidInodeState)?
            };
            if remaining_inline <= inline_capacity {
                if covering_entry.is_none_or(|(_, best_size)| entry_size < best_size) {
                    covering_entry = Some((index, entry_size));
                }
            } else {
                // Match ext4_xattr_make_inode_space(): if no one entry can
                // finish the expansion, keep the last movable entry seen and
                // rescan after moving it.
                fallback_entry = Some((index, entry_size));
            }
        }
        let Some((entry_index, _)) = covering_entry.or(fallback_entry) else {
            return Ok(None);
        };
        external_xattrs.push(inline_xattrs.remove(entry_index));
    }
    sort_xattrs(&mut external_xattrs);
    debug_assert!(inline_xattr_encoded_len(&inline_xattrs)? <= inline_capacity);
    debug_assert!(external_xattr_encoded_len(&external_xattrs)? <= external_capacity);
    Ok(Some(XattrStoragePlan {
        inline: inline_xattrs,
        external: external_xattrs,
    }))
}

fn xattr_can_move_to_external(xattr: &Ext4Xattr) -> bool {
    xattr.namespace != Ext4XattrNamespace::System || xattr.name != b"data"
}

fn xattr_encoded_entry_len(xattr: &Ext4Xattr) -> Ext4Result<usize> {
    disk_xattr::entry_len(xattr.name.len())?
        .checked_add(disk_xattr::padded_len(xattr.value.len())?)
        .ok_or(Ext4Error::Overflow)
}

fn compare_xattr_names(left: &Ext4Xattr, right: &Ext4Xattr) -> core::cmp::Ordering {
    (
        left.namespace.index(),
        left.name.len(),
        left.name.as_slice(),
    )
        .cmp(&(
            right.namespace.index(),
            right.name.len(),
            right.name.as_slice(),
        ))
}

fn sort_xattrs(xattrs: &mut [Ext4Xattr]) {
    xattrs.sort_by(compare_xattr_names);
}

fn xattr_mutation_credit_count(
    old: XattrStorageLayout,
    new: XattrStorageLayout,
    external_block_action: ExternalXattrBlockAction,
) -> u32 {
    if external_block_action == ExternalXattrBlockAction::Reuse {
        return XATTR_INODE_UPDATE_CREDITS;
    }
    let mut credits = XATTR_INODE_UPDATE_CREDITS;
    match (old, new) {
        (XattrStorageLayout::External { shared: false }, XattrStorageLayout::External { .. }) => {
            credits = credits.saturating_add(XATTR_EXTERNAL_REWRITE_CREDITS);
        }
        (XattrStorageLayout::External { shared: true }, XattrStorageLayout::External { .. }) => {
            credits = credits
                .saturating_add(XATTR_EXTERNAL_ALLOC_CREDITS)
                .saturating_add(XATTR_EXTERNAL_SHARED_REFCOUNT_CREDITS);
        }
        (XattrStorageLayout::External { shared: false }, _) => {
            credits = credits.saturating_add(XATTR_EXTERNAL_RELEASE_CREDITS);
        }
        (XattrStorageLayout::External { shared: true }, _) => {
            credits = credits.saturating_add(XATTR_EXTERNAL_SHARED_REFCOUNT_CREDITS);
        }
        (_, XattrStorageLayout::External { .. }) => {
            credits = credits.saturating_add(XATTR_EXTERNAL_ALLOC_CREDITS);
        }
        _ => {}
    }
    credits
}

fn inline_xattr_encoded_len(xattrs: &[Ext4Xattr]) -> Ext4Result<usize> {
    if xattrs.is_empty() {
        return Ok(0);
    }
    let mut len = disk_xattr::XATTR_IBODY_HEADER_SIZE;
    for xattr in xattrs {
        len = len
            .checked_add(xattr_encoded_entry_len(xattr)?)
            .ok_or(Ext4Error::Overflow)?;
    }
    len.checked_add(4).ok_or(Ext4Error::Overflow)
}

fn external_xattr_encoded_len(xattrs: &[Ext4Xattr]) -> Ext4Result<usize> {
    let mut len = disk_xattr::XATTR_HEADER_SIZE;
    for xattr in xattrs {
        len = len
            .checked_add(xattr_encoded_entry_len(xattr)?)
            .ok_or(Ext4Error::Overflow)?;
    }
    len.checked_add(4).ok_or(Ext4Error::Overflow)
}

fn encode_inline_xattrs(xattrs: &[Ext4Xattr], output: &mut [u8]) -> Ext4Result<()> {
    output.fill(0);
    if xattrs.is_empty() {
        return Ok(());
    }
    if output.len() < disk_xattr::XATTR_IBODY_HEADER_SIZE {
        return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock));
    }

    put_u32(output, 0, disk_xattr::XATTR_MAGIC)?;
    encode_xattr_entries(
        xattrs,
        output,
        disk_xattr::XATTR_IBODY_HEADER_SIZE,
        disk_xattr::XATTR_IBODY_HEADER_SIZE,
        XattrEntryHashWriter::Inline,
    )
}

fn encode_external_xattr_block(
    xattrs: &[Ext4Xattr],
    block: u64,
    checksum_seed: u32,
    has_metadata_checksum: bool,
    output: &mut [u8],
) -> Ext4Result<()> {
    output.fill(0);
    if xattrs.is_empty() || output.len() < disk_xattr::XATTR_HEADER_SIZE {
        return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock));
    }
    put_u32(output, 0x00, disk_xattr::XATTR_MAGIC)?;
    // KExt4 writes new external xattr blocks as private blocks. Linux-created
    // shared blocks are handled by COW/decrement during mutation.
    put_u32(output, 0x04, 1)?;
    put_u32(output, 0x08, 1)?;
    let mut block_hasher = ExternalXattrBlockHasher::new();
    encode_xattr_entries(
        xattrs,
        output,
        disk_xattr::XATTR_HEADER_SIZE,
        0,
        XattrEntryHashWriter::ExternalBlock(&mut block_hasher),
    )?;
    put_u32(output, 0x0c, block_hasher.finish())?;
    update_xattr_block_checksum(block, checksum_seed, has_metadata_checksum, output)
}

enum XattrEntryHashWriter<'a> {
    Inline,
    ExternalBlock(&'a mut ExternalXattrBlockHasher),
}

impl XattrEntryHashWriter<'_> {
    fn write_hash(&mut self, xattr: &Ext4Xattr) -> u32 {
        match self {
            Self::Inline => 0,
            Self::ExternalBlock(block_hasher) => {
                let entry_hash = xattr_entry_hash(&xattr.name, &xattr.value);
                block_hasher.update(entry_hash);
                entry_hash
            }
        }
    }
}

/// Accumulates the ext4 external xattr block hash stored in `h_hash`.
///
/// This follows Linux `ext4_xattr_rehash()`. An entry hash of zero marks the
/// block as unshareable, so Linux sets the complete block hash to zero:
/// <https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/ext4/xattr.c>
struct ExternalXattrBlockHasher {
    hash: Option<u32>,
}

impl ExternalXattrBlockHasher {
    const fn new() -> Self {
        Self { hash: Some(0) }
    }

    fn update(&mut self, entry_hash: u32) {
        if entry_hash == 0 {
            self.hash = None;
        } else if let Some(block_hash) = &mut self.hash {
            *block_hash = block_hash.rotate_left(16) ^ entry_hash;
        }
    }

    fn finish(self) -> u32 {
        self.hash.unwrap_or(0)
    }
}

fn encode_xattr_entries(
    xattrs: &[Ext4Xattr],
    output: &mut [u8],
    entries_offset: usize,
    value_base: usize,
    mut hash_writer: XattrEntryHashWriter<'_>,
) -> Ext4Result<()> {
    let mut sorted = Vec::from(xattrs);
    sort_xattrs(&mut sorted);

    let mut entry_offset = entries_offset;
    let mut value_cursor = output.len();
    for xattr in &sorted {
        let entry_len = disk_xattr::entry_len(xattr.name.len())?;
        let next_entry = entry_offset
            .checked_add(entry_len)
            .ok_or(Ext4Error::Overflow)?;
        let (value_offset, value_size) = if xattr.value.is_empty() {
            (0, 0)
        } else {
            let padded_value_len = disk_xattr::padded_len(xattr.value.len())?;
            value_cursor = value_cursor
                .checked_sub(padded_value_len)
                .ok_or(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock))?;
            output
                .get_mut(value_cursor..value_cursor + xattr.value.len())
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
                .copy_from_slice(&xattr.value);
            let relative = value_cursor
                .checked_sub(value_base)
                .ok_or(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock))?;
            (
                u16::try_from(relative).map_err(|_| Ext4Error::Overflow)?,
                u32::try_from(xattr.value.len()).map_err(|_| Ext4Error::Overflow)?,
            )
        };
        let marker_end = next_entry.checked_add(4).ok_or(Ext4Error::Overflow)?;
        if marker_end > value_cursor {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock));
        }
        let entry_hash = hash_writer.write_hash(xattr);
        encode_xattr_entry(
            output,
            entry_offset,
            xattr,
            value_offset,
            value_size,
            entry_hash,
        )?;
        entry_offset = next_entry;
    }
    Ok(())
}

/// Computes the ext4 external xattr entry hash stored in `e_hash`.
///
/// This follows Linux `ext4_xattr_hash_entry()`. Values are padded to a
/// four-byte boundary and folded as little-endian words:
/// <https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/ext4/xattr.c>
fn xattr_entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let mut hash = 0u32;
    for &byte in name {
        hash = hash.rotate_left(5) ^ u32::from(byte);
    }
    for chunk in value.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        hash = hash.rotate_left(16) ^ u32::from_le_bytes(word);
    }
    hash
}

fn update_xattr_block_checksum(
    block: u64,
    checksum_seed: u32,
    has_metadata_checksum: bool,
    output: &mut [u8],
) -> Ext4Result<()> {
    if !has_metadata_checksum {
        put_u32(output, disk_xattr::XATTR_BLOCK_CHECKSUM_OFFSET, 0)?;
        return Ok(());
    }
    put_u32(output, disk_xattr::XATTR_BLOCK_CHECKSUM_OFFSET, 0)?;
    let checksum = xattr_block_checksum(block, checksum_seed, output)?;
    put_u32(output, disk_xattr::XATTR_BLOCK_CHECKSUM_OFFSET, checksum)
}

fn xattr_block_checksum(block: u64, checksum_seed: u32, bytes: &[u8]) -> Ext4Result<u32> {
    let before_checksum = bytes
        .get(..disk_xattr::XATTR_BLOCK_CHECKSUM_OFFSET)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    let after_checksum = bytes
        .get(disk_xattr::XATTR_BLOCK_CHECKSUM_OFFSET + 4..)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;

    let mut actual = checksum::crc32c(checksum_seed, &block.to_le_bytes());
    actual = checksum::crc32c(actual, before_checksum);
    actual = checksum::crc32c(actual, &0u32.to_le_bytes());
    actual = checksum::crc32c(actual, after_checksum);
    Ok(actual)
}

struct InodeXattrBlockUpdate {
    file_acl: u64,
    current_blocks: u64,
    had_external_block: bool,
    has_external_block: bool,
    block_size: u32,
    has_64bit: bool,
}

fn update_inode_xattr_block_bytes(
    inode_bytes: &mut [u8],
    raw: &disk_inode::RawInode,
    update: InodeXattrBlockUpdate,
) -> Ext4Result<()> {
    if update.file_acl > u64::from(u32::MAX) && !update.has_64bit {
        return Err(Ext4Error::Overflow);
    }
    if update.file_acl >> 48 != 0 {
        return Err(Ext4Error::Overflow);
    }
    put_u32(inode_bytes, 0x68, update.file_acl as u32)?;
    put_u16(inode_bytes, 0x70, (update.file_acl >> 32) as u16)?;

    let block_sectors = u64::from(update.block_size) / 512;
    let blocks = match (update.had_external_block, update.has_external_block) {
        (false, true) => {
            if raw.flags() & disk_inode::EXT4_HUGE_FILE_FL != 0 {
                return Err(Ext4Error::Unsupported(UnsupportedKind::HugeFile));
            }
            update
                .current_blocks
                .checked_add(block_sectors)
                .ok_or(Ext4Error::Overflow)?
        }
        (true, false) => {
            if raw.flags() & disk_inode::EXT4_HUGE_FILE_FL != 0 {
                return Err(Ext4Error::Unsupported(UnsupportedKind::HugeFile));
            }
            update
                .current_blocks
                .checked_sub(block_sectors)
                .ok_or(Ext4Error::Overflow)?
        }
        _ => update.current_blocks,
    };
    put_u32(inode_bytes, disk_inode::BLOCKS_LO_OFFSET, blocks as u32)?;
    put_u16(
        inode_bytes,
        disk_inode::BLOCKS_HI_OFFSET,
        (blocks >> 32) as u16,
    )
}

fn encode_xattr_entry(
    output: &mut [u8],
    offset: usize,
    xattr: &Ext4Xattr,
    value_offset: u16,
    value_size: u32,
    entry_hash: u32,
) -> Ext4Result<()> {
    *output
        .get_mut(offset)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))? =
        u8::try_from(xattr.name.len()).map_err(|_| Ext4Error::InvalidName)?;
    let index_offset = offset.checked_add(1).ok_or(Ext4Error::Overflow)?;
    *output
        .get_mut(index_offset)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))? = xattr.namespace.index();
    put_u16(
        output,
        offset.checked_add(0x02).ok_or(Ext4Error::Overflow)?,
        value_offset,
    )?;
    put_u32(
        output,
        offset.checked_add(0x04).ok_or(Ext4Error::Overflow)?,
        0,
    )?;
    put_u32(
        output,
        offset.checked_add(0x08).ok_or(Ext4Error::Overflow)?,
        value_size,
    )?;
    put_u32(
        output,
        offset.checked_add(0x0c).ok_or(Ext4Error::Overflow)?,
        entry_hash,
    )?;
    let name_start = offset
        .checked_add(disk_xattr::XATTR_ENTRY_HEADER_SIZE)
        .ok_or(Ext4Error::Overflow)?;
    let name_end = name_start
        .checked_add(xattr.name.len())
        .ok_or(Ext4Error::Overflow)?;
    output
        .get_mut(name_start..name_end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        .copy_from_slice(&xattr.name);
    Ok(())
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) -> Ext4Result<()> {
    let end = offset.checked_add(2).ok_or(Ext4Error::Overflow)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) -> Ext4Result<()> {
    let end = offset.checked_add(4).ok_or(Ext4Error::Overflow)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn validate_xattr_block_header(header: disk_xattr::XattrBlockHeader) -> Ext4Result<()> {
    // Linux treats h_reserved as opaque. When metadata checksums are enabled,
    // their integrity is covered by the checksum verified after this check.
    if header.magic() != disk_xattr::XATTR_MAGIC
        || header.refcount() == 0
        || header.refcount() > disk_xattr::XATTR_REFCOUNT_MAX
        || header.blocks() != 1
    {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
    }
    Ok(())
}

fn collect_xattrs_from_region(
    bytes: &[u8],
    entries_offset: usize,
    value_base: usize,
    entry_order: XattrEntryOrder,
    output: &mut Vec<Ext4Xattr>,
) -> Ext4Result<()> {
    let mut entries = Vec::new();
    let entries_end = decode_xattr_entries(bytes, entries_offset, entry_order, &mut entries)?;
    let mut value_ranges = Vec::new();
    for entry in entries {
        let value = if entry.value_size == 0 {
            Vec::new()
        } else {
            let value_start = value_base
                .checked_add(entry.value_offset)
                .ok_or(Ext4Error::Overflow)?;
            let value_end = value_start
                .checked_add(entry.value_size)
                .ok_or(Ext4Error::Overflow)?;
            if value_start < entries_end || value_end > bytes.len() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
            }
            value_ranges.push((value_start, value_end));
            Vec::from(
                bytes
                    .get(value_start..value_end)
                    .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?,
            )
        };
        output.push(Ext4Xattr {
            namespace: entry.namespace,
            name: Vec::from(entry.name),
            value,
        });
    }
    validate_non_overlapping_value_ranges(&mut value_ranges)?;
    Ok(())
}

fn emit_xattr_names_from_region(
    bytes: &[u8],
    entries_offset: usize,
    value_base: usize,
    entry_order: XattrEntryOrder,
    sink: &mut dyn Ext4XattrNameSink,
) -> Ext4Result<()> {
    let mut entries = Vec::new();
    let entries_end = decode_xattr_entries(bytes, entries_offset, entry_order, &mut entries)?;
    let mut value_ranges = Vec::new();
    for entry in &entries {
        if entry.value_size == 0 {
            continue;
        }
        let value_start = value_base
            .checked_add(entry.value_offset)
            .ok_or(Ext4Error::Overflow)?;
        let value_end = value_start
            .checked_add(entry.value_size)
            .ok_or(Ext4Error::Overflow)?;
        if value_start < entries_end || value_end > bytes.len() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
        }
        value_ranges.push((value_start, value_end));
    }
    validate_non_overlapping_value_ranges(&mut value_ranges)?;

    for entry in entries {
        sink.emit(Ext4XattrNameRef {
            namespace: entry.namespace,
            name: entry.name,
        })?;
    }
    Ok(())
}

fn decode_xattr_entries<'a>(
    bytes: &'a [u8],
    entries_offset: usize,
    entry_order: XattrEntryOrder,
    output: &mut Vec<ParsedXattrEntry<'a>>,
) -> Ext4Result<usize> {
    let mut offset = entries_offset;
    let mut previous_sort_key: Option<(u8, u8, &[u8])> = None;
    loop {
        let marker_end = offset.checked_add(4).ok_or(Ext4Error::Overflow)?;
        let marker = codec::le_u32(bytes, offset)?;
        if marker == 0 {
            return Ok(marker_end);
        }

        let header = disk_xattr::XattrEntryHeader::decode(bytes, offset)?;
        if header.value_inum() != 0 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrInode));
        }
        if header.name_index() == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
        }

        let name_len = usize::from(header.name_len());
        let name_start = offset
            .checked_add(disk_xattr::XATTR_ENTRY_HEADER_SIZE)
            .ok_or(Ext4Error::Overflow)?;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or(Ext4Error::Overflow)?;
        let entry_len = disk_xattr::entry_len(name_len)?;
        let next = offset.checked_add(entry_len).ok_or(Ext4Error::Overflow)?;
        if next > bytes.len() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
        }
        let name = bytes
            .get(name_start..name_end)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        if name.contains(&0) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
        }

        if entry_order == XattrEntryOrder::RequireSorted {
            let sort_key = (header.name_index(), header.name_len(), name);
            if previous_sort_key.is_some_and(|previous| previous > sort_key) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
            }
            previous_sort_key = Some(sort_key);
        }

        output.push(ParsedXattrEntry {
            namespace: Ext4XattrNamespace::from_index(header.name_index()),
            name,
            value_offset: usize::from(header.value_offs()),
            value_size: usize::try_from(header.value_size()).map_err(|_| Ext4Error::Overflow)?,
        });
        offset = next;
    }
}

fn validate_non_overlapping_value_ranges(ranges: &mut [(usize, usize)]) -> Ext4Result<()> {
    ranges.sort_unstable();
    for window in ranges.windows(2) {
        let previous = window[0];
        let current = window[1];
        if previous == current {
            continue;
        }
        if previous.1 > current.0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
        }
    }
    Ok(())
}

unittest::path_mod_test!(tests, "../ut-tests/xattr/tests.rs");

unittest::path_mod_test!(/// QEMU unittest coverage for xattr layout decisions used by metadata writes.
 mod unittests, "../ut-tests/xattr/unittests.rs");

#[cfg(unittest)]
unittest::path_mod_test!(component_tests, "../ut-tests/xattr/component_tests.rs");
