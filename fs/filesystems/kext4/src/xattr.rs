// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use crate::{
    ChecksumTarget, CorruptKind, Ext4Error, Ext4Filesystem, Ext4Result, FilesystemBlock,
    PhysicalBlock, UnsupportedKind,
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
/// Disk-format and I/O failures are reported by [`Ext4Filesystem::list_xattrs`].
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

impl Ext4Filesystem {
    /// Reads all supported extended attributes stored on an inode.
    pub fn read_xattrs(&self, inode: &Ext4Inode) -> Ext4Result<Vec<Ext4Xattr>> {
        let mut xattrs = Vec::new();
        self.read_inline_xattrs(inode, &mut xattrs)?;
        self.read_external_xattrs(inode, &mut xattrs)?;
        Ok(xattrs)
    }

    /// Walks supported extended-attribute names without materializing values.
    ///
    /// Names borrow from validated inode snapshots or external-block metadata
    /// and remain valid only for the duration of each sink call.
    pub fn list_xattrs(
        &self,
        inode: &Ext4Inode,
        sink: &mut dyn Ext4XattrNameSink,
    ) -> Ext4Result<()> {
        let inline_xattrs = inode.inline_xattr_bytes();
        list_inline_xattr_names_from_bytes(&inline_xattrs, sink)?;
        self.list_external_xattr_names(inode, sink)
    }

    /// Reads one extended attribute by namespace and raw name suffix.
    pub fn get_xattr(
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
    /// namespaces, plus opaque POSIX ACL xattr storage. The updated xattr set is
    /// kept in the inode body when it fits, otherwise a single external xattr
    /// block is created or replaced with refcount/checksum maintenance.
    pub fn set_xattr(
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
    pub fn set_xattr_with_mode(
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
        let (credits, xattrs, mutation) = self.xattr_mutation_plan(inode, timestamp, |xattrs| {
            set_xattr_value_with_mode(xattrs, namespace, name, value, mode)
        })?;
        if mutation == XattrMutation::Unchanged {
            return Ok(());
        }
        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )?;
        let mut handle = journal.begin(credits)?;
        let result = self.update_xattr_in_transaction(inode, timestamp, &mut handle, xattrs);
        self.complete_metadata_mutation(handle, result)
    }

    /// Removes an extended attribute.
    pub fn remove_xattr(
        &mut self,
        inode: &Ext4Inode,
        namespace: Ext4XattrNamespace,
        name: &[u8],
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        validate_settable_xattr(namespace, name)?;
        self.validate_inode_timestamp_update(inode, timestamp)?;
        let (credits, xattrs, _) = self.xattr_mutation_plan(inode, timestamp, |xattrs| {
            remove_xattr_value(xattrs, namespace, name).map(|_| XattrMutation::Changed)
        })?;
        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )?;
        let mut handle = journal.begin(credits)?;
        let result = self.update_xattr_in_transaction(inode, timestamp, &mut handle, xattrs);
        self.complete_metadata_mutation(handle, result)
    }

    fn update_xattr_in_transaction(
        &mut self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut JournalHandle<'_>,
        xattrs: Vec<Ext4Xattr>,
    ) -> Ext4Result<()> {
        let old_external_block = inode.file_acl_block();

        let inline_capacity = inode.inline_xattr_bytes().len();
        let needs_external_block =
            !xattrs.is_empty() && inline_xattr_encoded_len(&xattrs)? > inline_capacity;
        let new_external_block = if !needs_external_block {
            None
        } else if old_external_block != 0
            && self.external_xattr_block_refcount(inode, old_external_block)? == 1
        {
            let block = FilesystemBlock::new(old_external_block);
            self.write_external_xattr_block(inode, block, &xattrs, handle)?;
            Some(block)
        } else {
            Some(self.create_external_xattr_block(inode, &xattrs, handle)?)
        };
        if old_external_block != 0
            && new_external_block.is_none_or(|block| block.get() != old_external_block)
        {
            self.drop_external_xattr_block(inode, old_external_block, handle)?;
        }

        let (inode_table_block, inode_table_bytes, updated_inode) =
            self.prepare_xattr_inode_update(inode, &xattrs, new_external_block, timestamp)?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        self.publish_inode_metadata(inode, updated_inode)
    }

    fn xattr_mutation_plan(
        &self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        update: impl FnOnce(&mut Vec<Ext4Xattr>) -> Ext4Result<XattrMutation>,
    ) -> Ext4Result<(JournalCredits, Vec<Ext4Xattr>, XattrMutation)> {
        let old_external_block = inode.file_acl_block();
        let mut xattrs = self.read_xattrs(inode)?;
        let old_inline_layout = (old_external_block == 0).then(|| xattr_inline_layout(&xattrs));
        let mutation = update(&mut xattrs)?;
        if mutation == XattrMutation::Unchanged {
            return Ok((JournalCredits::new(0), xattrs, mutation));
        }
        let old_layout = if let Some(layout) = old_inline_layout {
            layout
        } else {
            let refcount = self.external_xattr_block_refcount(inode, old_external_block)?;
            XattrStorageLayout::External {
                shared: refcount > 1,
            }
        };
        let new_layout = xattr_layout_after_update(&xattrs, inode.inline_xattr_bytes().len())?;
        if matches!(new_layout, XattrStorageLayout::External { .. }) {
            let block_size =
                usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
            if external_xattr_encoded_len(&xattrs)? > block_size {
                return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock));
            }
            let reuses_existing =
                matches!(old_layout, XattrStorageLayout::External { shared: false });
            if !reuses_existing && self.superblock().free_blocks_count() == 0 {
                return Err(Ext4Error::NoSpace);
            }
        }
        let planned_external_block = matches!(new_layout, XattrStorageLayout::External { .. })
            .then_some(FilesystemBlock::new(old_external_block.max(1)));
        self.prepare_xattr_inode_update(inode, &xattrs, planned_external_block, timestamp)?;
        Ok((
            JournalCredits::new(xattr_mutation_credit_count(old_layout, new_layout)),
            xattrs,
            mutation,
        ))
    }

    fn prepare_xattr_inode_update(
        &self,
        inode: &Ext4Inode,
        xattrs: &[Ext4Xattr],
        new_external_block: Option<FilesystemBlock>,
        timestamp: Ext4Timestamp,
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
                let inline_xattr_offset =
                    inline_xattr_offset(inode_bytes.len(), raw.extra_isize())?;
                let inline_xattr_bytes = inode_bytes
                    .get_mut(inline_xattr_offset..)
                    .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
                if let Some(block) = new_external_block {
                    inline_xattr_bytes.fill(0);
                    update_inode_xattr_block_bytes(
                        inode_bytes,
                        &raw,
                        InodeXattrBlockUpdate {
                            file_acl: block.get(),
                            current_blocks: inode.blocks(),
                            had_external_block: old_external_block != 0,
                            has_external_block: true,
                            block_size: self.layout().block_size(),
                            has_64bit: self.superblock().features().has_64bit(),
                        },
                    )?;
                } else {
                    encode_inline_xattrs(xattrs, inline_xattr_bytes)?;
                    update_inode_xattr_block_bytes(
                        inode_bytes,
                        &raw,
                        InodeXattrBlockUpdate {
                            file_acl: 0,
                            current_blocks: inode.blocks(),
                            had_external_block: old_external_block != 0,
                            has_external_block: false,
                            block_size: self.layout().block_size(),
                            has_64bit: self.superblock().features().has_64bit(),
                        },
                    )?;
                }
                update_inode_ctime_bytes(inode_bytes, timestamp)
            },
        )?;
        Ok((inode_table_block, inode_table_bytes, updated_inode))
    }

    fn create_external_xattr_block(
        &mut self,
        inode: &Ext4Inode,
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
        self.add_system_zone(block.get(), 1, Some(inode.number()))?;
        self.write_external_xattr_block(inode, block, xattrs, handle)?;
        Ok(block)
    }

    fn write_external_xattr_block(
        &mut self,
        inode: &Ext4Inode,
        block: FilesystemBlock,
        xattrs: &[Ext4Xattr],
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        if external_xattr_encoded_len(xattrs)? > block_size {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock));
        }
        if !self.is_inode_owned_system_zone_block(block, inode.number()) {
            self.add_system_zone(block.get(), 1, Some(inode.number()))?;
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

    fn external_xattr_block_refcount(&self, inode: &Ext4Inode, block: u64) -> Ext4Result<u32> {
        if !self.is_inode_physical_block_valid(inode.number(), block, 1) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr));
        }
        let buffer = self.read_metadata_block(FilesystemBlock::new(block))?;
        let header = disk_xattr::XattrBlockHeader::decode(buffer.as_ref())?;
        validate_xattr_block_header(header)?;
        self.verify_xattr_block_checksum(inode, block, buffer.as_ref(), header)?;
        Ok(header.refcount())
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
        if self.is_inode_owned_system_zone_block(FilesystemBlock::new(block), inode.number()) {
            if self.journal_supports_revoke() {
                self.release_inode_metadata_block(inode.number(), physical, handle)?;
            } else {
                self.release_inode_metadata_block_without_revoke(inode.number(), physical, handle)?;
            }
        } else if self.journal_supports_revoke() {
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
        collect_xattrs_from_region(
            bytes,
            disk_xattr::XATTR_HEADER_SIZE,
            0,
            XattrEntryOrder::RequireSorted,
            output,
        )
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

#[cfg(test)]
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

fn xattr_layout_after_update(
    xattrs: &[Ext4Xattr],
    inline_capacity: usize,
) -> Ext4Result<XattrStorageLayout> {
    if xattrs.is_empty() {
        return Ok(XattrStorageLayout::Empty);
    }
    if inline_xattr_encoded_len(xattrs)? <= inline_capacity {
        Ok(XattrStorageLayout::Inline)
    } else {
        Ok(XattrStorageLayout::External { shared: false })
    }
}

fn xattr_mutation_credit_count(old: XattrStorageLayout, new: XattrStorageLayout) -> u32 {
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
            .checked_add(disk_xattr::entry_len(xattr.name.len())?)
            .ok_or(Ext4Error::Overflow)?;
        len = len
            .checked_add(disk_xattr::padded_len(xattr.value.len())?)
            .ok_or(Ext4Error::Overflow)?;
    }
    len.checked_add(4).ok_or(Ext4Error::Overflow)
}

fn external_xattr_encoded_len(xattrs: &[Ext4Xattr]) -> Ext4Result<usize> {
    let mut len = disk_xattr::XATTR_HEADER_SIZE;
    for xattr in xattrs {
        len = len
            .checked_add(disk_xattr::entry_len(xattr.name.len())?)
            .ok_or(Ext4Error::Overflow)?;
        len = len
            .checked_add(disk_xattr::padded_len(xattr.value.len())?)
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
    encode_xattr_entries(xattrs, output, disk_xattr::XATTR_HEADER_SIZE, 0)?;
    update_xattr_block_checksum(block, checksum_seed, has_metadata_checksum, output)
}

fn encode_xattr_entries(
    xattrs: &[Ext4Xattr],
    output: &mut [u8],
    entries_offset: usize,
    value_base: usize,
) -> Ext4Result<()> {
    let mut sorted = Vec::from(xattrs);
    sorted.sort_by(|left, right| {
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
    });

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
        encode_xattr_entry(output, entry_offset, xattr, value_offset, value_size)?;
        entry_offset = next_entry;
    }
    Ok(())
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
        0,
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
    if header.magic() != disk_xattr::XATTR_MAGIC
        || header.refcount() == 0
        || header.refcount() > disk_xattr::XATTR_REFCOUNT_MAX
        || header.blocks() != 1
        || header.reserved() != [0; 3]
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

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    #[derive(Default)]
    struct NameCollector(Vec<(Ext4XattrNamespace, Vec<u8>)>);

    impl Ext4XattrNameSink for NameCollector {
        fn emit(&mut self, name: Ext4XattrNameRef<'_>) -> Ext4Result<()> {
            self.0
                .push((name.namespace(), Vec::from(name.name_bytes())));
            Ok(())
        }
    }

    fn put_xattr_entry(
        bytes: &mut [u8],
        offset: usize,
        name_index: u8,
        name: &[u8],
        value_offs: u16,
        value_size: u32,
    ) -> usize {
        bytes[offset] = u8::try_from(name.len()).unwrap();
        bytes[offset + 1] = name_index;
        bytes[offset + 2..offset + 4].copy_from_slice(&value_offs.to_le_bytes());
        bytes[offset + 4..offset + 8].copy_from_slice(&0u32.to_le_bytes());
        bytes[offset + 8..offset + 12].copy_from_slice(&value_size.to_le_bytes());
        bytes[offset + 12..offset + 16].copy_from_slice(&0u32.to_le_bytes());
        let name_start = offset + disk_xattr::XATTR_ENTRY_HEADER_SIZE;
        let name_end = name_start + name.len();
        bytes[name_start..name_end].copy_from_slice(name);
        offset + disk_xattr::entry_len(name.len()).unwrap()
    }

    #[test]
    fn xattr_value_ranges_allow_shared_value() {
        let mut ranges = [(32, 48), (32, 48), (48, 52)];

        validate_non_overlapping_value_ranges(&mut ranges).unwrap();
    }

    #[test]
    fn xattr_value_ranges_reject_partial_overlap() {
        let mut ranges = [(32, 48), (40, 52)];

        assert_eq!(
            validate_non_overlapping_value_ranges(&mut ranges),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr))
        );
    }

    #[test]
    fn external_xattr_entries_must_be_sorted() {
        let mut bytes = vec![0; 64];
        let next = put_xattr_entry(&mut bytes, 0, 1, b"z", 0, 0);
        put_xattr_entry(&mut bytes, next, 1, b"a", 0, 0);

        assert_eq!(
            decode_xattr_entries(&bytes, 0, XattrEntryOrder::RequireSorted, &mut Vec::new()),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr))
        );
        decode_xattr_entries(&bytes, 0, XattrEntryOrder::Unchecked, &mut Vec::new()).unwrap();
    }

    #[test]
    fn xattr_entry_names_reject_embedded_nul() {
        let mut bytes = vec![0; 64];
        put_xattr_entry(&mut bytes, 0, 1, b"a\0b", 0, 0);

        assert_eq!(
            decode_xattr_entries(&bytes, 0, XattrEntryOrder::Unchecked, &mut Vec::new()),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr))
        );
    }

    #[test]
    fn inline_xattr_set_replace_and_remove_round_trips() {
        let mut bytes = vec![0; 128];
        let mut xattrs = Vec::new();

        set_xattr_value(&mut xattrs, Ext4XattrNamespace::User, b"beta", b"two")
            .expect("insert first xattr");
        set_xattr_value(&mut xattrs, Ext4XattrNamespace::Security, b"alpha", b"one")
            .expect("insert second xattr");
        set_xattr_value(&mut xattrs, Ext4XattrNamespace::User, b"beta", b"replaced")
            .expect("replace first xattr");
        encode_inline_xattrs(&xattrs, &mut bytes).expect("encode inline xattrs");

        let decoded = read_inline_xattrs_from_bytes(&bytes).expect("decode inline xattrs");
        assert_eq!(decoded.len(), 2);
        assert!(decoded.iter().any(|xattr| {
            xattr.namespace() == Ext4XattrNamespace::Security
                && xattr.name_bytes() == b"alpha"
                && xattr.value() == b"one"
        }));
        assert!(decoded.iter().any(|xattr| {
            xattr.namespace() == Ext4XattrNamespace::User
                && xattr.name_bytes() == b"beta"
                && xattr.value() == b"replaced"
        }));

        let mut decoded = decoded;
        remove_xattr_value(&mut decoded, Ext4XattrNamespace::User, b"beta")
            .expect("remove user xattr");
        encode_inline_xattrs(&decoded, &mut bytes).expect("encode after remove");
        let decoded = read_inline_xattrs_from_bytes(&bytes).expect("decode after remove");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].namespace(), Ext4XattrNamespace::Security);
        assert_eq!(decoded[0].name_bytes(), b"alpha");
    }

    #[test]
    fn inline_xattr_listing_emits_only_borrowed_names() {
        let mut bytes = vec![0; 128];
        let xattrs = vec![
            Ext4Xattr {
                namespace: Ext4XattrNamespace::User,
                name: Vec::from(&b"alpha"[..]),
                value: Vec::from(&b"large-value-is-not-returned"[..]),
            },
            Ext4Xattr {
                namespace: Ext4XattrNamespace::Security,
                name: Vec::from(&b"beta"[..]),
                value: Vec::new(),
            },
        ];
        encode_inline_xattrs(&xattrs, &mut bytes).expect("encode inline xattrs");
        let mut names = NameCollector::default();

        list_inline_xattr_names_from_bytes(&bytes, &mut names).expect("list inline xattrs");

        assert_eq!(
            names.0,
            vec![
                (Ext4XattrNamespace::User, Vec::from(&b"alpha"[..])),
                (Ext4XattrNamespace::Security, Vec::from(&b"beta"[..])),
            ]
        );
    }

    #[test]
    fn xattr_set_mode_enforces_create_and_replace_atomically() {
        let mut xattrs = Vec::new();
        set_xattr_value_with_mode(
            &mut xattrs,
            Ext4XattrNamespace::User,
            b"key",
            b"initial",
            Ext4XattrSetMode::Create,
        )
        .expect("create missing xattr");
        assert_eq!(
            set_xattr_value_with_mode(
                &mut xattrs,
                Ext4XattrNamespace::User,
                b"key",
                b"duplicate",
                Ext4XattrSetMode::Create,
            ),
            Err(Ext4Error::AlreadyExists)
        );
        assert_eq!(xattrs[0].value(), b"initial");

        set_xattr_value_with_mode(
            &mut xattrs,
            Ext4XattrNamespace::User,
            b"key",
            b"replacement",
            Ext4XattrSetMode::Replace,
        )
        .expect("replace existing xattr");
        assert_eq!(xattrs[0].value(), b"replacement");
        assert_eq!(
            set_xattr_value_with_mode(
                &mut xattrs,
                Ext4XattrNamespace::User,
                b"missing",
                b"value",
                Ext4XattrSetMode::Replace,
            ),
            Err(Ext4Error::NotFound)
        );

        assert_eq!(
            set_xattr_value_with_mode(
                &mut xattrs,
                Ext4XattrNamespace::User,
                b"key",
                b"value",
                Ext4XattrSetMode::CreateAndReplace,
            ),
            Err(Ext4Error::AlreadyExists)
        );
        assert_eq!(
            set_xattr_value_with_mode(
                &mut xattrs,
                Ext4XattrNamespace::User,
                b"missing",
                b"value",
                Ext4XattrSetMode::CreateAndReplace,
            ),
            Err(Ext4Error::NotFound)
        );
    }

    #[test]
    fn replacing_an_identical_xattr_value_is_unchanged() {
        let mut xattrs = vec![Ext4Xattr {
            namespace: Ext4XattrNamespace::User,
            name: Vec::from(&b"key"[..]),
            value: Vec::from(&b"value"[..]),
        }];

        assert_eq!(
            set_xattr_value_with_mode(
                &mut xattrs,
                Ext4XattrNamespace::User,
                b"key",
                b"value",
                Ext4XattrSetMode::Replace,
            ),
            Ok(XattrMutation::Unchanged)
        );
        assert_eq!(xattrs.len(), 1);
        assert_eq!(xattrs[0].value(), b"value");
    }

    #[test]
    fn inline_xattr_encode_orders_entries_for_linux_lookup() {
        let mut bytes = vec![0; 128];
        let xattrs = vec![
            Ext4Xattr {
                namespace: Ext4XattrNamespace::Security,
                name: Vec::from(&b"b"[..]),
                value: Vec::from(&b"2"[..]),
            },
            Ext4Xattr {
                namespace: Ext4XattrNamespace::User,
                name: Vec::from(&b"zz"[..]),
                value: Vec::from(&b"3"[..]),
            },
            Ext4Xattr {
                namespace: Ext4XattrNamespace::User,
                name: Vec::from(&b"a"[..]),
                value: Vec::from(&b"1"[..]),
            },
        ];

        encode_inline_xattrs(&xattrs, &mut bytes).expect("encode inline xattrs");
        let mut parsed = Vec::new();
        decode_xattr_entries(
            &bytes,
            disk_xattr::XATTR_IBODY_HEADER_SIZE,
            XattrEntryOrder::RequireSorted,
            &mut parsed,
        )
        .expect("encoded entries are sorted");
        assert_eq!(
            parsed
                .iter()
                .map(|entry| (entry.namespace, entry.name))
                .collect::<Vec<_>>(),
            vec![
                (Ext4XattrNamespace::User, &b"a"[..]),
                (Ext4XattrNamespace::User, &b"zz"[..]),
                (Ext4XattrNamespace::Security, &b"b"[..]),
            ]
        );
    }

    #[test]
    fn inline_xattr_encode_reports_external_block_requirement() {
        let xattrs = vec![Ext4Xattr {
            namespace: Ext4XattrNamespace::User,
            name: Vec::from(&b"large"[..]),
            value: Vec::from(&b"value-too-large-for-inline-body"[..]),
        }];
        let mut bytes = vec![0; 32];
        let error = encode_inline_xattrs(&xattrs, &mut bytes)
            .expect_err("large value requires external xattr block");

        assert_eq!(
            error,
            Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock)
        );
    }

    #[test]
    fn xattr_mutation_credits_follow_storage_transition() {
        assert_eq!(
            xattr_mutation_credit_count(XattrStorageLayout::Empty, XattrStorageLayout::Inline),
            XATTR_INODE_UPDATE_CREDITS
        );
        assert_eq!(
            xattr_mutation_credit_count(
                XattrStorageLayout::Inline,
                XattrStorageLayout::External { shared: false },
            ),
            XATTR_INODE_UPDATE_CREDITS + XATTR_EXTERNAL_ALLOC_CREDITS
        );
        assert_eq!(
            xattr_mutation_credit_count(
                XattrStorageLayout::External { shared: false },
                XattrStorageLayout::External { shared: false },
            ),
            XATTR_INODE_UPDATE_CREDITS + XATTR_EXTERNAL_REWRITE_CREDITS
        );
        assert_eq!(
            xattr_mutation_credit_count(
                XattrStorageLayout::External { shared: false },
                XattrStorageLayout::Inline,
            ),
            XATTR_INODE_UPDATE_CREDITS + XATTR_EXTERNAL_RELEASE_CREDITS
        );
        assert_eq!(
            xattr_mutation_credit_count(
                XattrStorageLayout::External { shared: true },
                XattrStorageLayout::External { shared: false },
            ),
            XATTR_INODE_UPDATE_CREDITS
                + XATTR_EXTERNAL_ALLOC_CREDITS
                + XATTR_EXTERNAL_SHARED_REFCOUNT_CREDITS
        );
    }

    #[test]
    fn settable_xattr_validation_rejects_unsupported_namespace() {
        validate_settable_xattr(Ext4XattrNamespace::PosixAclAccess, b"").unwrap();
        validate_settable_xattr(Ext4XattrNamespace::PosixAclDefault, b"").unwrap();
        assert_eq!(
            validate_settable_xattr(Ext4XattrNamespace::PosixAclAccess, b"name"),
            Err(Ext4Error::InvalidName)
        );
        assert_eq!(
            validate_settable_xattr(Ext4XattrNamespace::System, b"posix_acl_access"),
            Err(Ext4Error::InvalidName)
        );
        validate_settable_xattr(Ext4XattrNamespace::Trusted, b"name").unwrap();
    }
}
