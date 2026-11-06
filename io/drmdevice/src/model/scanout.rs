// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The one scanout slot of this card.
//!
//! The slot holds two facts that are deliberately separate:
//!
//! * `bound` — the framebuffer the plane is logically bound to. Zero when the
//!   plane is disabled.
//! * `retained` — the resource the host may still be scanning. It outlives the
//!   binding: this driver has no "stop scanning" call, so unbinding a plane
//!   (RMFB, SETCRTC-off) must not free the pixels the host is still reading.
//!
//! Invariant: `bound != 0` implies `retained` is that framebuffer's object.
//! `retained` may be `Some` while `bound` is zero.
//!
//! The retained reference is dropped only once a *new* resource has been
//! successfully presented, which is the moment the host stops scanning the old
//! one. That is the whole retirement policy; there is no parking table.

use alloc::sync::Arc;

use crate::prelude::*;

/// What the host is currently scanning, and what the plane is bound to.
#[derive(Default)]
pub(crate) struct ScanoutSlot {
    bound: FbId,
    retained: Option<Arc<GemObject>>,
    /// Resources whose presentation failed.
    ///
    /// A failed present means the host may have switched to the new resource
    /// even though the call reported an error, so neither the previous nor the
    /// attempted resource can be freed. Both stay here until a present
    /// succeeds; then nothing from the failed attempt can still be scanned.
    ///
    /// This is deliberately unbounded across failed presents: each attempted
    /// object may be the one the host is scanning. A success clears the whole
    /// vector, so the cost ends at the first successful present.
    uncertain: Vec<Arc<GemObject>>,
}

/// The slot's contents before an [`ScanoutSlot::install`].
pub(crate) struct ScanoutSnapshot {
    bound: FbId,
    retained: Option<Arc<GemObject>>,
    uncertain: Vec<Arc<GemObject>>,
}

impl ScanoutSlot {
    /// Whether `fb` is the bound framebuffer.
    pub(crate) fn is_bound(&self, fb: FbId) -> bool {
        self.bound == fb
    }

    /// Installs a new scanout target, returning what was there before.
    ///
    /// The caller drops the snapshot only after the backend accepted the new
    /// resource; on failure it calls [`ScanoutSlot::restore`] so the resource
    /// the host still scans stays alive.
    pub(crate) fn install(&mut self, fb: FbId, object: Arc<GemObject>) -> ScanoutSnapshot {
        let previous = ScanoutSnapshot {
            bound: self.bound,
            retained: self.retained.take(),
            uncertain: core::mem::take(&mut self.uncertain),
        };
        self.bound = fb;
        self.retained = Some(object);
        previous
    }

    /// Clears the plane binding but keeps the scanout resource alive.
    pub(crate) fn unbind(&mut self) {
        self.bound = FbId::default();
    }

    /// Puts a previous snapshot back after a failed present.
    ///
    /// `attempted` is the resource whose present failed: the host may have
    /// switched to it, so it is kept alive alongside the previous target and
    /// the earlier uncertain resources.
    pub(crate) fn restore(&mut self, snapshot: ScanoutSnapshot, attempted: Arc<GemObject>) {
        self.bound = snapshot.bound;
        self.retained = snapshot.retained;
        self.uncertain = snapshot.uncertain;
        self.uncertain.push(attempted);
    }

    /// Drops the retained reference.
    ///
    /// The caller must have presented another resource first, so the host is no
    /// longer scanning this one.
    pub(crate) fn release_retained(snapshot: ScanoutSnapshot) {
        drop(snapshot.retained);
    }
}
