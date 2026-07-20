// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bitmap allocation primitives shared by `balloc` and `ialloc`.
//!
//! This module owns the M4 single-group correctness path. Filesystem-level
//! policy lives in the Linux-style `balloc` and `ialloc` modules; these
//! primitives deliberately do not update group descriptors, superblock
//! counters, extent trees, or directory metadata.

mod bitmap;
mod block;
mod inode;

pub(crate) use block::{
    BlockAllocation, BlockGroupRange, BlockRunAllocation, allocate_block_run_from_bitmap,
    release_block_to_bitmap,
};
pub(crate) use inode::{
    InodeAllocation, InodeGroupRange, allocate_inode_from_bitmap, release_inode_to_bitmap,
};
