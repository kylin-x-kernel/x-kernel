// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The one scanout slot of this card.
//!
//! See [`ScanoutSlot`] for the two facts it keeps apart and the one rule that
//! retires a resource: it is dropped only once a *new* resource has been
//! successfully presented.

use alloc::sync::Arc;

use crate::prelude::*;

/// What the host is currently scanning, and what the plane is bound to.
///
/// The two facts are deliberately separate:
///
/// * `bound` is the framebuffer the plane is logically bound to, zero when it
///   is disabled;
/// * `scanning` is the framebuffer of the resource the host was last asked to
///   display. It outlives `bound`: this driver has no "stop scanning" call, so
///   unbinding a plane (RMFB, a CRTC disable) must not free pixels the host may
///   still be reading.
#[derive(Default)]
pub(crate) struct ScanoutSlot {
    bound: FbId,
    scanning: Option<PendingRetire>,
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
    uncertain: Vec<PendingRetire>,
}

/// The slot's contents before an [`ScanoutSlot::install`].
pub(crate) struct ScanoutSnapshot {
    bound: FbId,
    scanning: Option<PendingRetire>,
    uncertain: Vec<PendingRetire>,
}

/// A resource to destroy once the host stopped scanning it.
///
/// A resource created for one framebuffer has no other owner, so the scanout
/// slot destroys it when it releases it; a framebuffer presenting the
/// object's own resource leaves the destruction to the object.
pub(crate) struct PendingRetire {
    object: Arc<GemObject>,
    resource: Option<ResourceId>,
}

impl ScanoutSlot {
    /// Whether `fb` is the bound framebuffer.
    pub(crate) fn is_bound(&self, fb: FbId) -> bool {
        self.bound == fb
    }

    /// The resource the host is scanning for the bound framebuffer.
    ///
    /// `None` when nothing is installed, or when the presented framebuffer
    /// covers its whole object: that case scans the object's own resource,
    /// which the caller looks up itself. `Some` is the view resource created
    /// for a framebuffer smaller than its object — the one a flush must
    /// target, because the object's resource is not what the host displays.
    pub(crate) fn scanning_resource(&self) -> Option<ResourceId> {
        self.scanning.as_ref().and_then(|pending| pending.resource)
    }

    /// Installs a new scanout target, returning what was there before.
    ///
    /// The caller drops the snapshot only after the backend accepted the new
    /// resource; on failure it calls [`ScanoutSlot::restore`] so the resource
    /// the host still scans stays alive.
    pub(crate) fn install(
        &mut self,
        fb: FbId,
        object: Arc<GemObject>,
        owned: Option<ResourceId>,
    ) -> ScanoutSnapshot {
        let previous = ScanoutSnapshot {
            bound: self.bound,
            scanning: self.scanning.take(),
            uncertain: core::mem::take(&mut self.uncertain),
        };
        self.bound = fb;
        self.scanning = Some(PendingRetire {
            object,
            resource: owned,
        });
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
    pub(crate) fn restore(&mut self, snapshot: ScanoutSnapshot, attempted: PendingRetire) {
        self.bound = snapshot.bound;
        self.scanning = snapshot.scanning;
        self.uncertain = snapshot.uncertain;
        self.uncertain.push(attempted);
    }

    /// Drops the framebuffer the host no longer displays.
    ///
    /// The caller must have presented another resource first, so the host is no
    /// longer scanning this one.
    pub(crate) fn release_replaced(snapshot: ScanoutSnapshot) {
        for pending in snapshot.scanning.into_iter().chain(snapshot.uncertain) {
            pending.retire();
        }
    }
}

impl PendingRetire {
    /// Records a resource together with the object whose memory it addresses.
    pub(crate) const fn new(object: Arc<GemObject>, resource: Option<ResourceId>) -> Self {
        Self { object, resource }
    }

    /// Releases a resource the host stopped scanning.
    ///
    /// The framebuffer's own resource is destroyed; the object's own resource
    /// is only referenced here and is destroyed by the object, so this drops
    /// the entry either way.
    fn retire(self) {
        if let Some(resource) = self.resource
            && let Err(error) = self.object.destroy_owned_resource(resource)
        {
            log::warn!("drm: cannot destroy framebuffer resource {resource:?}: {error:?}");
        }
    }
}
