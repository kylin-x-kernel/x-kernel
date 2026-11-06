// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Commit and present methods of the card.
//!
//! The committed KMS state has one writer: [`DrmDevice::commit_with`]. The
//! validate, publish and present steps run under the commit lock, so the
//! state and what is on screen cannot disagree. The KMS ioctls in `kms.rs`
//! reach the committed state only through these methods.

use crate::prelude::*;

impl DrmDevice {
    /// Committed KMS state snapshot.
    pub(crate) fn kms_state(&self) -> KmsState {
        self.state.lock().clone()
    }

    /// Publishes `next` and presents its scanout.
    ///
    /// This is the only writer of the committed state. The validate, publish
    /// and present steps happen while the commit lock is held, so two clients
    /// cannot interleave such that the older framebuffer reaches the screen
    /// last.
    pub(crate) fn commit(&self, next: KmsState) -> DrmResult<()> {
        self.commit_with(|state| {
            *state = next;
            Ok(())
        })
    }

    /// Mutates the committed state and presents the result as one operation.
    ///
    /// This is the only writer of `state` and the scanout slot: validate,
    /// publish and present happen under the commit lock, so two clients cannot
    /// interleave such that the older framebuffer reaches the screen last.
    pub(crate) fn commit_with(
        &self,
        mutate: impl FnOnce(&mut KmsState) -> DrmResult<()>,
    ) -> DrmResult<()> {
        let _commit = self.commit.lock();
        let mut next = self.state.lock().clone();
        mutate(&mut next)?;
        if !next.plane_fb_id.is_none() && !self.has_framebuffer(next.plane_fb_id) {
            return Err(DrmError::InvalidInput);
        }
        // Present first, publish second: a failed present must leave the
        // committed state describing what is actually on screen. An inactive
        // CRTC shows nothing, so nothing is presented and the host keeps
        // scanning whatever it held (the slot retains it).
        // A generic commit no longer names the driver-created legacy blob (an
        // atomic commit replaced it with a client blob, or disabled the CRTC),
        // so retire it instead of leaving the id registered forever.
        let keeps_legacy_blob = self
            .legacy_mode
            .lock()
            .as_ref()
            .is_some_and(|(id, _)| next.crtc_mode.as_ref().is_some_and(|(mid, _)| mid == id));
        let presented = if next.crtc_active != 0 {
            next.plane_fb_id
        } else {
            FbId::default()
        };
        self.present_locked(presented)?;
        if !keeps_legacy_blob {
            self.clear_legacy_mode_blob();
        }
        *self.state.lock() = next;
        Ok(())
    }

    /// Validates a state proposal without publishing it (`MODE_ATOMIC` dry run).
    pub(crate) fn validate_with(
        &self,
        mutate: impl FnOnce(&mut KmsState) -> DrmResult<()>,
    ) -> DrmResult<()> {
        let _commit = self.commit.lock();
        let mut next = self.state.lock().clone();
        mutate(&mut next)?;
        if !next.plane_fb_id.is_none() && !self.has_framebuffer(next.plane_fb_id) {
            return Err(DrmError::InvalidInput);
        }
        Ok(())
    }

    /// Commits a legacy modeset and adopts `mode_blob` as the new legacy blob.
    ///
    /// The adoption happens under the same commit lock as the publish: doing it
    /// afterwards would let two concurrent modesets free each other's blob and
    /// leave the committed state pointing at a freed id.
    pub(crate) fn commit_legacy_modeset(
        &self,
        next: KmsState,
        mode_blob: BlobId,
        mode: Arc<Blob>,
    ) -> DrmResult<()> {
        let _commit = self.commit.lock();
        if !next.plane_fb_id.is_none() && !self.has_framebuffer(next.plane_fb_id) {
            return Err(DrmError::InvalidInput);
        }
        let presented = if next.crtc_active != 0 {
            next.plane_fb_id
        } else {
            FbId::default()
        };
        self.present_locked(presented)?;
        *self.state.lock() = next;
        self.replace_legacy_mode_blob(mode_blob, mode);
        Ok(())
    }

    /// Pushes updated pixels of `fb` to the host without changing the binding.
    ///
    /// `fb` zero means "the framebuffer currently on screen", which is what a
    /// client that lost track of its front buffer asks for. When nothing is
    /// bound there is nothing to flush.
    pub(crate) fn flush(&self, fb: FbId) -> DrmResult<()> {
        let _commit = self.commit.lock();
        let fb = if fb.is_none() {
            self.state.lock().plane_fb_id
        } else {
            fb
        };
        if fb.is_none() {
            return Ok(());
        }
        let Some((object, width, height)) = self.framebuffer_object(fb) else {
            return Err(DrmError::NotFound);
        };
        if !self.scanout.lock().is_bound(fb) {
            // The host is not scanning this resource, so there is no host copy
            // to update; its pixels are transferred when it is next presented.
            return Ok(());
        }
        self.backend()?.flush(
            object.resource()?,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
    }

    fn present_locked(&self, fb: FbId) -> DrmResult<()> {
        if fb.is_none() {
            // Disabling the plane does not stop the host from scanning the last
            // resource, so the slot keeps it until another present replaces it.
            self.scanout.lock().unbind();
            return Ok(());
        }
        let Some((object, width, height)) = self.framebuffer_object(fb) else {
            return Err(DrmError::NotFound);
        };
        // Resolve the backend and the resource before touching the slot: a
        // failure here must not leave the slot pointing at a resource that was
        // never presented.
        let backend = self.backend()?;
        let resource = object.resource()?;
        let rollback = object.clone();
        let previous = self.scanout.lock().install(fb, object);
        let rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        match backend.present(resource, rect) {
            Ok(()) => {
                // The host now scans the new resource; the previous one (and
                // any resource from an earlier failed present) can be retired.
                ScanoutSlot::release_retained(previous);
                Ok(())
            }
            Err(error) => {
                // A failed present means the host may or may not have switched:
                // keep the previous target and the attempted resource alive.
                self.scanout.lock().restore(previous, rollback);
                Err(error)
            }
        }
    }
}
