// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod checksum;
mod legacy;
mod map;
mod mutate;
mod validate;

pub(crate) use mutate::ordered_writeback_credit_bound;

#[cfg(test)]
mod tests;

use crate::{BlockCount, PhysicalBlock};

bitflags::bitflags! {
    /// Semantics attached to a logical-block mapping result.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct BlockMappingFlags: u32 {
        /// The returned run was coalesced from adjacent mappings.
        const MERGED = 1 << 0;
        /// The hole is reserved for delayed allocation.
        const DELAYED = 1 << 1;
    }
}

/// Query-only logical block mapping result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockMapping {
    /// The logical range has no physical blocks and reads as zeroes.
    Hole {
        len: BlockCount,
        flags: BlockMappingFlags,
    },
    /// The logical range maps to initialized physical blocks.
    Mapped {
        physical: PhysicalBlock,
        len: BlockCount,
        flags: BlockMappingFlags,
    },
    /// The logical range is preallocated but reads as zeroes.
    Unwritten {
        physical: PhysicalBlock,
        len: BlockCount,
        flags: BlockMappingFlags,
    },
}

/// Target state for a newly inserted extent mapping.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentMappingState {
    /// The extent exposes initialized file data.
    Initialized,
    /// The extent is allocated but must read as zeroes until conversion.
    Unwritten,
}
