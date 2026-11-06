// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Framebuffer objects (`drm_framebuffer.c`).
//!
//! A framebuffer is a device-wide object that names one GEM object; it keeps
//! that object alive, which is what makes "close the handle first, destroy the
//! framebuffer later" safe without a parking table.

use crate::prelude::*;

/// One registered framebuffer.
pub(crate) struct Framebuffer {
    /// Backing object; holding it keeps the resource alive.
    pub(crate) object: alloc::sync::Arc<GemObject>,
    /// File that created the framebuffer, for `fb->filp` accounting.
    pub(crate) owner: Weak<DrmFile>,
}

/// `DRM_IOCTL_MODE_RMFB`: the wire request is a bare framebuffer id.
#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmModeRmFb(pub(crate) u32);

impl Framebuffer {
    /// Creates a framebuffer over one dumb handle of `file`.
    fn from_handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        handle: u32,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> DrmResult<alloc::sync::Arc<GemObject>> {
        let object = file
            .handle(DumbHandle::from_raw(handle))
            .ok_or(DrmError::InvalidInput)?;
        // The scanout resource is created at exactly the dumb buffer's size and
        // pitch, and TRANSFER_TO_HOST_2D reads it row by row. A sub-size
        // framebuffer would be misread, so the layouts must match exactly.
        if width != object.width() || height != object.height() || pitch != object.pitch() {
            return Err(DrmError::InvalidInput);
        }
        dev.backend()?;
        Ok(object)
    }
}

/// `DRM_IOCTL_MODE_ADDFB`.
impl Ioctl for DrmModeFbCmd {
    const NAME: &'static str = "MODE_ADDFB";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, f: &mut Self) -> DrmResult<usize> {
        // Legacy ADDFB: translate into the modern form so the two entry points
        // cannot drift apart. There is no pixel_format field; the legacy ABI
        // implies XRGB8888 for depth 24/32.
        if f.handle == 0 || !matches!(f.bpp, 24 | 32) || !matches!(f.depth, 24 | 32) {
            return Err(DrmError::InvalidInput);
        }
        let mut modern = DrmModeFbCmd2 {
            fb_id: f.fb_id,
            width: f.width,
            height: f.height,
            pixel_format: DRM_FORMAT_XRGB8888,
            flags: 0,
            handles: [f.handle, 0, 0, 0],
            pitches: [f.pitch, 0, 0, 0],
            offsets: [0; 4],
            abi_pad: 0,
            modifier: [0; 4],
        };
        Ioctl::handle(dev, file, &mut modern)?;
        f.fb_id = modern.fb_id;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_ADDFB2`.
impl Ioctl for DrmModeFbCmd2 {
    const NAME: &'static str = "MODE_ADDFB2";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, f: &mut Self) -> DrmResult<usize> {
        let handle = f.handles[0];
        if handle == 0 || f.offsets[0] != 0 {
            return Err(DrmError::InvalidInput);
        }
        for i in 1..4 {
            if f.handles[i] != 0 || f.pitches[i] != 0 || f.offsets[i] != 0 {
                return Err(DrmError::InvalidInput);
            }
        }
        if !matches!(f.pixel_format, DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888) {
            return Err(DrmError::InvalidInput);
        }
        if f.flags & DRM_MODE_FB_MODIFIERS != 0 {
            for i in 0..4 {
                if f.handles[i] == 0 {
                    continue;
                }
                let modifier = f.modifier[i];
                if modifier != DRM_FORMAT_MOD_LINEAR && modifier != DRM_FORMAT_MOD_INVALID {
                    return Err(DrmError::InvalidInput);
                }
            }
        }
        let object = Framebuffer::from_handle(dev, file, handle, f.width, f.height, f.pitches[0])?;
        let id = dev.add_framebuffer(file, object)?;
        f.fb_id = id.raw();
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_RMFB`.
impl Ioctl for DrmModeRmFb {
    const NAME: &'static str = "MODE_RMFB";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, fb: &mut Self) -> DrmResult<usize> {
        // `drm_mode_rmfb_ioctl()` walks the caller's `file_priv->fbs`, so an
        // unknown id and another file's id are both ENOENT, and only an
        // owned framebuffer is unbound from the plane and the scanout slot.
        dev.remove_framebuffer(file, FbId::from_raw(fb.0))?;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_CLOSEFB`.
impl Ioctl for DrmModeCloseFb {
    const NAME: &'static str = "MODE_CLOSEFB";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, args: &mut Self) -> DrmResult<usize> {
        // CLOSEFB is RMFB plus "refuse while it is on screen": it tells a client
        // that its framebuffer is still in use instead of pulling the scanout
        // out from under it.
        if args.pad != 0 {
            return Err(DrmError::InvalidInput);
        }
        // The "still in use" test and the removal run under the commit lock,
        // so a concurrent commit cannot bind the framebuffer in between.
        dev.close_framebuffer(file, FbId::from_raw(args.fb_id))?;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_DIRTYFB`.
impl Ioctl for DrmModeDirtyFb {
    const NAME: &'static str = "MODE_DIRTYFB";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, dirty: &mut Self) -> DrmResult<usize> {
        // `drm_mode_dirtyfb_ioctl()` looks the framebuffer up first (v4.9
        // through master: no `fb_id == 0` branch), so an unknown id — including
        // zero — is ENOENT even when the rest of the request is malformed.
        // Xorg keeps its damage tracking enabled only when the generic path
        // answers ENOENT rather than EINVAL.
        if !dev.has_framebuffer(FbId::from_raw(dirty.fb_id)) {
            return Err(DrmError::NotFound);
        }
        // Then the flag bits the driver does not know are masked off (Linux
        // does not reject them), and the clip protocol is validated: a count
        // and a pointer are both present or both absent, ANNOTATE_COPY takes
        // clips in pairs, the count is bounded, and the array must be readable.
        let flags = dirty.flags & DRM_MODE_FB_DIRTY_FLAGS;
        if (dirty.num_clips == 0) != (dirty.clips_ptr == 0) {
            return Err(DrmError::InvalidInput);
        }
        if flags & DRM_MODE_FB_DIRTY_ANNOTATE_COPY != 0 && !dirty.num_clips.is_multiple_of(2) {
            return Err(DrmError::InvalidInput);
        }
        if dirty.num_clips > DRM_MODE_FB_DIRTY_MAX_CLIPS {
            return Err(DrmError::InvalidInput);
        }
        if dirty.num_clips != 0 {
            // This driver flushes whole frames, but an unreadable clip array is
            // EFAULT in Linux because it copies the rectangles before calling
            // into the driver.
            let clips = UserPtr::<DrmClipRect>::from(dirty.clips_ptr as usize);
            clips
                .load_vm_vec(dirty.num_clips as usize)
                .map_err(|_| DrmError::BadAddress)?;
        }
        // A dirty flush pushes pixels; it never changes the scanout binding.
        dev.flush(FbId::from_raw(dirty.fb_id))?;
        Ok(0)
    }
}

impl DrmDevice {
    /// Removes `fb` unless it is bound or on screen (`MODE_CLOSEFB`).
    ///
    /// The ownership check, the "still in use" check and the removal happen
    /// under the commit lock, so a concurrent commit cannot bind the
    /// framebuffer between them.  A file may only close a framebuffer it
    /// created; Linux reports another file's id as `ENOENT` rather than
    /// revealing that it exists.
    pub(crate) fn close_framebuffer(&self, file: &Arc<DrmFile>, fb: FbId) -> DrmResult<()> {
        let _commit = self.commit.lock();
        if !file.fbs.lock().contains(&fb) || !self.framebuffers.lock().contains(fb) {
            return Err(DrmError::NotFound);
        }
        if self.kms_state().plane_fb_id == fb || self.is_on_scanout(fb) {
            return Err(DrmError::Busy);
        }
        self.unbind_and_remove(fb).ok_or(DrmError::NotFound)?;
        Ok(())
    }

    /// Whether `fb` names a live framebuffer.
    pub(crate) fn has_framebuffer(&self, fb: FbId) -> bool {
        self.framebuffers.lock().contains(fb)
    }

    /// Object, width and height behind `fb`.
    pub(crate) fn framebuffer_object(&self, fb: FbId) -> Option<(Arc<GemObject>, u32, u32)> {
        self.framebuffers
            .lock()
            .get(fb)
            .map(|fb| (fb.object.clone(), fb.object.width(), fb.object.height()))
    }

    /// Registers a framebuffer and records it on the creating file.
    pub(crate) fn add_framebuffer(
        &self,
        file: &Arc<DrmFile>,
        object: Arc<GemObject>,
    ) -> DrmResult<FbId> {
        let id = self
            .framebuffers
            .lock()
            .insert(Framebuffer {
                object,
                owner: Arc::downgrade(file),
            })
            .ok_or(DrmError::NoMemory)?;
        file.fbs.lock().insert(id);
        Ok(id)
    }

    /// Removes a framebuffer owned by `file` and unbinds it from the plane.
    ///
    /// Runs under the commit lock: the ownership test, unbinding the plane,
    /// dropping the registry entry and updating the creating file are one
    /// step, so a concurrent commit cannot publish or re-present a
    /// framebuffer that is being removed.  Checking ownership before touching
    /// the scanout or committed state also means a failed `RMFB` (unknown or
    /// foreign id, including zero) leaves everything unchanged.
    /// The scanout resource stays retained until another present replaces it —
    /// the host may still be scanning it.
    pub(crate) fn remove_framebuffer(&self, file: &Arc<DrmFile>, fb: FbId) -> DrmResult<()> {
        let _commit = self.commit.lock();
        if !file.fbs.lock().contains(&fb) {
            return Err(DrmError::NotFound);
        }
        self.unbind_and_remove(fb).ok_or(DrmError::NotFound)?;
        Ok(())
    }

    /// Unbinds and removes a framebuffer; the caller holds the commit lock.
    fn unbind_and_remove(&self, fb: FbId) -> Option<()> {
        if self.scanout.lock().is_bound(fb) {
            self.scanout.lock().unbind();
        }
        {
            let mut state = self.state.lock();
            if state.plane_fb_id == fb {
                state.plane_fb_id = FbId::default();
                state.plane_crtc_id = CrtcId::default();
            }
        }
        let removed = self.framebuffers.lock().remove(fb)?;
        if let Some(owner) = removed.owner.upgrade() {
            owner.fbs.lock().remove(&fb);
        }
        Some(())
    }

    /// Whether `fb` is currently bound to the plane.
    pub(crate) fn is_on_scanout(&self, fb: FbId) -> bool {
        self.scanout.lock().is_bound(fb)
    }
}
