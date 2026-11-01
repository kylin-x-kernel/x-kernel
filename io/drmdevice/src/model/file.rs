// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-open file state, the equivalent of Linux `struct drm_file`.
//!
//! Everything here is scoped to one open file description: the node it was
//! opened through, negotiated client capabilities, GEM handles, created
//! framebuffers and property blobs, and its authentication binding.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use core::sync::atomic::{AtomicU64, Ordering};

use ksync::Mutex;

use super::{
    device::{DrmDevice, MinorKind},
    ids::{DumbHandle, FbId},
};
use crate::{auth::DrmMaster, gem::GemObject};

/// `DRM_CLIENT_CAP_ATOMIC`.
pub(crate) const CLIENT_CAP_ATOMIC: u64 = 3;

/// One open file description of a DRM minor.
pub(crate) struct DrmFile {
    /// Card this file belongs to.
    pub(crate) dev: Arc<DrmDevice>,
    /// Minor the file was opened through; the permission input.
    pub(crate) kind: MinorKind,
    /// Capabilities negotiated through `SET_CLIENT_CAP`, indexed by the Linux
    /// `DRM_CLIENT_CAP_*` value.
    caps: AtomicU64,
    /// GEM handles owned by this file, exactly like `file_priv->object_idr`.
    pub(crate) handles: Mutex<BTreeMap<DumbHandle, Arc<GemObject>>>,
    /// Framebuffers created by this file (`file_priv->fbs`); removed on close.
    pub(crate) fbs: Mutex<BTreeSet<FbId>>,
    /// Property blobs created by this file.
    ///
    /// Linux records client blobs on the creating file (`drm_property.c`
    /// stores the file pointer) and destroys them in `drm_file_free()`; the
    /// committed state holds its own `Arc` so the mode stays usable after the
    /// id is removed.
    pub(crate) blobs: Mutex<
        alloc::collections::BTreeMap<
            crate::model::ids::BlobId,
            alloc::sync::Arc<crate::blob::Blob>,
        >,
    >,
    /// Authentication binding and master ownership.
    pub(crate) auth: Mutex<AuthState>,
}

/// Authentication state of one file.
pub(crate) struct AuthState {
    /// Master object this file is bound to.
    pub master: Arc<DrmMaster>,
    /// Whether this file owns `master` and may reuse it when it re-acquires the
    /// card. It does **not** mean the file currently owns the card: that is
    /// `DrmDevice::is_master`, and it is the only authority for permissions.
    pub owns_master_object: bool,
    /// Magic token issued to this file, if any.
    pub magic: Option<u32>,
}

impl DrmFile {
    /// Creates the per-open state for one minor.
    pub(crate) fn new(dev: Arc<DrmDevice>, kind: MinorKind, auth: AuthState) -> Arc<Self> {
        Arc::new(Self {
            dev,
            kind,
            caps: AtomicU64::new(0),
            handles: Mutex::new(BTreeMap::new()),
            blobs: Mutex::new(BTreeMap::new()),
            fbs: Mutex::new(BTreeSet::new()),
            auth: Mutex::new(auth),
        })
    }

    /// Grants one `DRM_CLIENT_CAP_*` bit.
    pub(crate) fn grant_cap(&self, cap: u64) {
        self.caps.fetch_or(1u64 << cap, Ordering::Relaxed);
    }

    /// Clears one `DRM_CLIENT_CAP_*` bit.
    pub(crate) fn revoke_cap(&self, cap: u64) {
        self.caps.fetch_and(!(1u64 << cap), Ordering::Relaxed);
    }

    /// Whether one `DRM_CLIENT_CAP_*` bit was granted.
    pub(crate) fn has_cap(&self, cap: u64) -> bool {
        self.caps.load(Ordering::Relaxed) & (1u64 << cap) != 0
    }

    /// Removes a handle from this file's table.
    pub(crate) fn take_handle(&self, handle: DumbHandle) -> Option<Arc<GemObject>> {
        self.handles.lock().remove(&handle)
    }

    /// Looks up a handle owned by this file.
    pub(crate) fn handle(&self, handle: DumbHandle) -> Option<Arc<GemObject>> {
        self.handles.lock().get(&handle).cloned()
    }

    /// Finds an owned handle by its mmap offset, in bytes.
    pub(crate) fn handle_by_offset_bytes(
        &self,
        offset_bytes: u64,
    ) -> Option<(DumbHandle, Arc<GemObject>)> {
        self.handles
            .lock()
            .iter()
            .find(|(_, object)| object.offset_bytes() == offset_bytes)
            .map(|(handle, object)| (*handle, object.clone()))
    }
}
