// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeSet, vec, vec::Vec};

use super::{
    ExtentMappingState,
    checksum::{update_extent_block_checksum, verify_extent_block_checksum},
    validate::{
        decode_header, decode_index, decode_leaf, entry_offset, find_index, min_lblk,
        validate_extent_entries,
    },
};
use crate::{
    BlockCount, CorruptKind, Ext4Error, Ext4Filesystem, Ext4Result, FilesystemBlock, LogicalBlock,
    PhysicalBlock, UnsupportedKind,
    disk::{extent as disk_extent, inode as disk_inode},
    inode::Ext4Inode,
    superblock::{metadata_access_bytes, replace_metadata_access_bytes},
};

impl Ext4Filesystem {
    /// Returns a conservative credit bound for an ordered-data writeback.
    pub(crate) fn extent_writeback_metadata_credits(
        &self,
        inode: &Ext4Inode,
        logical_blocks: u32,
    ) -> Ext4Result<u32> {
        self.ensure_extent_mutation_supported(inode)?;
        ordered_writeback_credit_bound(logical_blocks)
    }

    /// Returns a conservative credit bound for rewriting an extent tree and
    /// releasing every mapping at or beyond `new_blocks`.
    pub(crate) fn extent_truncate_metadata_credits(
        &self,
        inode: &Ext4Inode,
        new_blocks: LogicalBlock,
    ) -> Ext4Result<u32> {
        const INODE_ROOT_CREDITS: u32 = 1;
        const SUPERBLOCK_CREDITS: u32 = 1;
        const ALLOCATOR_BLOCKS_PER_GROUP: u32 = 2;
        const NEW_TREE_BLOCK_CREDITS: u32 = 4;
        const ALLOCATOR_API_HEADROOM: u32 = 4;

        self.ensure_extent_mutation_supported(inode)?;
        let Some(range) = LogicalExtentRange::from_logical_to_tree_end(new_blocks)? else {
            return Ok(0);
        };
        let collected = self.collect_extent_tree(inode)?;
        let mut remaining_extents = collected.extents.clone();
        let released_extents = remove_extent_range(&mut remaining_extents, range)?;
        if released_extents.is_empty() {
            return Ok(0);
        }

        let mut released_groups = BTreeSet::new();
        for extent in &released_extents {
            self.collect_extent_block_groups(*extent, &mut released_groups)?;
        }
        for block in &collected.metadata_blocks {
            released_groups.insert(self.block_group_for_block(FilesystemBlock::new(block.get()))?);
        }

        let new_tree_blocks =
            extent_tree_metadata_block_count(remaining_extents.len(), self.device.block_size())?;
        let old_tree_blocks =
            u32::try_from(collected.metadata_blocks.len()).map_err(|_| Ext4Error::Overflow)?;
        let released_group_count =
            u32::try_from(released_groups.len()).map_err(|_| Ext4Error::Overflow)?;

        // New tree blocks may each need a create record plus allocator bitmap,
        // descriptor, and superblock access. Released data blocks only add one
        // bitmap and descriptor target per physical group; old tree blocks also
        // need one revoke credit each. Extra headroom satisfies allocator entry
        // checks even after all distinct target credits have been consumed.
        INODE_ROOT_CREDITS
            .checked_add(
                new_tree_blocks
                    .checked_mul(NEW_TREE_BLOCK_CREDITS)
                    .ok_or(Ext4Error::Overflow)?,
            )
            .and_then(|credits| credits.checked_add(old_tree_blocks))
            .and_then(|credits| {
                released_group_count
                    .checked_mul(ALLOCATOR_BLOCKS_PER_GROUP)
                    .and_then(|group_credits| credits.checked_add(group_credits))
            })
            .and_then(|credits| credits.checked_add(SUPERBLOCK_CREDITS))
            .and_then(|credits| credits.checked_add(ALLOCATOR_API_HEADROOM))
            .ok_or(Ext4Error::Overflow)
    }

    fn collect_extent_block_groups(
        &self,
        extent: MutableExtent,
        groups: &mut BTreeSet<crate::BlockGroupNumber>,
    ) -> Ext4Result<()> {
        let first = self
            .block_group_for_block(FilesystemBlock::new(extent.physical.get()))?
            .get();
        let last_block = extent
            .physical_end()?
            .checked_sub(1)
            .ok_or(Ext4Error::Overflow)?;
        let last = self
            .block_group_for_block(FilesystemBlock::new(last_block))?
            .get();
        for group in first..=last {
            groups.insert(crate::BlockGroupNumber::new(group));
        }
        Ok(())
    }

    pub(crate) fn insert_extent_mapping(
        &mut self,
        inode: &Ext4Inode,
        logical: LogicalBlock,
        physical: PhysicalBlock,
        len: BlockCount,
        state: ExtentMappingState,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_extent_mutation_supported(inode)?;
        let logical = logical_block_u32(logical)?;
        let new_extents =
            MutableExtent::from_run(logical, physical, len, state, |block, count| {
                self.is_inode_physical_block_valid(inode.number(), block, count)
            })?;
        self.insert_extent_mapping_path_local(inode, &new_extents, handle)
    }

    #[expect(dead_code)]
    pub(crate) fn insert_inline_extent_mapping(
        &mut self,
        inode: &Ext4Inode,
        logical: LogicalBlock,
        physical: PhysicalBlock,
        len: BlockCount,
        state: ExtentMappingState,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_extent_mutation_supported(inode)?;

        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        let mut inode_table_bytes = metadata_access_bytes(&inode_table_access)?;
        let logical = logical_block_u32(logical)?;
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| {
                let i_block = inode_bytes
                    .get_mut(
                        disk_inode::I_BLOCK_OFFSET
                            ..disk_inode::I_BLOCK_OFFSET + disk_inode::INODE_BLOCK_BYTES,
                    )
                    .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
                insert_inline_extent_bytes(
                    i_block,
                    logical,
                    physical,
                    len,
                    state,
                    |block, count| self.is_inode_physical_block_valid(inode.number(), block, count),
                )
            },
        )?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        Ok(updated_inode)
    }

    pub(crate) fn convert_unwritten_extent_range(
        &mut self,
        inode: &Ext4Inode,
        logical: LogicalBlock,
        len: BlockCount,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_extent_mutation_supported(inode)?;
        let range = LogicalExtentRange::new(logical, len)?;
        self.set_extent_range_state_path_local(
            inode,
            range,
            ExtentMappingState::Initialized,
            handle,
        )
    }

    #[expect(dead_code)]
    pub(crate) fn remove_extent_range(
        &mut self,
        inode: &Ext4Inode,
        logical: LogicalBlock,
        len: BlockCount,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_extent_mutation_supported(inode)?;
        let range = LogicalExtentRange::new(logical, len)?;
        if let Some(inode) = self.try_remove_extent_range_path_local(inode, range, handle)? {
            return Ok(inode);
        }
        self.mutate_extent_tree(inode, handle, |extents| remove_extent_range(extents, range))
    }

    pub(crate) fn truncate_extent_mappings(
        &mut self,
        inode: &Ext4Inode,
        new_blocks: LogicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_extent_mutation_supported(inode)?;
        let Some(range) = LogicalExtentRange::from_logical_to_tree_end(new_blocks)? else {
            return Ok(inode.clone());
        };
        if let Some(inode) = self.try_remove_extent_range_path_local(inode, range, handle)? {
            return Ok(inode);
        }
        self.mutate_extent_tree(inode, handle, |extents| remove_extent_range(extents, range))
    }

    fn insert_extent_mapping_path_local(
        &mut self,
        inode: &Ext4Inode,
        new_extents: &[MutableExtent],
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let first = new_extents
            .first()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
        let last = new_extents
            .last()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
        let path = self.locate_extent_path(inode, first.logical)?;
        // `map_blocks()` limits hole mappings to the selected leaf boundary.
        // Keeping this operation single-path makes its journal cost independent
        // of the number of extents already present in the inode.
        if path
            .upper_lblk
            .is_some_and(|upper| last.end().map_or(true, |end| end > upper))
        {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation));
        }

        let header = path.leaf_header()?;
        let mut extents = path.decode_leaf_extents(self, inode)?;
        insert_extent_run(&mut extents, new_extents)?;
        let (inode, new_metadata_blocks) = if extents.len() > usize::from(header.max()) {
            path.split_leaf(self, inode, &extents, handle)?
        } else {
            (path.rewrite_leaf(self, inode, &extents, handle)?, 0)
        };
        let added_blocks = new_extents.iter().try_fold(0u64, |blocks, extent| {
            blocks
                .checked_add(u64::from(extent.len))
                .ok_or(Ext4Error::Overflow)
        })?;
        let allocated_delta = added_blocks
            .checked_add(new_metadata_blocks)
            .ok_or(Ext4Error::Overflow)?;
        let inode =
            self.update_inode_extent_block_delta(&inode, i128::from(allocated_delta), handle)?;
        Ok(inode)
    }

    fn inline_extent_root_growth_is_supported(
        &self,
        mut root_depth: u16,
        mut child_count: usize,
    ) -> Ext4Result<bool> {
        let inline_capacity = usize::from(disk_extent::inline_extent_capacity()?);
        let index_capacity = usize::from(disk_extent::extent_block_capacity(
            self.device.block_size(),
        )?);
        if inline_capacity == 0 || index_capacity == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        while child_count > inline_capacity {
            if root_depth >= disk_extent::EXTENT_MAX_DEPTH {
                return Ok(false);
            }
            child_count = child_count.div_ceil(index_capacity);
            root_depth = root_depth.checked_add(1).ok_or(Ext4Error::Overflow)?;
        }
        Ok(root_depth <= disk_extent::EXTENT_MAX_DEPTH)
    }

    #[allow(clippy::too_many_arguments)]
    fn rewrite_split_extent_index_blocks(
        &mut self,
        inode: &Ext4Inode,
        generation: u32,
        depth: u16,
        capacity: usize,
        existing_block: PhysicalBlock,
        children: &[ExtentTreeBlockRef],
        new_metadata_blocks: &mut u64,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Vec<ExtentTreeBlockRef>> {
        if capacity == 0 || children.is_empty() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        let ranges = balanced_partition_ranges(children.len(), capacity)?;
        let mut parents = Vec::with_capacity(ranges.len());
        for (index, range) in ranges.into_iter().enumerate() {
            let chunk = children
                .get(range)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            let first = chunk
                .first()
                .copied()
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            let reused_block = (index == 0).then_some(existing_block);
            let block = if let Some(block) = reused_block {
                block
            } else {
                *new_metadata_blocks = new_metadata_blocks
                    .checked_add(1)
                    .ok_or(Ext4Error::Overflow)?;
                self.allocate_extent_tree_block(inode, None, handle)?
            };
            let access = if reused_block.is_some() {
                self.metadata_io
                    .write_access(FilesystemBlock::new(block.get()), handle)?
            } else {
                self.metadata_io
                    .create_access(FilesystemBlock::new(block.get()), handle)?
            };
            let mut bytes = vec![0; self.device.block_size()];
            encode_extent_index_block(self, inode, &mut bytes, generation, depth, chunk)?;
            replace_metadata_access_bytes(&access, bytes)?;
            parents.push(ExtentTreeBlockRef {
                first_lblk: first.first_lblk,
                block,
                depth,
            });
        }
        Ok(parents)
    }

    #[allow(clippy::too_many_arguments)]
    fn install_inline_extent_root_from_children(
        &mut self,
        inode: &Ext4Inode,
        generation: u32,
        mut root_depth: u16,
        mut children: Vec<ExtentTreeBlockRef>,
        new_metadata_blocks: &mut u64,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let inline_capacity = usize::from(disk_extent::inline_extent_capacity()?);
        let index_capacity = usize::from(disk_extent::extent_block_capacity(
            self.device.block_size(),
        )?);
        if inline_capacity == 0 || index_capacity == 0 || children.is_empty() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        while children.len() > inline_capacity {
            if root_depth >= disk_extent::EXTENT_MAX_DEPTH {
                return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentDepth));
            }
            let mut parents = Vec::with_capacity(children.len().div_ceil(index_capacity));
            for chunk in children.chunks(index_capacity) {
                let first = chunk
                    .first()
                    .copied()
                    .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
                let block = self.allocate_extent_tree_block(inode, None, handle)?;
                *new_metadata_blocks = new_metadata_blocks
                    .checked_add(1)
                    .ok_or(Ext4Error::Overflow)?;
                let access = self
                    .metadata_io
                    .create_access(FilesystemBlock::new(block.get()), handle)?;
                let mut bytes = vec![0; self.device.block_size()];
                encode_extent_index_block(self, inode, &mut bytes, generation, root_depth, chunk)?;
                replace_metadata_access_bytes(&access, bytes)?;
                parents.push(ExtentTreeBlockRef {
                    first_lblk: first.first_lblk,
                    block,
                    depth: root_depth,
                });
            }
            children = parents;
            root_depth = root_depth.checked_add(1).ok_or(Ext4Error::Overflow)?;
        }
        self.rewrite_inode_extent_root(inode, handle, |i_block| {
            encode_inline_index_root(i_block, generation, root_depth, &children, |_, _| true)
        })
    }

    fn set_extent_range_state_path_local(
        &mut self,
        inode: &Ext4Inode,
        range: LogicalExtentRange,
        state: ExtentMappingState,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let path = self.locate_extent_path(inode, range.start_u32()?)?;
        // Ordered writeback converts one leaf-bounded mapping at a time. Do not
        // silently switch algorithms after its transaction budget is reserved.
        if !path.contains_range(range) {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation));
        }
        let header = path.leaf_header()?;
        let mut extents = path.decode_leaf_extents(self, inode)?;
        let old_extents = extents.clone();
        set_extent_range_state(&mut extents, range, state)?;
        if extents == old_extents {
            return Ok(inode.clone());
        }

        let (inode, new_metadata_blocks) = if extents.len() > usize::from(header.max()) {
            path.split_leaf(self, inode, &extents, handle)?
        } else {
            (path.rewrite_leaf(self, inode, &extents, handle)?, 0)
        };
        self.update_inode_extent_block_delta(&inode, i128::from(new_metadata_blocks), handle)
    }

    fn try_remove_extent_range_path_local(
        &mut self,
        inode: &Ext4Inode,
        range: LogicalExtentRange,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Option<Ext4Inode>> {
        let path = self.locate_extent_path(inode, range.start_u32()?)?;
        // `None` is a pre-write decision: callers may safely run the full-tree
        // algorithm only because this branch has not acquired metadata access.
        if !path.contains_range(range) {
            return Ok(None);
        }
        let header = path.leaf_header()?;
        let mut extents = path.decode_leaf_extents(self, inode)?;
        let released = remove_extent_range(&mut extents, range)?;
        if released.is_empty() {
            return Ok(Some(inode.clone()));
        }
        if extents.len() > usize::from(header.max()) && !path.is_split_supported(self, &extents)? {
            return Ok(None);
        }

        let (updated_inode, metadata_delta) = if extents.is_empty() {
            let (inode, released_metadata_blocks) = path.prune_empty_leaf(self, inode, handle)?;
            (inode, -i128::from(released_metadata_blocks))
        } else if extents.len() > usize::from(header.max()) {
            let (inode, new_metadata_blocks) = path.split_leaf(self, inode, &extents, handle)?;
            (inode, i128::from(new_metadata_blocks))
        } else {
            (path.rewrite_leaf(self, inode, &extents, handle)?, 0)
        };
        let released_data_blocks = released.iter().try_fold(0u64, |blocks, extent| {
            blocks
                .checked_add(u64::from(extent.len))
                .ok_or(Ext4Error::Overflow)
        })?;
        for extent in released {
            self.release_extent_data_blocks(extent, handle)?;
        }
        let delta = metadata_delta
            .checked_sub(i128::from(released_data_blocks))
            .ok_or(Ext4Error::Overflow)?;
        let inode = self.update_inode_extent_block_delta(&updated_inode, delta, handle)?;
        Ok(Some(inode))
    }

    fn prune_extent_path_leaf(
        &mut self,
        inode: &Ext4Inode,
        location: &ExtentPath,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<(Ext4Inode, u64)> {
        let generation = decode_header(inode.extent_bytes())?.generation();
        let Some(leaf_block) = location.block else {
            let inode = self.rewrite_inode_extent_root(inode, handle, |i_block| {
                encode_inline_leaf_root(i_block, generation, &[], |_, _| true)
            })?;
            return Ok((inode, 0));
        };
        if location.parents.is_empty() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }

        let mut replacement = None;
        let mut released_blocks = vec![leaf_block];
        let mut updated_inode = None;
        for parent in location.parents.iter().rev() {
            let header = decode_header(&parent.bytes)?;
            let child_depth = header
                .depth()
                .checked_sub(1)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            let mut children = decode_extent_tree_block_refs(&parent.bytes, child_depth)?;
            if parent.selected_entry >= children.len() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            match replacement {
                Some(child) => children[parent.selected_entry] = child,
                None => {
                    children.remove(parent.selected_entry);
                }
            }

            let Some(parent_block) = parent.block else {
                let inode = if children.is_empty() {
                    self.rewrite_inode_extent_root(inode, handle, |i_block| {
                        encode_inline_leaf_root(i_block, generation, &[], |_, _| true)
                    })?
                } else {
                    self.rewrite_inode_extent_root(inode, handle, |i_block| {
                        encode_inline_index_root(
                            i_block,
                            generation,
                            header.depth(),
                            &children,
                            |_, _| true,
                        )
                    })?
                };
                updated_inode = Some(inode);
                break;
            };

            if children.is_empty() {
                released_blocks.push(parent_block);
                replacement = None;
                continue;
            }
            let access = self
                .metadata_io
                .write_access(FilesystemBlock::new(parent_block.get()), handle)?;
            let mut bytes = vec![0; self.device.block_size()];
            encode_extent_index_block(
                self,
                inode,
                &mut bytes,
                generation,
                header.depth(),
                &children,
            )?;
            replace_metadata_access_bytes(&access, bytes)?;
            if parent.selected_entry != 0 {
                updated_inode = Some(inode.clone());
                break;
            }
            replacement = Some(ExtentTreeBlockRef {
                first_lblk: children
                    .first()
                    .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?
                    .first_lblk,
                block: parent_block,
                depth: header.depth(),
            });
        }

        let updated_inode = updated_inode.ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
        for block in &released_blocks {
            if self.journal_supports_revoke() {
                self.release_inode_metadata_block(inode.number(), *block, handle)?;
            } else {
                self.release_inode_metadata_block_without_revoke(inode.number(), *block, handle)?;
            }
        }
        let released_blocks =
            u64::try_from(released_blocks.len()).map_err(|_| Ext4Error::Overflow)?;
        Ok((updated_inode, released_blocks))
    }

    fn locate_extent_path(&self, inode: &Ext4Inode, logical: u32) -> Ext4Result<ExtentPath> {
        let mut bytes = ExtentPathBytes::Inline(inode.extent_bytes().to_vec());
        let mut block = None;
        let mut expected_depth = None;
        let mut expected_lblk = None;
        let mut upper_lblk = None;
        let mut parents = Vec::new();

        loop {
            let header = decode_header(&bytes)?;
            if expected_depth.is_some_and(|depth| depth != header.depth()) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            validate_extent_entries(
                &bytes,
                header,
                expected_lblk,
                upper_lblk,
                |physical, count| {
                    self.is_inode_physical_block_valid(inode.number(), physical, count)
                },
            )?;
            if header.depth() == 0 {
                return Ok(ExtentPath {
                    block,
                    expected_lblk,
                    upper_lblk,
                    bytes,
                    parents,
                });
            }

            let selected = find_index(&bytes, header, logical)?;
            let child_upper_lblk = min_lblk(upper_lblk, selected.next_lblk);
            let child_block = selected.index.leaf();
            let child_expected_lblk = selected.index.block();
            let child_depth = header
                .depth()
                .checked_sub(1)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            let child = self.read_metadata_block(FilesystemBlock::new(child_block.get()))?;
            if self.superblock().features().has_metadata_checksum() {
                verify_extent_block_checksum(self, inode, child_block, child.as_ref())?;
            }
            parents.push(ExtentPathNode {
                block,
                bytes,
                selected_entry: selected.entry,
            });
            block = Some(child_block);
            expected_depth = Some(child_depth);
            expected_lblk = Some(child_expected_lblk);
            upper_lblk = child_upper_lblk;
            bytes = ExtentPathBytes::Metadata(child);
        }
    }

    fn rewrite_extent_path_leaf_and_indexes(
        &mut self,
        inode: &Ext4Inode,
        location: &ExtentPath,
        extents: &[MutableExtent],
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let first_lblk = extents
            .first()
            .map(|extent| extent.logical)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
        let Some(block) = location.block else {
            return self.rewrite_inode_extent_root(inode, handle, |i_block| {
                let header = decode_header(i_block)?;
                rewrite_leaf_extent_entries(i_block, header, extents, None, None, |_, _| true)
            });
        };

        let access = self
            .metadata_io
            .write_access(FilesystemBlock::new(block.get()), handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        let header = decode_header(&bytes)?;
        rewrite_leaf_extent_entries(
            &mut bytes,
            header,
            extents,
            Some(first_lblk),
            location.upper_lblk,
            |physical, count| self.is_inode_physical_block_valid(inode.number(), physical, count),
        )?;
        update_extent_block_checksum(self, inode, &mut bytes)?;
        replace_metadata_access_bytes(&access, bytes)?;
        if location.expected_lblk == Some(first_lblk) {
            return Ok(inode.clone());
        }

        let generation = decode_header(inode.extent_bytes())?.generation();
        let mut replacement = ExtentTreeBlockRef {
            first_lblk,
            block,
            depth: 0,
        };
        for parent in location.parents.iter().rev() {
            let header = decode_header(&parent.bytes)?;
            let child_depth = header
                .depth()
                .checked_sub(1)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            if replacement.depth != child_depth {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            let mut children = decode_extent_tree_block_refs(&parent.bytes, child_depth)?;
            let child = children
                .get_mut(parent.selected_entry)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            *child = replacement;

            let Some(parent_block) = parent.block else {
                return self.rewrite_inode_extent_root(inode, handle, |i_block| {
                    encode_inline_index_root(
                        i_block,
                        generation,
                        header.depth(),
                        &children,
                        |_, _| true,
                    )
                });
            };
            let access = self
                .metadata_io
                .write_access(FilesystemBlock::new(parent_block.get()), handle)?;
            let mut bytes = vec![0; self.device.block_size()];
            encode_extent_index_block(
                self,
                inode,
                &mut bytes,
                generation,
                header.depth(),
                &children,
            )?;
            replace_metadata_access_bytes(&access, bytes)?;
            if parent.selected_entry != 0 {
                return Ok(inode.clone());
            }
            replacement = ExtentTreeBlockRef {
                first_lblk: children
                    .first()
                    .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?
                    .first_lblk,
                block: parent_block,
                depth: header.depth(),
            };
        }
        Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent))
    }

    pub(crate) fn has_extent_mapping_from(
        &self,
        inode: &Ext4Inode,
        logical: LogicalBlock,
    ) -> Ext4Result<bool> {
        self.ensure_extent_mutation_supported(inode)?;
        let start = logical.get();
        let collected = self.collect_extent_tree(inode)?;
        for extent in collected.extents {
            if extent.end_u64()? > start {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn ensure_extent_mutation_supported(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        if !inode.has_extents() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode));
        }
        if inode.uses_huge_file_accounting() {
            if !self.superblock().features().has_huge_file() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
            }
            return Err(Ext4Error::Unsupported(UnsupportedKind::HugeFile));
        }
        if !self.superblock().features().has_extents() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        Ok(())
    }

    fn mutate_extent_tree(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
        mutate: impl FnOnce(&mut Vec<MutableExtent>) -> Ext4Result<Vec<MutableExtent>>,
    ) -> Ext4Result<Ext4Inode> {
        let collected = self.collect_extent_tree(inode)?;
        let mut extents = collected.extents;
        let old_extents = extents.clone();
        let released_data = mutate(&mut extents)?;
        merge_adjacent_extents(&mut extents)?;
        validate_mutable_extents(&extents, |block, count| {
            self.is_inode_physical_block_valid(inode.number(), block, count)
        })?;

        if extents == old_extents && released_data.is_empty() {
            return Ok(inode.clone());
        }

        let old_metadata_blocks =
            u64::try_from(collected.metadata_blocks.len()).map_err(|_| Ext4Error::Overflow)?;
        let old_allocated_blocks = extent_tree_allocated_blocks(&old_extents, old_metadata_blocks)?;
        let rewrite = self.rewrite_extent_tree(inode, &extents, handle)?;
        for extent in released_data {
            self.release_extent_data_blocks(extent, handle)?;
        }
        for block in collected.metadata_blocks {
            if self.journal_supports_revoke() {
                self.release_inode_metadata_block(inode.number(), block, handle)?;
            } else {
                // The current coordinator checkpoints every older transaction
                // before opening this handle, so there is no older journaled
                // image that recovery must suppress for a reused block.
                self.release_inode_metadata_block_without_revoke(inode.number(), block, handle)?;
            }
        }
        let inode = self.update_inode_extent_block_accounting(
            &rewrite.inode,
            old_allocated_blocks,
            &extents,
            rewrite.metadata_blocks,
            handle,
        )?;
        Ok(inode)
    }

    fn collect_extent_tree(&self, inode: &Ext4Inode) -> Ext4Result<CollectedExtentTree> {
        let mut collected = CollectedExtentTree {
            extents: Vec::new(),
            metadata_blocks: Vec::new(),
        };
        self.collect_extent_node(
            inode,
            inode.extent_bytes(),
            None,
            None,
            None,
            None,
            &mut collected,
        )?;
        collected.metadata_blocks.sort_unstable();
        if collected
            .metadata_blocks
            .windows(2)
            .any(|window| window[0] == window[1])
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        validate_mutable_extents(&collected.extents, |block, count| {
            self.is_inode_physical_block_valid(inode.number(), block, count)
        })?;
        Ok(collected)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_extent_node(
        &self,
        inode: &Ext4Inode,
        bytes: &[u8],
        block: Option<PhysicalBlock>,
        expected_depth: Option<u16>,
        expected_lblk: Option<u32>,
        upper_lblk: Option<u32>,
        collected: &mut CollectedExtentTree,
    ) -> Ext4Result<()> {
        let header = decode_header(bytes)?;
        if expected_depth.is_some_and(|depth| depth != header.depth()) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        validate_extent_entries(bytes, header, expected_lblk, upper_lblk, |block, count| {
            self.is_inode_physical_block_valid(inode.number(), block, count)
        })?;

        if let Some(block) = block {
            collected.metadata_blocks.push(block);
        }

        if header.depth() == 0 {
            for index in 0..usize::from(header.entries()) {
                collected
                    .extents
                    .push(MutableExtent::from_leaf(decode_leaf(bytes, index)?)?);
            }
            return Ok(());
        }

        let child_depth = header
            .depth()
            .checked_sub(1)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
        for index in 0..usize::from(header.entries()) {
            let extent_index = decode_index(bytes, index)?;
            let next_lblk = if index + 1 < usize::from(header.entries()) {
                Some(decode_index(bytes, index + 1)?.block())
            } else {
                None
            };
            let child_upper_lblk = min_lblk(upper_lblk, next_lblk);
            let child_block = extent_index.leaf();
            let child_bytes = self.read_metadata_block(FilesystemBlock::new(child_block.get()))?;
            if self.superblock().features().has_metadata_checksum() {
                verify_extent_block_checksum(self, inode, child_block, child_bytes.as_ref())?;
            }
            self.collect_extent_node(
                inode,
                child_bytes.as_ref(),
                Some(child_block),
                Some(child_depth),
                Some(extent_index.block()),
                child_upper_lblk,
                collected,
            )?;
        }
        Ok(())
    }

    fn rewrite_extent_tree(
        &mut self,
        inode: &Ext4Inode,
        extents: &[MutableExtent],
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<ExtentRewrite> {
        let generation = decode_header(inode.extent_bytes())?.generation();
        let inline_capacity = usize::from(disk_extent::inline_extent_capacity()?);
        if extents.len() <= inline_capacity {
            let inode = self.rewrite_inode_extent_root(inode, handle, |i_block| {
                encode_inline_leaf_root(i_block, generation, extents, |_, _| true)
            })?;
            return Ok(ExtentRewrite {
                inode,
                metadata_blocks: 0,
            });
        }

        let created = self.create_extent_tree_blocks(inode, generation, extents, handle)?;
        let children = created.root_children;
        let root_depth = children
            .first()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?
            .depth
            .checked_add(1)
            .ok_or(Ext4Error::Overflow)?;
        if root_depth > disk_extent::EXTENT_MAX_DEPTH {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentDepth));
        }
        let inode = self.rewrite_inode_extent_root(inode, handle, |i_block| {
            encode_inline_index_root(i_block, generation, root_depth, &children, |_, _| true)
        })?;
        Ok(ExtentRewrite {
            inode,
            metadata_blocks: created.metadata_blocks,
        })
    }

    fn rewrite_inode_extent_root(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
        encode: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<Ext4Inode> {
        let inode_table_block = self.inode_table_entry_block(inode.number())?;
        let inode_table_access = self.metadata_io.write_access(inode_table_block, handle)?;
        let mut inode_table_bytes = metadata_access_bytes(&inode_table_access)?;
        let updated_inode = self.update_referenced_inode_table_entry(
            &mut inode_table_bytes,
            inode,
            |inode_bytes| {
                let i_block = inode_bytes
                    .get_mut(
                        disk_inode::I_BLOCK_OFFSET
                            ..disk_inode::I_BLOCK_OFFSET + disk_inode::INODE_BLOCK_BYTES,
                    )
                    .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
                encode(i_block)
            },
        )?;
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes)?;
        Ok(updated_inode)
    }

    fn create_extent_tree_blocks(
        &mut self,
        inode: &Ext4Inode,
        generation: u32,
        extents: &[MutableExtent],
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<CreatedExtentTree> {
        let block_size = self.device.block_size();
        let leaf_capacity = usize::from(disk_extent::extent_block_capacity(block_size)?);
        if leaf_capacity == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }

        let mut level = Vec::new();
        let mut metadata_blocks = 0u64;
        for chunk in extents.chunks(leaf_capacity) {
            let block = self.allocate_extent_tree_block(inode, chunk.first().copied(), handle)?;
            metadata_blocks = metadata_blocks.checked_add(1).ok_or(Ext4Error::Overflow)?;
            let access = self
                .metadata_io
                .create_access(FilesystemBlock::new(block.get()), handle)?;
            let mut bytes = vec![0; block_size];
            encode_extent_leaf_block(self, inode, &mut bytes, generation, chunk)?;
            replace_metadata_access_bytes(&access, bytes)?;
            level.push(ExtentTreeBlockRef {
                first_lblk: chunk
                    .first()
                    .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?
                    .logical,
                block,
                depth: 0,
            });
        }

        let inline_capacity = usize::from(disk_extent::inline_extent_capacity()?);
        let index_capacity = usize::from(disk_extent::extent_block_capacity(block_size)?);
        while level.len() > inline_capacity {
            let child_depth = level
                .first()
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?
                .depth;
            let parent_depth = child_depth.checked_add(1).ok_or(Ext4Error::Overflow)?;
            if parent_depth >= disk_extent::EXTENT_MAX_DEPTH {
                return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentDepth));
            }
            let mut next_level = Vec::new();
            for chunk in level.chunks(index_capacity) {
                let block = self.allocate_extent_tree_block(inode, None, handle)?;
                metadata_blocks = metadata_blocks.checked_add(1).ok_or(Ext4Error::Overflow)?;
                let access = self
                    .metadata_io
                    .create_access(FilesystemBlock::new(block.get()), handle)?;
                let mut bytes = vec![0; block_size];
                encode_extent_index_block(
                    self,
                    inode,
                    &mut bytes,
                    generation,
                    parent_depth,
                    chunk,
                )?;
                replace_metadata_access_bytes(&access, bytes)?;
                next_level.push(ExtentTreeBlockRef {
                    first_lblk: chunk
                        .first()
                        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?
                        .first_lblk,
                    block,
                    depth: parent_depth,
                });
            }
            level = next_level;
        }
        Ok(CreatedExtentTree {
            root_children: level,
            metadata_blocks,
        })
    }

    fn update_inode_extent_block_accounting(
        &self,
        inode: &Ext4Inode,
        old_allocated_blocks: u64,
        extents: &[MutableExtent],
        metadata_blocks: u64,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let new_allocated_blocks = extent_tree_allocated_blocks(extents, metadata_blocks)?;
        let delta = i128::from(new_allocated_blocks)
            .checked_sub(i128::from(old_allocated_blocks))
            .ok_or(Ext4Error::Overflow)?;
        self.update_inode_extent_block_delta(inode, delta, handle)
    }

    fn update_inode_extent_block_delta(
        &self,
        inode: &Ext4Inode,
        delta: i128,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        if delta == 0 {
            return Ok(inode.clone());
        }
        let blocks = inode_blocks_after_extent_delta(inode, self.layout().block_size(), delta)?;
        self.update_inode_blocks_metadata(inode, blocks, handle)
    }

    fn allocate_extent_tree_block(
        &mut self,
        inode: &Ext4Inode,
        goal_extent: Option<MutableExtent>,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<PhysicalBlock> {
        let goal = goal_extent.map(|extent| FilesystemBlock::new(extent.physical.get()));
        let block = self.allocate_block(goal, handle)?.block();
        self.add_system_zone(block.get(), 1, Some(inode.number()))?;
        Ok(block)
    }

    fn release_extent_data_blocks(
        &mut self,
        extent: MutableExtent,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let start = extent.physical.get();
        let end = extent.physical_end()?;
        for block in start..end {
            self.release_allocated_block(PhysicalBlock::new(block), handle)?;
        }
        Ok(())
    }
}

fn extent_tree_metadata_block_count(extent_count: usize, block_size: usize) -> Ext4Result<u32> {
    let inline_capacity = usize::from(disk_extent::inline_extent_capacity()?);
    if extent_count <= inline_capacity {
        return Ok(0);
    }
    let block_capacity = usize::from(disk_extent::extent_block_capacity(block_size)?);
    if block_capacity == 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }

    let mut level_blocks = extent_count.div_ceil(block_capacity);
    let mut total_blocks = level_blocks;
    while level_blocks > inline_capacity {
        level_blocks = level_blocks.div_ceil(block_capacity);
        total_blocks = total_blocks
            .checked_add(level_blocks)
            .ok_or(Ext4Error::Overflow)?;
    }
    u32::try_from(total_blocks).map_err(|_| Ext4Error::Overflow)
}

fn balanced_partition_ranges(
    entry_count: usize,
    capacity: usize,
) -> Ext4Result<Vec<core::ops::Range<usize>>> {
    if entry_count == 0 || capacity == 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    let partition_count = entry_count.div_ceil(capacity);
    let base_len = entry_count / partition_count;
    let longer_partitions = entry_count % partition_count;
    let mut ranges = Vec::with_capacity(partition_count);
    let mut start = 0usize;
    for index in 0..partition_count {
        let len = base_len + usize::from(index < longer_partitions);
        let end = start.checked_add(len).ok_or(Ext4Error::Overflow)?;
        ranges.push(start..end);
        start = end;
    }
    if start != entry_count {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    Ok(ranges)
}

fn ordered_writeback_credit_bound(logical_blocks: u32) -> Ext4Result<u32> {
    const BASE_METADATA_CREDITS: u32 = 8;
    const MUTATIONS_PER_LOGICAL_BLOCK: u32 = 2;
    // One existing path block may be dirtied, while a split can allocate one
    // replacement block with bitmap, group-descriptor, superblock, and create
    // targets. Counting both at every possible level also covers root growth.
    const TARGETS_PER_PATH_LEVEL: u32 = 5;
    const INODE_TARGETS_PER_MUTATION: u32 = 2;
    const DATA_ALLOCATOR_TARGETS_PER_LOGICAL_BLOCK: u32 = 3;

    let path_levels = u32::from(disk_extent::EXTENT_MAX_DEPTH)
        .checked_add(1)
        .ok_or(Ext4Error::Overflow)?;
    let targets_per_mutation = path_levels
        .checked_mul(TARGETS_PER_PATH_LEVEL)
        .and_then(|credits| credits.checked_add(INODE_TARGETS_PER_MUTATION))
        .ok_or(Ext4Error::Overflow)?;
    let targets_per_logical_block = targets_per_mutation
        .checked_mul(MUTATIONS_PER_LOGICAL_BLOCK)
        .and_then(|credits| credits.checked_add(DATA_ALLOCATOR_TARGETS_PER_LOGICAL_BLOCK))
        .ok_or(Ext4Error::Overflow)?;
    logical_blocks
        .checked_mul(targets_per_logical_block)
        .and_then(|credits| credits.checked_add(BASE_METADATA_CREDITS))
        .ok_or(Ext4Error::Overflow)
}

pub(super) fn insert_inline_extent_bytes(
    bytes: &mut [u8],
    logical: u32,
    physical: PhysicalBlock,
    len: BlockCount,
    state: ExtentMappingState,
    mut is_valid_physical_block: impl FnMut(u64, u64) -> bool,
) -> Ext4Result<()> {
    let mut extents = decode_leaf_extents(bytes, None, None, |block, count| {
        is_valid_physical_block(block, count)
    })?;
    let new_extents = MutableExtent::from_run(logical, physical, len, state, |block, count| {
        is_valid_physical_block(block, count)
    })?;
    insert_extent_run(&mut extents, &new_extents)?;
    let header = decode_header(bytes)?;
    if extents.len() > usize::from(header.max()) {
        return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation));
    }
    rewrite_leaf_extent_entries(bytes, header, &extents, None, None, |block, count| {
        is_valid_physical_block(block, count)
    })
}

fn insert_extent_run(
    extents: &mut Vec<MutableExtent>,
    new_extents: &[MutableExtent],
) -> Ext4Result<Vec<MutableExtent>> {
    let first = new_extents
        .first()
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
    let last = new_extents
        .last()
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
    let run_end = last.end()?;
    let insert_at = extents.partition_point(|extent| extent.logical < first.logical);
    if insert_at
        .checked_sub(1)
        .and_then(|index| extents.get(index))
        .is_some_and(|extent| extent.end().map_or(true, |end| end > first.logical))
        || extents
            .get(insert_at)
            .is_some_and(|extent| extent.logical < run_end)
    {
        return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation));
    }

    extents.splice(insert_at..insert_at, new_extents.iter().copied());
    merge_adjacent_extents(extents)?;
    Ok(Vec::new())
}

fn set_extent_range_state(
    extents: &mut Vec<MutableExtent>,
    range: LogicalExtentRange,
    state: ExtentMappingState,
) -> Ext4Result<Vec<MutableExtent>> {
    let mut cursor = range.start;
    let mut converted = Vec::with_capacity(extents.len().saturating_add(2));
    for extent in extents.iter().copied() {
        let extent_start = u64::from(extent.logical);
        let extent_end = extent.end_u64()?;
        if extent_end <= range.start || extent_start >= range.end {
            converted.push(extent);
            continue;
        }

        let overlap_start = extent_start.max(range.start);
        let overlap_end = extent_end.min(range.end);
        if overlap_start > cursor {
            return Err(Ext4Error::Unsupported(UnsupportedKind::UnallocatedWrite));
        }

        if extent_start < overlap_start {
            converted.push(extent.slice(extent_start, overlap_start)?);
        }
        converted.push(extent.slice(overlap_start, overlap_end)?.with_state(state));
        if overlap_end < extent_end {
            converted.push(extent.slice(overlap_end, extent_end)?);
        }
        cursor = overlap_end;
    }

    if cursor < range.end {
        return Err(Ext4Error::Unsupported(UnsupportedKind::UnallocatedWrite));
    }

    *extents = converted;
    merge_adjacent_extents(extents)?;
    Ok(Vec::new())
}

fn remove_extent_range(
    extents: &mut Vec<MutableExtent>,
    range: LogicalExtentRange,
) -> Ext4Result<Vec<MutableExtent>> {
    let mut kept = Vec::with_capacity(extents.len());
    let mut released = Vec::new();

    for extent in extents.iter().copied() {
        let extent_start = u64::from(extent.logical);
        let extent_end = extent.end_u64()?;
        if extent_end <= range.start || extent_start >= range.end {
            kept.push(extent);
            continue;
        }

        let overlap_start = extent_start.max(range.start);
        let overlap_end = extent_end.min(range.end);
        if extent_start < overlap_start {
            kept.push(extent.slice(extent_start, overlap_start)?);
        }
        released.push(extent.slice(overlap_start, overlap_end)?);
        if overlap_end < extent_end {
            kept.push(extent.slice(overlap_end, extent_end)?);
        }
    }

    *extents = kept;
    merge_adjacent_extents(extents)?;
    Ok(released)
}

fn decode_leaf_extents(
    bytes: &[u8],
    expected_lblk: Option<u32>,
    upper_lblk: Option<u32>,
    mut is_valid_physical_block: impl FnMut(u64, u64) -> bool,
) -> Ext4Result<Vec<MutableExtent>> {
    let header = decode_header(bytes)?;
    if header.depth() != 0 {
        return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentDepth));
    }
    validate_extent_entries(bytes, header, expected_lblk, upper_lblk, |block, count| {
        is_valid_physical_block(block, count)
    })?;

    let mut extents = Vec::with_capacity(usize::from(header.entries()));
    for index in 0..usize::from(header.entries()) {
        extents.push(MutableExtent::from_leaf(decode_leaf(bytes, index)?)?);
    }
    Ok(extents)
}

fn rewrite_leaf_extent_entries(
    bytes: &mut [u8],
    header: disk_extent::ExtentHeader,
    extents: &[MutableExtent],
    expected_lblk: Option<u32>,
    upper_lblk: Option<u32>,
    mut is_valid_physical_block: impl FnMut(u64, u64) -> bool,
) -> Ext4Result<()> {
    clear_extent_entries(bytes, header)?;
    for (index, extent) in extents.iter().copied().enumerate() {
        let offset = entry_offset(index)?;
        let end = offset
            .checked_add(disk_extent::EXTENT_ENTRY_SIZE)
            .ok_or(Ext4Error::Overflow)?;
        extent
            .to_leaf()?
            .encode(bytes.get_mut(offset..end).ok_or(Ext4Error::OutOfBounds)?)?;
    }
    let entries = u16::try_from(extents.len()).map_err(|_| Ext4Error::Overflow)?;
    disk_extent::update_header_entries(bytes, entries)?;
    let header = decode_header(bytes)?;
    validate_extent_entries(bytes, header, expected_lblk, upper_lblk, |block, count| {
        is_valid_physical_block(block, count)
    })
}

fn encode_inline_leaf_root(
    bytes: &mut [u8],
    generation: u32,
    extents: &[MutableExtent],
    mut is_valid_physical_block: impl FnMut(u64, u64) -> bool,
) -> Ext4Result<()> {
    bytes.fill(0);
    let max = disk_extent::inline_extent_capacity()?;
    if extents.len() > usize::from(max) {
        return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation));
    }
    let entries = u16::try_from(extents.len()).map_err(|_| Ext4Error::Overflow)?;
    disk_extent::encode_header(bytes, entries, max, 0, generation)?;
    let header = decode_header(bytes)?;
    rewrite_leaf_extent_entries(bytes, header, extents, None, None, |block, count| {
        is_valid_physical_block(block, count)
    })
}

fn encode_extent_leaf_block(
    filesystem: &Ext4Filesystem,
    inode: &Ext4Inode,
    bytes: &mut [u8],
    generation: u32,
    extents: &[MutableExtent],
) -> Ext4Result<()> {
    bytes.fill(0);
    let max = disk_extent::extent_block_capacity(bytes.len())?;
    if extents.len() > usize::from(max) {
        return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation));
    }
    let entries = u16::try_from(extents.len()).map_err(|_| Ext4Error::Overflow)?;
    disk_extent::encode_header(bytes, entries, max, 0, generation)?;
    let header = decode_header(bytes)?;
    rewrite_leaf_extent_entries(bytes, header, extents, None, None, |block, count| {
        filesystem.is_inode_physical_block_valid(inode.number(), block, count)
    })?;
    update_extent_block_checksum(filesystem, inode, bytes)
}

fn encode_inline_index_root(
    bytes: &mut [u8],
    generation: u32,
    depth: u16,
    children: &[ExtentTreeBlockRef],
    mut is_valid_physical_block: impl FnMut(u64, u64) -> bool,
) -> Ext4Result<()> {
    bytes.fill(0);
    let max = disk_extent::inline_extent_capacity()?;
    if depth == 0 || children.is_empty() || children.len() > usize::from(max) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    let entries = u16::try_from(children.len()).map_err(|_| Ext4Error::Overflow)?;
    disk_extent::encode_header(bytes, entries, max, depth, generation)?;
    write_extent_indexes(bytes, children)?;
    let header = decode_header(bytes)?;
    validate_extent_entries(bytes, header, None, None, |block, count| {
        is_valid_physical_block(block, count)
    })
}

fn encode_extent_index_block(
    filesystem: &Ext4Filesystem,
    inode: &Ext4Inode,
    bytes: &mut [u8],
    generation: u32,
    depth: u16,
    children: &[ExtentTreeBlockRef],
) -> Ext4Result<()> {
    bytes.fill(0);
    let max = disk_extent::extent_block_capacity(bytes.len())?;
    if depth == 0 || children.is_empty() || children.len() > usize::from(max) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    let entries = u16::try_from(children.len()).map_err(|_| Ext4Error::Overflow)?;
    disk_extent::encode_header(bytes, entries, max, depth, generation)?;
    write_extent_indexes(bytes, children)?;
    let header = decode_header(bytes)?;
    validate_extent_entries(bytes, header, None, None, |block, count| {
        filesystem.is_inode_physical_block_valid(inode.number(), block, count)
    })?;
    update_extent_block_checksum(filesystem, inode, bytes)
}

fn write_extent_indexes(bytes: &mut [u8], children: &[ExtentTreeBlockRef]) -> Ext4Result<()> {
    for (index, child) in children.iter().copied().enumerate() {
        let offset = entry_offset(index)?;
        let end = offset
            .checked_add(disk_extent::EXTENT_ENTRY_SIZE)
            .ok_or(Ext4Error::Overflow)?;
        disk_extent::ExtentIndex::new(child.first_lblk, child.block)
            .encode(bytes.get_mut(offset..end).ok_or(Ext4Error::OutOfBounds)?)?;
    }
    Ok(())
}

fn decode_extent_tree_block_refs(
    bytes: &[u8],
    child_depth: u16,
) -> Ext4Result<Vec<ExtentTreeBlockRef>> {
    let header = decode_header(bytes)?;
    let parent_depth = child_depth.checked_add(1).ok_or(Ext4Error::Overflow)?;
    if header.depth() != parent_depth {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    let mut children = Vec::with_capacity(usize::from(header.entries()));
    for entry in 0..usize::from(header.entries()) {
        let index = decode_index(bytes, entry)?;
        children.push(ExtentTreeBlockRef {
            first_lblk: index.block(),
            block: index.leaf(),
            depth: child_depth,
        });
    }
    Ok(children)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CollectedExtentTree {
    extents: Vec<MutableExtent>,
    metadata_blocks: Vec<PhysicalBlock>,
}

struct ExtentPath {
    block: Option<PhysicalBlock>,
    expected_lblk: Option<u32>,
    upper_lblk: Option<u32>,
    bytes: ExtentPathBytes,
    parents: Vec<ExtentPathNode>,
}

struct ExtentPathNode {
    block: Option<PhysicalBlock>,
    bytes: ExtentPathBytes,
    selected_entry: usize,
}

enum ExtentPathBytes {
    Inline(Vec<u8>),
    Metadata(crate::buffer::MetadataBuffer),
}

impl core::ops::Deref for ExtentPathBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Inline(bytes) => bytes,
            Self::Metadata(buffer) => buffer.as_ref(),
        }
    }
}

impl ExtentPath {
    fn contains_range(&self, range: LogicalExtentRange) -> bool {
        let starts_in_leaf = self
            .expected_lblk
            .is_none_or(|lower| range.start >= u64::from(lower));
        let ends_in_leaf = self
            .upper_lblk
            .is_none_or(|upper| range.end <= u64::from(upper));
        starts_in_leaf && ends_in_leaf
    }

    fn leaf_header(&self) -> Ext4Result<disk_extent::ExtentHeader> {
        decode_header(&self.bytes)
    }

    fn decode_leaf_extents(
        &self,
        filesystem: &Ext4Filesystem,
        inode: &Ext4Inode,
    ) -> Ext4Result<Vec<MutableExtent>> {
        decode_leaf_extents(
            &self.bytes,
            self.expected_lblk,
            self.upper_lblk,
            |block, count| filesystem.is_inode_physical_block_valid(inode.number(), block, count),
        )
    }

    fn leaf_capacity(&self, filesystem: &Ext4Filesystem) -> Ext4Result<usize> {
        let capacity = if self.block.is_some() {
            usize::from(self.leaf_header()?.max())
        } else {
            usize::from(disk_extent::extent_block_capacity(
                filesystem.device.block_size(),
            )?)
        };
        if capacity == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        Ok(capacity)
    }

    fn is_split_supported(
        &self,
        filesystem: &Ext4Filesystem,
        extents: &[MutableExtent],
    ) -> Ext4Result<bool> {
        if extents.is_empty() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        let leaf_count = extents.len().div_ceil(self.leaf_capacity(filesystem)?);
        if self.block.is_none() {
            if !self.parents.is_empty() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            return filesystem.inline_extent_root_growth_is_supported(1, leaf_count);
        }
        if self.parents.is_empty() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }

        let mut replacement_count = leaf_count;
        for parent in self.parents.iter().rev() {
            let header = decode_header(&parent.bytes)?;
            if parent.selected_entry >= usize::from(header.entries()) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            let child_count = usize::from(header.entries())
                .checked_sub(1)
                .and_then(|count| count.checked_add(replacement_count))
                .ok_or(Ext4Error::Overflow)?;
            if parent.block.is_none() {
                return filesystem
                    .inline_extent_root_growth_is_supported(header.depth(), child_count);
            }
            let capacity = usize::from(header.max());
            if capacity == 0 {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            replacement_count = child_count.div_ceil(capacity);
        }
        Ok(false)
    }

    fn split_leaf(
        &self,
        filesystem: &mut Ext4Filesystem,
        inode: &Ext4Inode,
        extents: &[MutableExtent],
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<(Ext4Inode, u64)> {
        if !self.is_split_supported(filesystem, extents)? {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentDepth));
        }

        let leaf_ranges =
            balanced_partition_ranges(extents.len(), self.leaf_capacity(filesystem)?)?;
        let generation = decode_header(inode.extent_bytes())?.generation();
        let mut new_metadata_blocks = 0u64;
        let mut replacements = Vec::with_capacity(leaf_ranges.len());
        for (index, range) in leaf_ranges.into_iter().enumerate() {
            let chunk = extents
                .get(range)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            let first = chunk
                .first()
                .copied()
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            let reused_block = (index == 0).then_some(self.block).flatten();
            let block = if let Some(block) = reused_block {
                block
            } else {
                new_metadata_blocks = new_metadata_blocks
                    .checked_add(1)
                    .ok_or(Ext4Error::Overflow)?;
                filesystem.allocate_extent_tree_block(inode, Some(first), handle)?
            };
            let access = if reused_block.is_some() {
                filesystem
                    .metadata_io
                    .write_access(FilesystemBlock::new(block.get()), handle)?
            } else {
                filesystem
                    .metadata_io
                    .create_access(FilesystemBlock::new(block.get()), handle)?
            };
            let mut bytes = vec![0; filesystem.device.block_size()];
            encode_extent_leaf_block(filesystem, inode, &mut bytes, generation, chunk)?;
            replace_metadata_access_bytes(&access, bytes)?;
            replacements.push(ExtentTreeBlockRef {
                first_lblk: first.logical,
                block,
                depth: 0,
            });
        }

        if self.parents.is_empty() {
            if self.block.is_some() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            let inode = filesystem.install_inline_extent_root_from_children(
                inode,
                generation,
                1,
                replacements,
                &mut new_metadata_blocks,
                handle,
            )?;
            return Ok((inode, new_metadata_blocks));
        }

        let mut root = None;
        for parent in self.parents.iter().rev() {
            let header = decode_header(&parent.bytes)?;
            let child_depth = header
                .depth()
                .checked_sub(1)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            if replacements.iter().any(|child| child.depth != child_depth) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            let mut children = decode_extent_tree_block_refs(&parent.bytes, child_depth)?;
            if parent.selected_entry >= children.len() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            children.splice(parent.selected_entry..=parent.selected_entry, replacements);

            let Some(parent_block) = parent.block else {
                root = Some((header.depth(), children));
                break;
            };
            replacements = filesystem.rewrite_split_extent_index_blocks(
                inode,
                generation,
                header.depth(),
                usize::from(header.max()),
                parent_block,
                &children,
                &mut new_metadata_blocks,
                handle,
            )?;
        }

        let (root_depth, root_children) =
            root.ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
        let inode = filesystem.install_inline_extent_root_from_children(
            inode,
            generation,
            root_depth,
            root_children,
            &mut new_metadata_blocks,
            handle,
        )?;
        Ok((inode, new_metadata_blocks))
    }

    fn rewrite_leaf(
        &self,
        filesystem: &mut Ext4Filesystem,
        inode: &Ext4Inode,
        extents: &[MutableExtent],
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        filesystem.rewrite_extent_path_leaf_and_indexes(inode, self, extents, handle)
    }

    fn prune_empty_leaf(
        &self,
        filesystem: &mut Ext4Filesystem,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<(Ext4Inode, u64)> {
        filesystem.prune_extent_path_leaf(inode, self, handle)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExtentRewrite {
    inode: Ext4Inode,
    metadata_blocks: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CreatedExtentTree {
    root_children: Vec<ExtentTreeBlockRef>,
    metadata_blocks: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExtentTreeBlockRef {
    first_lblk: u32,
    block: PhysicalBlock,
    depth: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LogicalExtentRange {
    start: u64,
    end: u64,
}

impl LogicalExtentRange {
    fn new(logical: LogicalBlock, len: BlockCount) -> Ext4Result<Self> {
        if len.get() == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        let start = logical.get();
        let end = start
            .checked_add(u64::from(len.get()))
            .ok_or(Ext4Error::Overflow)?;
        let tree_end = extent_tree_logical_end();
        if start >= tree_end || end > tree_end {
            return Err(Ext4Error::Overflow);
        }
        Ok(Self { start, end })
    }

    fn from_logical_to_tree_end(logical: LogicalBlock) -> Ext4Result<Option<Self>> {
        let start = logical.get();
        let end = extent_tree_logical_end();
        if start >= end {
            return Ok(None);
        }
        Ok(Some(Self { start, end }))
    }

    fn start_u32(self) -> Ext4Result<u32> {
        u32::try_from(self.start).map_err(|_| Ext4Error::Overflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MutableExtent {
    logical: u32,
    physical: PhysicalBlock,
    len: u32,
    is_unwritten: bool,
}

impl MutableExtent {
    fn from_run(
        logical: u32,
        physical: PhysicalBlock,
        len: BlockCount,
        state: ExtentMappingState,
        mut is_valid_physical_block: impl FnMut(u64, u64) -> bool,
    ) -> Ext4Result<Vec<Self>> {
        let len = len.get();
        if len == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        logical.checked_add(len).ok_or(Ext4Error::Overflow)?;
        if !is_valid_physical_block(physical.get(), u64::from(len)) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }

        let max_len = max_extent_len(matches!(state, ExtentMappingState::Unwritten));
        let mut remaining = len;
        let mut logical_cursor = logical;
        let mut physical_cursor = physical.get();
        let mut extents = Vec::new();
        while remaining != 0 {
            let chunk = remaining.min(max_len);
            extents.push(Self::new(
                logical_cursor,
                PhysicalBlock::new(physical_cursor),
                BlockCount::new(chunk),
                state,
                |_, _| true,
            )?);
            logical_cursor = logical_cursor
                .checked_add(chunk)
                .ok_or(Ext4Error::Overflow)?;
            physical_cursor = physical_cursor
                .checked_add(u64::from(chunk))
                .ok_or(Ext4Error::Overflow)?;
            remaining -= chunk;
        }
        Ok(extents)
    }

    fn new(
        logical: u32,
        physical: PhysicalBlock,
        len: BlockCount,
        state: ExtentMappingState,
        mut is_valid_physical_block: impl FnMut(u64, u64) -> bool,
    ) -> Ext4Result<Self> {
        let len = len.get();
        validate_extent_len(len, matches!(state, ExtentMappingState::Unwritten))?;
        logical.checked_add(len).ok_or(Ext4Error::Overflow)?;
        if !is_valid_physical_block(physical.get(), u64::from(len)) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        Ok(Self {
            logical,
            physical,
            len,
            is_unwritten: matches!(state, ExtentMappingState::Unwritten),
        })
    }

    fn from_leaf(leaf: disk_extent::ExtentLeaf) -> Ext4Result<Self> {
        let len = u32::from(leaf.actual_len());
        if len == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        leaf.block().checked_add(len).ok_or(Ext4Error::Overflow)?;
        Ok(Self {
            logical: leaf.block(),
            physical: leaf.start(),
            len,
            is_unwritten: leaf.is_unwritten(),
        })
    }

    fn end(self) -> Ext4Result<u32> {
        self.logical
            .checked_add(self.len)
            .ok_or(Ext4Error::Overflow)
    }

    fn end_u64(self) -> Ext4Result<u64> {
        u64::from(self.logical)
            .checked_add(u64::from(self.len))
            .ok_or(Ext4Error::Overflow)
    }

    fn physical_end(self) -> Ext4Result<u64> {
        self.physical
            .get()
            .checked_add(u64::from(self.len))
            .ok_or(Ext4Error::Overflow)
    }

    fn can_merge(self, other: Self) -> Ext4Result<bool> {
        if self.is_unwritten != other.is_unwritten {
            return Ok(false);
        }
        if self.end()? != other.logical || self.physical_end()? != other.physical.get() {
            return Ok(false);
        }
        let merged_len = self.len.checked_add(other.len).ok_or(Ext4Error::Overflow)?;
        Ok(validate_extent_len(merged_len, self.is_unwritten).is_ok())
    }

    fn merge(self, other: Self) -> Ext4Result<Self> {
        let len = self.len.checked_add(other.len).ok_or(Ext4Error::Overflow)?;
        validate_extent_len(len, self.is_unwritten)?;
        Ok(Self { len, ..self })
    }

    fn slice(self, start: u64, end: u64) -> Ext4Result<Self> {
        if start >= end || start < u64::from(self.logical) || end > self.end_u64()? {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        let offset = start
            .checked_sub(u64::from(self.logical))
            .ok_or(Ext4Error::Overflow)?;
        let logical = u32::try_from(start).map_err(|_| Ext4Error::Overflow)?;
        let physical = self
            .physical
            .get()
            .checked_add(offset)
            .ok_or(Ext4Error::Overflow)?;
        let len = u32::try_from(end - start).map_err(|_| Ext4Error::Overflow)?;
        validate_extent_len(len, self.is_unwritten)?;
        Ok(Self {
            logical,
            physical: PhysicalBlock::new(physical),
            len,
            is_unwritten: self.is_unwritten,
        })
    }

    fn with_state(self, state: ExtentMappingState) -> Self {
        Self {
            is_unwritten: matches!(state, ExtentMappingState::Unwritten),
            ..self
        }
    }

    fn to_leaf(self) -> Ext4Result<disk_extent::ExtentLeaf> {
        let encoded_len = encode_extent_len(self.len, self.is_unwritten)?;
        Ok(disk_extent::ExtentLeaf::new(
            self.logical,
            encoded_len,
            self.physical,
        ))
    }
}

fn validate_mutable_extents(
    extents: &[MutableExtent],
    mut is_valid_physical_block: impl FnMut(u64, u64) -> bool,
) -> Ext4Result<()> {
    let mut previous_end = 0u32;
    for extent in extents.iter().copied() {
        if extent.len == 0 || extent.logical < previous_end {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        validate_extent_len(extent.len, extent.is_unwritten)?;
        if !is_valid_physical_block(extent.physical.get(), u64::from(extent.len)) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }
        previous_end = extent.end()?;
    }
    Ok(())
}

fn merge_adjacent_extents(extents: &mut Vec<MutableExtent>) -> Ext4Result<()> {
    let mut index = 0usize;
    while index + 1 < extents.len() {
        let left = extents[index];
        let right = extents[index + 1];
        if left.can_merge(right)? {
            extents[index] = left.merge(right)?;
            extents.remove(index + 1);
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn clear_extent_entries(bytes: &mut [u8], header: disk_extent::ExtentHeader) -> Ext4Result<()> {
    let start = disk_extent::EXTENT_HEADER_SIZE;
    let end = usize::from(header.max())
        .checked_mul(disk_extent::EXTENT_ENTRY_SIZE)
        .and_then(|len| start.checked_add(len))
        .ok_or(Ext4Error::Overflow)?;
    bytes
        .get_mut(start..end)
        .ok_or(Ext4Error::OutOfBounds)?
        .fill(0);
    Ok(())
}

fn logical_block_u32(logical: LogicalBlock) -> Ext4Result<u32> {
    u32::try_from(logical.get()).map_err(|_| Ext4Error::Overflow)
}

fn extent_tree_allocated_blocks(
    extents: &[MutableExtent],
    metadata_blocks: u64,
) -> Ext4Result<u64> {
    extents.iter().try_fold(metadata_blocks, |blocks, extent| {
        blocks
            .checked_add(u64::from(extent.len))
            .ok_or(Ext4Error::Overflow)
    })
}

fn inode_blocks_after_extent_delta(
    inode: &Ext4Inode,
    block_size: u32,
    fs_block_delta: i128,
) -> Ext4Result<u64> {
    if inode.flags() & disk_inode::EXT4_HUGE_FILE_FL != 0 {
        return Err(Ext4Error::Unsupported(UnsupportedKind::HugeFile));
    }
    if !block_size.is_multiple_of(512) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockSize));
    }
    let sectors_per_block = i128::from(block_size / 512);
    let sector_delta = fs_block_delta
        .checked_mul(sectors_per_block)
        .ok_or(Ext4Error::Overflow)?;
    let blocks = i128::from(inode.blocks())
        .checked_add(sector_delta)
        .ok_or(Ext4Error::Overflow)?;
    if blocks < 0 {
        return Err(Ext4Error::Overflow);
    }
    u64::try_from(blocks).map_err(|_| Ext4Error::Overflow)
}

fn extent_tree_logical_end() -> u64 {
    u64::from(u32::MAX) + 1
}

fn validate_extent_len(len: u32, is_unwritten: bool) -> Ext4Result<()> {
    let _ = encode_extent_len(len, is_unwritten)?;
    Ok(())
}

fn max_extent_len(is_unwritten: bool) -> u32 {
    if is_unwritten {
        u32::from(disk_extent::EXT_UNWRITTEN_MAX_LEN)
    } else {
        u32::from(disk_extent::EXT_INIT_MAX_LEN)
    }
}

fn encode_extent_len(len: u32, is_unwritten: bool) -> Ext4Result<u16> {
    if len == 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    if is_unwritten {
        if len > u32::from(disk_extent::EXT_UNWRITTEN_MAX_LEN) {
            return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation));
        }
        let len = u16::try_from(len).map_err(|_| Ext4Error::Overflow)?;
        return Ok(len | disk_extent::EXT_UNWRITTEN_FLAG);
    }
    if len > u32::from(disk_extent::EXT_INIT_MAX_LEN) {
        return Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation));
    }
    u16::try_from(len).map_err(|_| Ext4Error::Overflow)
}

#[cfg(test)]
mod tests {
    use super::{balanced_partition_ranges, ordered_writeback_credit_bound};

    #[test]
    fn single_block_writeback_covers_maximum_extent_path() {
        assert_eq!(ordered_writeback_credit_bound(1), Ok(75));
    }

    #[test]
    fn multi_block_writeback_scales_credit_budget() {
        let single = ordered_writeback_credit_bound(1).unwrap();
        let multiple = ordered_writeback_credit_bound(8).unwrap();

        assert!(multiple > single);
    }

    #[test]
    fn writeback_credit_budget_is_not_truncated() {
        assert!(ordered_writeback_credit_bound(8).unwrap() > 512);
    }

    #[test]
    fn local_split_balances_new_partitions() {
        assert_eq!(
            balanced_partition_ranges(341, 340),
            Ok(vec![0..171, 171..341])
        );
    }
}
