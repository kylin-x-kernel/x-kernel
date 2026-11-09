// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem quota control operations. Callers hold superblock shutdown exclusion.

use alloc::sync::Arc;

use kerrno::KResult;
use kquota::{Dqblk, QuotaFields, QuotaId, QuotaType};

use crate::{SuperBlock, VfsInode};

/// Filesystem methods for quota-file lifecycle and administrative record I/O.
///
/// Accounting remains in the filesystem allocation paths. These operations may
/// sleep and must not run with inode, allocator, or quota-state locks held.
pub trait QuotaOperations: Send + Sync {
    /// Supported quota classes, as one bit per class.
    fn supported_types(&self, sb: &SuperBlock) -> u8;

    /// Whether quota files are filesystem-owned metadata.
    fn has_system_files(&self, sb: &SuperBlock) -> bool;

    /// Loads a quota file and enables accounting and limit enforcement.
    fn quota_on(
        &self,
        sb: &SuperBlock,
        kind: QuotaType,
        format_id: u32,
        file: &Arc<VfsInode>,
    ) -> KResult<()>;

    /// Disables enforcement, and external accounting, after draining writeback.
    fn quota_off(&self, sb: &SuperBlock, kind: QuotaType) -> KResult<()>;

    /// Flushes the selected class; a disabled class is a successful no-op.
    fn sync(&self, sb: &SuperBlock, kind: QuotaType) -> KResult<()>;

    /// Acquires a record and returns its current usage and limits.
    fn get(&self, sb: &SuperBlock, id: QuotaId) -> KResult<Dqblk>;

    /// Returns the first stored record at or above the supplied ID.
    fn next(&self, sb: &SuperBlock, kind: QuotaType, minimum: u32) -> KResult<(QuotaId, Dqblk)>;

    /// Updates only the selected record fields.
    fn set(&self, sb: &SuperBlock, id: QuotaId, value: Dqblk, fields: QuotaFields) -> KResult<()>;

    /// Changes grace durations for future soft-limit crossings.
    fn set_grace_times(
        &self,
        sb: &SuperBlock,
        kind: QuotaType,
        space_seconds: u32,
        inode_seconds: u32,
    ) -> KResult<()>;
}
