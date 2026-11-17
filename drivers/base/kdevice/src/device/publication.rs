// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Opaque, non-reused identities for class publications.

use core::sync::atomic::{AtomicU64, Ordering};

use driver_base::{DriverError, DriverResult};

/// Identity of one class publication.
///
/// kdevice owns only the global identity space because device-node ownership
/// must be keyed without depending on the kclass crate. The publication state,
/// runtime and revocation credential remain kclass-owned. A publication id is
/// never a revocation permission by itself.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PublicationId(u64);

static NEXT_PUBLICATION_ID: AtomicU64 = AtomicU64::new(1);

impl PublicationId {
    /// Allocate a new publication identity.
    ///
    /// Ids are globally unique, monotonic and never reused. Exhaustion is
    /// reported as `ResourceBusy`; an allocated id may be burned if a later
    /// publish step fails.
    pub fn allocate() -> DriverResult<Self> {
        let id = NEXT_PUBLICATION_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| DriverError::ResourceBusy)?;
        Ok(Self(id))
    }

    /// Raw value for diagnostics and tests.
    pub const fn raw(self) -> u64 {
        self.0
    }
}
