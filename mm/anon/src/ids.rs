// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU64, Ordering};

use vmobj::AnonObjectId;

static NEXT_ANON_OBJECT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_ANON_LINEAGE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_anon_object_id() -> AnonObjectId {
    AnonObjectId::from_raw(NEXT_ANON_OBJECT_ID.fetch_add(1, Ordering::Relaxed))
}

pub(crate) fn next_anon_lineage_id() -> AnonLineageId {
    AnonLineageId(NEXT_ANON_LINEAGE_ID.fetch_add(1, Ordering::Relaxed))
}

/// Stable identity for one private anonymous lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnonLineageId(pub(crate) u64);

impl AnonLineageId {
    /// Returns the raw lineage identity.
    pub const fn raw(self) -> u64 {
        self.0
    }
}
