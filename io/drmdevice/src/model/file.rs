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
    syncobj::SyncobjTable,
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
    /// The blank this file observed last (`file_priv->vblank`).
    pub(crate) vblank: Mutex<crate::model::vblank::SequenceTracker>,
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
    /// virtio-gpu 3D context state owned by this open file.
    ///
    /// Linux keeps exactly one context per file (`virtio_gpu_fpriv`): it is
    /// created on first use or by `CONTEXT_INIT`, and every buffer and submit
    /// from this file belongs to it. The state is kept whole instead of split
    /// across the core file model so the virtio-gpu ABI stays in one module.
    pub(crate) virtgpu: Mutex<crate::virtgpu::VirtgpuFile>,
    /// Syncobjs created by this file (`file_priv->syncobj_table`).
    ///
    /// Closing the file drops the table, which releases every object it owned;
    /// a fence a descriptor or another object still references stays alive
    /// through that reference.
    pub(crate) syncobjs: Mutex<SyncobjTable>,
    /// Authentication binding and master ownership.
    pub(crate) auth: Mutex<AuthState>,
    /// Name the client attached to itself through `SET_CLIENT_NAME`.
    ///
    /// Linux keeps it for tracking and debugging (`file_priv->client_name`);
    /// nothing reads it back, so this file records it and that is all.
    pub(crate) client_name: Mutex<alloc::string::String>,
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
    /// Whether the file has been authenticated with `AUTH_MAGIC`.
    ///
    /// This is `file_priv->authenticated`: it is what `DRM_AUTH` commands
    /// check, and it is per file.
    pub authenticated: bool,
}

impl DrmFile {
    /// Creates the per-open state for one minor.
    pub(crate) fn new(dev: Arc<DrmDevice>, kind: MinorKind, auth: AuthState) -> Arc<Self> {
        Arc::new(Self {
            dev,
            kind,
            caps: AtomicU64::new(0),
            vblank: Mutex::new(crate::model::vblank::SequenceTracker::default()),
            handles: Mutex::new(BTreeMap::new()),
            blobs: Mutex::new(BTreeMap::new()),
            fbs: Mutex::new(BTreeSet::new()),
            virtgpu: Mutex::new(crate::virtgpu::VirtgpuFile::default()),
            syncobjs: Mutex::new(SyncobjTable::new()),
            auth: Mutex::new(auth),
            client_name: Mutex::new(alloc::string::String::new()),
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

    /// The object a `mmap` offset names (`drm_gem_object_lookup` by offset).
    ///
    /// This is `drm_vma_offset_lookup_locked` plus the handle check: the
    /// card's offset namespace says which object owns the offset, and the
    /// file must still hold a handle to it. A null object (a fresh import
    /// that was never bound to a handle) is reported as `None`, so a client
    /// cannot map a buffer it never received.
    #[cfg(unittest)]
    pub(crate) fn object_by_offset(&self, offset_bytes: u64) -> Option<Arc<GemObject>> {
        self.object_by_offset_range(offset_bytes, 1)
    }

    /// The object that covers the mapping `[offset, offset + bytes)`.
    pub(crate) fn object_by_offset_range(
        &self,
        offset_bytes: u64,
        bytes: u64,
    ) -> Option<Arc<GemObject>> {
        let range = self.dev.vma.lookup_exact(offset_bytes, bytes)?;
        let object = self.dev.object_at_offset(range.offset)?;
        self.handles
            .lock()
            .values()
            .any(|known| Arc::ptr_eq(known, &object))
            .then_some(object)
    }
}
