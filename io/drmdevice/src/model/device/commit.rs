// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Commit and present methods of the card.
//!
//! The committed KMS state has one writer: [`DrmDevice::commit_with`]. The
//! validate, publish and present steps run under the commit lock, so the
//! state and what is on screen cannot disagree. The KMS ioctls in `kms.rs`
//! reach the committed state only through these methods.

use crate::{model::scanout::PendingRetire, prelude::*};

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
        let was_active = self.state.lock().crtc_active != 0;
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
        let still_active = next.crtc_active != 0;
        let refresh = refresh_hz(&next);
        *self.state.lock() = next;
        self.sync_vblank_timer(was_active, still_active, refresh);
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
        let was_active = self.state.lock().crtc_active != 0;
        if !next.plane_fb_id.is_none() && !self.has_framebuffer(next.plane_fb_id) {
            return Err(DrmError::InvalidInput);
        }
        let presented = if next.crtc_active != 0 {
            next.plane_fb_id
        } else {
            FbId::default()
        };
        self.present_locked(presented)?;
        let mut next = next;
        // A legacy modeset has no in-fence or out-fence of its own, so the
        // published state carries neither forward.
        next.plane_in_fence = None;
        next.req_out_fence_ptr = 0;
        let still_active = next.crtc_active != 0;
        let refresh = refresh_hz(&next);
        *self.state.lock() = next;
        self.replace_legacy_mode_blob(mode_blob, mode);
        self.sync_vblank_timer(was_active, still_active, refresh);
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
        // Resolve what the host is actually scanning under the slot lock: a
        // framebuffer smaller than its object is displayed through a view
        // resource, and flushing the object's own resource would update pixels
        // nobody sees.
        let scanning_resource = {
            let scanout = self.scanout.lock();
            if !scanout.is_bound(fb) {
                // The host is not scanning this resource, so there is no host
                // copy to update; its pixels are transferred when it is next
                // presented.
                return Ok(());
            }
            scanout.scanning_resource()
        };
        let Some(resource) = scanning_resource else {
            return self.backend()?.flush(
                object.resource()?,
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                object.is_host_rendered(),
            );
        };
        // The view resource has the framebuffer's geometry, not the object's.
        let (width, height) = self
            .framebuffers
            .lock()
            .get(fb)
            .map(|framebuffer| {
                let geometry = framebuffer.geometry();
                (geometry.width, geometry.height)
            })
            .unwrap_or((width, height));
        self.backend()?.flush(
            resource,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            object.is_host_rendered(),
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
        // The transfer rectangle must stay inside the resource this present
        // targets. A framebuffer that does not cover its whole object is shown
        // through a view resource created from the framebuffer's own geometry
        // (`scanout_resource`), which may be smaller than the object: an object
        // created with 832 rows can hold a 1280x800 framebuffer. Reporting the
        // object's size here would ask the host to transfer rows the view does
        // not have, and the display rejects the command.
        let (width, height) = self
            .framebuffers
            .lock()
            .get(fb)
            .map(|framebuffer| {
                let geometry = framebuffer.geometry();
                (geometry.width, geometry.height)
            })
            .unwrap_or((width, height));
        // Resolve the backend and the resource before touching the slot: a
        // failure here must not leave the slot pointing at a resource that was
        // never presented.
        let backend = self.backend()?;
        let (resource, owned) = self.scanout_resource(fb, &object)?;
        let host_rendered = object.is_host_rendered();
        let rollback = PendingRetire::new(object.clone(), owned);
        log::debug!(
            "drm: presenting {fb:?} resource {resource:?} owned {owned:?} \
             host_rendered={host_rendered}"
        );
        let previous = self.scanout.lock().install(fb, object, owned);
        let rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        match backend.present(resource, rect, host_rendered) {
            Ok(()) => {
                // The host now scans the new resource; the previous one (and
                // any resource from an earlier failed present) can be retired.
                log::debug!("drm: present ok for {fb:?}, retiring previous");
                ScanoutSlot::release_replaced(previous);
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

    /// The resource a framebuffer presents, and the slot's ownership of it.
    ///
    /// A framebuffer that covers its object's whole memory presents the
    /// object's own resource; every other one gets a resource for its window,
    /// created on first use and destroyed when the host stops scanning it.
    fn scanout_resource(
        &self,
        fb: FbId,
        object: &Arc<GemObject>,
    ) -> DrmResult<(ResourceId, Option<ResourceId>)> {
        let whole = self
            .framebuffers
            .lock()
            .get(fb)
            .is_some_and(|framebuffer| framebuffer.covers_object());
        if whole {
            return Ok((object.resource()?, None));
        }
        let framebuffer = self
            .framebuffers
            .lock()
            .get(fb)
            .map(|framebuffer| (framebuffer.geometry(), framebuffer.offset().unwrap_or(0)));
        let Some((geometry, offset)) = framebuffer else {
            return Err(DrmError::NotFound);
        };
        let resource = self.next_resource_id();
        object.create_view_resource(geometry, offset, resource)?;
        Ok((resource, Some(resource)))
    }
}

impl DrmDevice {
    /// Starts or stops the timer-fallback timeline with the CRTC.
    ///
    /// Linux arms vblank in the CRTC enable path (`drm_crtc_vblank_on`) and
    /// disarms it on disable; the timer fallback follows the same edge and
    /// refreshes its rate while the CRTC stays active. Pending flips cannot
    /// reach a stopped clock, so disabling the CRTC completes them now.
    fn sync_vblank_timer(&self, was_active: bool, is_active: bool, refresh: u32) {
        if is_active {
            // A real display source and the timer fallback must not advance
            // the same timeline: only create the timer when the backend has
            // no source to subscribe to.
            if self.start_scanout_source().is_ok() {
                self.stop_vblank_timer();
            } else {
                self.start_vblank_timer(refresh);
            }
        } else if was_active {
            self.stop_vblank_timer();
            self.complete_flips();
        }
    }
}

/// The refresh rate the timer fallback should tick at.
///
/// The committed mode carries the rate; a card without a mode falls back to
/// 60 Hz, the rate the driver's synthesized timing runs at.
fn refresh_hz(state: &KmsState) -> u32 {
    state
        .crtc_mode
        .as_ref()
        .and_then(|(_, blob)| crate::kms::mode_from_bytes(blob.bytes().as_slice()))
        .map_or(60, |mode| mode.vrefresh.max(1))
}
