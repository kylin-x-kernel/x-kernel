// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub(crate) mod checksum;
pub(crate) mod codec;
pub(crate) mod dir;
pub(crate) mod extent;
pub(crate) mod features;
mod group;
pub(crate) mod inode;
pub(crate) mod superblock;
pub(crate) mod xattr;

pub use dir::DirectoryFileType;
pub use features::{CompatFeatures, FeatureSet, IncompatFeatures, ReadOnlyCompatFeatures};
pub use group::BlockGroupDescriptor;
pub(crate) use group::{
    decrement_group_free_blocks_count, decrement_group_free_inodes_count,
    decrement_group_used_directories_count, increment_group_free_blocks_count,
    increment_group_free_inodes_count, increment_group_used_directories_count,
    set_group_free_blocks_count, update_group_block_bitmap_metadata,
    update_group_inode_allocation_metadata, update_group_inode_bitmap_metadata,
};
pub use superblock::{JournalFields, Superblock};
