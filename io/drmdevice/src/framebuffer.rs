// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Framebuffer objects (`drm_framebuffer.c`).
//!
//! A framebuffer is a device-wide object that names one GEM object; it keeps
//! that object alive, which is what makes "close the handle first, destroy the
//! framebuffer later" safe without a parking table.

use crate::{prelude::*, sync::dma_resv::DmaResv};

/// One registered framebuffer.
pub(crate) struct Framebuffer {
    /// Backing object; holding it keeps the resource alive.
    pub(crate) object: alloc::sync::Arc<GemObject>,
    /// File that created the framebuffer, for `fb->filp` accounting.
    pub(crate) owner: Weak<DrmFile>,
    /// The fence set a client must wait for before this framebuffer is safe to
    /// scan out or scan from.
    ///
    /// This is Linux's `drm_framebuffer::resv` plus its per-buffer
    /// `DMA_RESV_USAGE_WRITE` slot: the object's reservation describes the
    /// memory, and this slot records the one extra fence the framebuffer
    /// itself adds (the commit that last displayed it). Keeping the two apart
    /// is what lets a framebuffer over an imported dma-buf carry both its
    /// importer's ordering and the card's own display ordering.
    pub(crate) resv: Arc<DmaResv>,
    /// Format, size, stride and byte offset the framebuffer names.
    geometry: ResourceSpec,
    /// Byte offset of the first pixel inside the object.
    offset: u64,
}

/// `DRM_IOCTL_MODE_RMFB`: the wire request is a bare framebuffer id.
#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmModeRmFb(pub(crate) u32);

impl Framebuffer {
    /// Resolves the object one framebuffer plane names.
    ///
    /// This is `drm_gem_object_lookup` plus `drm_gem_fb_init_with_funcs`'s
    /// size rule: the object may be a dumb buffer or an imported dma-buf, and
    /// the framebuffer must fit inside it. Nothing here requires the
    /// framebuffer to have the object's creation-time geometry — a client
    /// that lays out its own planes (a compositor scanning a window of a
    /// larger allocation) gets a framebuffer of its own size.
    fn from_handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        plane: &FramebufferPlane,
    ) -> DrmResult<alloc::sync::Arc<GemObject>> {
        let object = file
            .handle(DumbHandle::from_raw(plane.handle))
            .ok_or(DrmError::InvalidInput)?;
        dev.backend()?;
        // `objs[i]->size < min_size` → `EINVAL`: the last row starts at
        // `(height - 1) * pitch`, followed by at least one pixel.
        let visible = u64::from(plane.height)
            .checked_sub(1)
            .and_then(|rows| rows.checked_mul(u64::from(plane.pitch)))
            .and_then(|last_row| last_row.checked_add(u64::from(plane.bytes_per_pixel)))
            .and_then(|visible| visible.checked_add(plane.offset))
            .ok_or(DrmError::InvalidInput)?;
        if object.size_bytes() < visible {
            return Err(DrmError::InvalidInput);
        }
        Ok(object)
    }

    /// Whether this framebuffer covers its object's whole memory.
    ///
    /// A framebuffer with exactly the object's creation-time layout presents
    /// the object's own resource; every other one needs a window resource.
    pub(crate) fn covers_object(&self) -> bool {
        self.offset == 0
            && self.geometry.pitch == self.object.pitch()
            && self.geometry.height == self.object.height()
            && self.geometry.width == self.object.width()
            && self.geometry.format == self.object.spec().format
    }

    /// The resource layout this framebuffer names.
    pub(crate) const fn geometry(&self) -> ResourceSpec {
        self.geometry
    }

    /// Byte offset of the first pixel inside the object.
    pub(crate) const fn offset(&self) -> Option<u64> {
        Some(self.offset)
    }
}

/// One plane of an `ADDFB2` request, validated.
struct FramebufferPlane {
    handle: u32,
    width: u32,
    height: u32,
    pitch: u32,
    offset: u64,
    bytes_per_pixel: u32,
}

impl FramebufferPlane {
    fn spec(&self) -> ResourceSpec {
        ResourceSpec {
            width: self.width,
            height: self.height,
            pitch: self.pitch,
            format: ScanoutFormat::Bgra8888,
        }
    }
}

/// `DRM_IOCTL_MODE_ADDFB`.
impl Ioctl for DrmModeFbCmd {
    const NAME: &'static str = "MODE_ADDFB";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, f: &mut Self) -> DrmResult<usize> {
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
        Ioctl::handle(dev, file, 0, &mut modern)?;
        f.fb_id = modern.fb_id;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_ADDFB2`.
impl Ioctl for DrmModeFbCmd2 {
    const NAME: &'static str = "MODE_ADDFB2";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, f: &mut Self) -> DrmResult<usize> {
        // `drm_internal_framebuffer_create` → `framebuffer_check` → the
        // driver's `fb_create`. The driver side is virtio-gpu's
        // `virtio_gpu_user_framebuffer_create`: it supports XRGB8888/ARGB8888
        // (answering `ENOENT` for any other format, which is what makes a
        // client fall back to the format it knows) and does not check any
        // plane against an object's size — the generic helper does.
        let plane = validate_planes(f)?;
        if !matches!(f.pixel_format, DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888) {
            return Err(DrmError::NotFound);
        }
        let object = Framebuffer::from_handle(dev, file, &plane)?;
        let id = dev.add_framebuffer(file, object, plane.spec(), plane.offset)?;
        f.fb_id = id.raw();
        Ok(0)
    }
}

/// Checks one `ADDFB2` request the way Linux's `framebuffer_check` does.
///
/// The format is one the driver handles, the size is non-zero, the object
/// handle is present, the stride holds at least one pixel, and the unused
/// planes are empty. A modifier may only be present with
/// `DRM_MODE_FB_MODIFIERS`, and the only modifier this driver accepts is
/// `LINEAR` (or none).
fn validate_planes(f: &DrmModeFbCmd2) -> DrmResult<FramebufferPlane> {
    if f.flags & !DRM_MODE_FB_FLAGS != 0 {
        return Err(DrmError::InvalidInput);
    }
    if f.width == 0 || f.height == 0 || f.handles[0] == 0 {
        return Err(DrmError::InvalidInput);
    }
    // One plane: the format every scanout-capable object of this card has.
    if u64::from(f.pitches[0]) < u64::from(f.width) * 4 {
        return Err(DrmError::InvalidInput);
    }
    if (u64::from(f.height))
        .checked_mul(u64::from(f.pitches[0]))
        .and_then(|visible| visible.checked_add(u64::from(f.offsets[0])))
        .is_none_or(|visible| visible > u64::from(u32::MAX))
    {
        return Err(DrmError::InvalidInput);
    }
    for i in 0..4 {
        let modifier = f.modifier[i];
        if modifier != 0 && f.flags & DRM_MODE_FB_MODIFIERS == 0 {
            return Err(DrmError::InvalidInput);
        }
        if i > 0 && (f.handles[i] != 0 || f.pitches[i] != 0 || f.offsets[i] != 0) {
            return Err(DrmError::InvalidInput);
        }
        if i == 0 {
            continue;
        }
        if modifier != 0 {
            return Err(DrmError::InvalidInput);
        }
    }
    // A client that sends `DRM_FORMAT_MOD_INVALID` asks for whatever the
    // driver supports; only LINEAR is supported, and any other code is
    // refused so the client learns to fall back to LINEAR.
    if !matches!(
        f.modifier[0],
        DRM_FORMAT_MOD_LINEAR | DRM_FORMAT_MOD_INVALID
    ) {
        return Err(DrmError::InvalidInput);
    }
    Ok(FramebufferPlane {
        handle: f.handles[0],
        width: f.width,
        height: f.height,
        pitch: f.pitches[0],
        offset: u64::from(f.offsets[0]),
        bytes_per_pixel: 4,
    })
}

/// `DRM_IOCTL_MODE_RMFB`.
impl Ioctl for DrmModeRmFb {
    const NAME: &'static str = "MODE_RMFB";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, fb: &mut Self) -> DrmResult<usize> {
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

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        args: &mut Self,
    ) -> DrmResult<usize> {
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

    fn handle(
        dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        dirty: &mut Self,
    ) -> DrmResult<usize> {
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

    /// Geometry and object of the framebuffer behind `fb`.
    ///
    /// This is `drm_framebuffer_lookup` without the `filp` check: the caller
    /// decides whether the file may name it.
    pub(crate) fn framebuffer_geometry(
        &self,
        fb: FbId,
    ) -> Option<(Arc<GemObject>, ResourceSpec, u64)> {
        self.framebuffers
            .lock()
            .get(fb)
            .map(|fb| (fb.object.clone(), fb.geometry, fb.offset))
    }

    /// Registers a framebuffer and records it on the creating file.
    pub(crate) fn add_framebuffer(
        &self,
        file: &Arc<DrmFile>,
        object: Arc<GemObject>,
        geometry: ResourceSpec,
        offset: u64,
    ) -> DrmResult<FbId> {
        let id = self
            .framebuffers
            .lock()
            .insert(Framebuffer {
                object,
                owner: Arc::downgrade(file),
                resv: crate::sync::dma_resv::DmaResv::new(),
                geometry,
                offset,
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
        {
            // One guard: `is_bound` and `unbind` must observe the same slot,
            // and a second acquisition can interleave with a present.
            let mut scanout = self.scanout.lock();
            if scanout.is_bound(fb) {
                scanout.unbind();
            }
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

/// `DRM_IOCTL_MODE_GETFB2`.
///
/// `drm_mode_getfb2_ioctl`: the descriptor must name the calling file's
/// framebuffer, the geometry comes from the stored framebuffer, and the
/// per-plane handles are only filled for the master (an unprivileged client
/// gets zeros rather than a handle it could not have obtained otherwise).
impl Ioctl for DrmModeGetFb2 {
    const NAME: &'static str = "MODE_GETFB2";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, r: &mut Self) -> DrmResult<usize> {
        let request = &mut r.0;
        let id = FbId::from_raw(request.fb_id);
        let (object, geometry, offset) = dev.framebuffer_geometry(id).ok_or(DrmError::NotFound)?;
        // `drm_framebuffer_lookup` only finds a framebuffer the calling file
        // created or opened.
        if !file.fbs.lock().contains(&id) {
            return Err(DrmError::NotFound);
        }
        request.width = geometry.width;
        request.height = geometry.height;
        request.pixel_format = DRM_FORMAT_XRGB8888;
        request.flags = 0;
        for index in 0..MAX_PLANES {
            request.handles[index] = 0;
            request.pitches[index] = 0;
            request.offsets[index] = 0;
            request.modifier[index] = 0;
        }
        request.pitches[0] = geometry.pitch;
        request.offsets[0] = u32::try_from(offset).unwrap_or(0);
        // Only the master receives a handle: the buffer may be another
        // client's, and `GETFB2` is an unprivileged query.
        if !dev.is_master(file) {
            return Ok(0);
        }
        let handle = file
            .handles
            .lock()
            .iter()
            .find_map(|(handle, known)| Arc::ptr_eq(known, &object).then_some(*handle));
        request.handles[0] = match handle {
            Some(handle) => handle.raw(),
            None => {
                let handle = object.import_handle();
                file.handles.lock().insert(handle, object);
                handle.raw()
            }
        };
        Ok(0)
    }
}

/// Planes a `drm_mode_fb_cmd2` request names.
const MAX_PLANES: usize = 4;

/// `DRM_IOCTL_MODE_GETFB`: the legacy framebuffer query.
///
/// One wire struct, one more command: the metadata is the legacy ADDFB view of
/// the framebuffer, and the handle follows `drm_mode_getfb` — a fresh handle
/// for the master, zeros for anyone else.
#[repr(transparent)]
#[derive(
    Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite,
)]
pub(crate) struct DrmModeGetFb(pub(crate) DrmModeFbCmd);

impl Ioctl for DrmModeGetFb {
    const NAME: &'static str = "MODE_GETFB";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, r: &mut Self) -> DrmResult<usize> {
        let id = FbId::from_raw(r.0.fb_id);
        let (object, geometry, _offset) = dev.framebuffer_geometry(id).ok_or(DrmError::NotFound)?;
        if !file.fbs.lock().contains(&id) {
            return Err(DrmError::NotFound);
        }
        r.0.width = geometry.width;
        r.0.height = geometry.height;
        r.0.pitch = geometry.pitch;
        r.0.bpp = 32;
        r.0.depth = 24;
        if !dev.is_master(file) {
            r.0.handle = 0;
            return Ok(0);
        }
        let owned = file
            .handles
            .lock()
            .iter()
            .find_map(|(handle, known)| Arc::ptr_eq(known, &object).then_some(*handle));
        r.0.handle = match owned {
            Some(handle) => handle.raw(),
            None => {
                let handle = object.import_handle();
                file.handles.lock().insert(handle, object);
                handle.raw()
            }
        };
        Ok(0)
    }
}
