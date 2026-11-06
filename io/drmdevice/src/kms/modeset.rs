// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The KMS query, modeset and atomic ioctls (`drm_crtc.c`, `drm_atomic.c`).
//!
//! Every handler here reaches the committed state through the card's commit
//! path ([`DrmDevice::commit_with`] and friends) and reads it back through
//! [`DrmDevice::kms_state`]; none of them touch the state directly.

use alloc::sync::Arc;

use crate::{model::file::CLIENT_CAP_ATOMIC, prelude::*};

/// `DRM_IOCTL_MODE_SETCRTC`: the wire request is a `drm_mode_crtc`.
#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmModeSetCrtc(pub(crate) DrmModeCrtc);

/// The card's single CRTC.
pub(crate) const CRTC: CrtcId = CrtcId::from_raw(CRTC_ID);

/// Upper bound on objects in one atomic request.
///
/// Userspace names at most one object of each kind in this driver, so a larger
/// count can only be malformed. Rejecting it before `load_vm_vec` keeps a
/// malformed count from driving a huge kernel allocation.
const MAX_ATOMIC_OBJECTS: usize = 1024;

/// Upper bound on the total properties in one atomic request.
const MAX_ATOMIC_PROPERTIES: usize = 4096;

fn display_resolution(dev: &DrmDevice) -> (u32, u32) {
    dev.info()
        .map(|info| (info.width, info.height))
        .unwrap_or((640, 480))
}

/// Writes the decimal form of `value` at `cursor`, returning the new cursor.
///
/// The mode name lives in a fixed 32-byte array and this runs on the ioctl
/// path, so the conversion must not allocate.
fn write_decimal(out: &mut [u8], mut cursor: usize, value: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut len = 0;
    let mut rest = value;
    loop {
        digits[len] = b'0' + (rest % 10) as u8;
        len += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    while len > 0 {
        len -= 1;
        if cursor < out.len() {
            out[cursor] = digits[len];
            cursor += 1;
        }
    }
    cursor
}

/// Synthesizes the connector's single mode from the visible display size.
///
/// The name tracks the pixel size instead of being a constant: a host resize
/// changes both the dimensions and the name, so a client that re-reads the
/// mode list can tell the new mode from the one it had cached, the same way an
/// EDID-derived mode is named after its timing.
pub(crate) fn current_mode(dev: &DrmDevice) -> DrmModeModeInfo {
    let (width, height) = display_resolution(dev);
    mode_for_size(width, height)
}

/// Builds the connector's single mode for one visible size.
fn mode_for_size(w: u32, h: u32) -> DrmModeModeInfo {
    let mut name = [0u8; DRM_MODE_NAME_LEN];
    let mut cursor = write_decimal(&mut name, 0, w);
    if cursor < name.len() {
        name[cursor] = b'x';
        cursor += 1;
    }
    let _ = write_decimal(&mut name, cursor, h);
    DrmModeModeInfo {
        clock: 0,
        hdisplay: w as u16,
        hsync_start: w as u16,
        hsync_end: w as u16,
        htotal: w as u16,
        hskew: 0,
        vdisplay: h as u16,
        vsync_start: h as u16,
        vsync_end: h as u16,
        vtotal: h as u16,
        vscan: 0,
        vrefresh: 60,
        flags: 0,
        kind: 0,
        name,
    }
}

/// `DRM_IOCTL_MODE_GETRESOURCES`.
impl Ioctl for DrmModeCardRes {
    const NAME: &'static str = "MODE_GETRESOURCES";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, r: &mut Self) -> DrmResult<usize> {
        // Snapshot only this open file's IDs. Bound allocation by both the
        // actual list and the caller's capacity; count-only queries allocate
        // nothing. Drop the list lock before potentially faulting user copies.
        let (fb_count, fb_ids) = {
            let fbs = file.fbs.lock();
            let copy = (r.count_fbs as usize).min(fbs.len());
            let mut ids = alloc::vec::Vec::new();
            ids.try_reserve_exact(copy)
                .map_err(|_| DrmError::NoMemory)?;
            ids.extend(fbs.iter().take(copy).map(|id| id.raw()));
            (fbs.len() as u32, ids)
        };
        report_user_array(r.fb_id_ptr, r.count_fbs, &fb_ids)?;
        r.count_fbs = fb_count;
        // The visible size is a point inside this envelope, not the envelope:
        // reporting it as the maximum would make a client reject every larger
        // mode the host can still resize to.
        r.min_width = MIN_SCANOUT_WIDTH;
        r.max_width = MAX_SCANOUT_WIDTH;
        r.min_height = MIN_SCANOUT_HEIGHT;
        r.max_height = MAX_SCANOUT_HEIGHT;
        r.count_crtcs = report_user_array(r.crtc_id_ptr, r.count_crtcs, &[CRTC_ID])?;
        r.count_encoders = report_user_array(r.encoder_id_ptr, r.count_encoders, &[ENCODER_ID])?;
        r.count_connectors =
            report_user_array(r.connector_id_ptr, r.count_connectors, &[CONNECTOR_ID])?;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_GETCRTC`.
impl Ioctl for DrmModeCrtc {
    const NAME: &'static str = "MODE_GETCRTC";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, crtc: &mut Self) -> DrmResult<usize> {
        if crtc.crtc_id != CRTC_ID {
            return Err(DrmError::NotFound);
        }
        let state = dev.kms_state();
        crtc.gamma_size = 0;
        // A client can bind a framebuffer with an atomic FB_ID write without
        // ever setting MODE_ID; report the current display mode rather than a
        // zeroed one, and only report a connector while the CRTC is active.
        let bound = !state.plane_fb_id.is_none();
        crtc.x = state.plane_crtc_x.clamp(0, u32::MAX as i64) as u32;
        crtc.y = state.plane_crtc_y.clamp(0, u32::MAX as i64) as u32;
        crtc.fb_id = state.plane_fb_id.raw();
        crtc.mode_valid = 1;
        crtc.mode = state
            .crtc_mode
            .as_ref()
            .and_then(|(_, blob)| mode_from_bytes(blob.bytes().as_slice()))
            .unwrap_or_else(|| current_mode(dev));
        let connectors: &[u32] = if bound && state.crtc_active != 0 {
            &[CONNECTOR_ID]
        } else {
            &[]
        };
        crtc.count_connectors =
            report_user_array(crtc.set_connectors_ptr, crtc.count_connectors, connectors)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_SETCRTC`.
impl Ioctl for DrmModeSetCrtc {
    const NAME: &'static str = "MODE_SETCRTC";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, crtc: &mut Self) -> DrmResult<usize> {
        let c = &crtc.0;
        if c.crtc_id != CRTC_ID {
            return Err(DrmError::NotFound);
        }
        if c.fb_id == 0 {
            // A mode request with no framebuffer is a lookup failure in Linux,
            // not a disable: only `mode_valid == 0` may disable the CRTC.
            if c.mode_valid != 0 {
                return Err(DrmError::NotFound);
            }
            if c.count_connectors == 0 {
                // Disable: one commit with an inactive CRTC and no framebuffer.
                dev.commit(KmsState::default())?;
                return Ok(0);
            }
        }
        let fb = FbId::from_raw(c.fb_id);
        if fb.is_none() || !dev.has_framebuffer(fb) {
            return Err(DrmError::NotFound);
        }
        if c.count_connectors == 0 || c.set_connectors_ptr.is_null() {
            return Err(DrmError::InvalidInput);
        }
        if c.count_connectors > 16 {
            return Err(DrmError::InvalidInput);
        }
        let connectors: Vec<u32> = c
            .set_connectors_ptr
            .load_vm_vec(c.count_connectors as usize)
            .map_err(|_| DrmError::BadAddress)?;
        if connectors.iter().any(|&id| id != CONNECTOR_ID) {
            return Err(DrmError::NotFound);
        }
        let mode = if c.mode_valid != 0 {
            c.mode
        } else {
            current_mode(dev)
        };
        let (mode_id, mode_blob) =
            dev.create_blob(None, Arc::new(mode_blob_bytes(&mode)), false)?;
        let next = KmsState {
            crtc_active: 1,
            crtc_mode: Some((mode_id, mode_blob.clone())),
            conn_crtc_id: CRTC,
            plane_fb_id: fb,
            plane_crtc_id: CRTC,
            plane_src_x: 0,
            plane_src_y: 0,
            plane_src_w: (mode.hdisplay as u64) << 16,
            plane_src_h: (mode.vdisplay as u64) << 16,
            plane_crtc_x: 0,
            plane_crtc_y: 0,
            plane_crtc_w: mode.hdisplay as u64,
            plane_crtc_h: mode.vdisplay as u64,
        };
        // The driver created this blob for the modeset; adopting it under the
        // commit lock retires the previous one without racing another modeset.
        match dev.commit_legacy_modeset(next, mode_id, mode_blob.clone()) {
            Ok(()) => Ok(0),
            Err(error) => {
                dev.destroy_blob(mode_id);
                Err(error)
            }
        }
    }
}

/// `DRM_IOCTL_MODE_GETENCODER`.
impl Ioctl for DrmModeGetEncoder {
    const NAME: &'static str = "MODE_GETENCODER";

    fn handle(_dev: &DrmDevice, _file: &Arc<DrmFile>, e: &mut Self) -> DrmResult<usize> {
        if e.encoder_id != ENCODER_ID {
            return Err(DrmError::NotFound);
        }
        e.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
        e.crtc_id = CRTC_ID;
        e.possible_crtcs = 1;
        e.possible_clones = 0;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_GETCONNECTOR`.
impl Ioctl for DrmModeGetConnector {
    const NAME: &'static str = "MODE_GETCONNECTOR";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, c: &mut Self) -> DrmResult<usize> {
        if c.connector_id != CONNECTOR_ID {
            return Err(DrmError::NotFound);
        }
        // A host-side resize only shows up in a fresh query: this driver has no
        // hotplug notification path, so the mode list is refreshed here and the
        // display driver reports the change once it is acknowledged. The mode
        // and the geometry below come from the same snapshot, so a concurrent
        // resize cannot make them disagree.
        let _ = dev.refresh_info();
        let (width, height) = display_resolution(dev);
        c.encoder_id = ENCODER_ID;
        c.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
        c.connector_type_id = 1;
        c.connection = DRM_MODE_CONNECTED;
        // DisplayInfo supplies pixels, not physical dimensions or EDID.
        // Zero denotes an unknown physical size; deriving millimetres from
        // the visible mode would invent a DPI for userspace.
        c.mm_width = 0;
        c.mm_height = 0;
        c.subpixel = 0;
        c.count_encoders = report_user_array(c.encoders_ptr, c.count_encoders, &[ENCODER_ID])?;
        if !c.modes_ptr.is_null() && c.count_modes > 0 {
            c.modes_ptr
                .write_vm(mode_for_size(width, height))
                .map_err(|_| DrmError::BadAddress)?;
        }
        c.count_modes = 1;
        c.count_props = 0;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_PAGE_FLIP`.
impl Ioctl for DrmModeCrtcPageFlip {
    const NAME: &'static str = "MODE_PAGE_FLIP";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, f: &mut Self) -> DrmResult<usize> {
        if f.crtc_id != CRTC_ID {
            return Err(DrmError::NotFound);
        }
        let fb = FbId::from_raw(f.fb_id);
        if !dev.has_framebuffer(fb) {
            return Err(DrmError::NotFound);
        }
        // Reserve the completion event before presenting: a full queue must
        // fail the flip rather than leave userspace waiting for an event that
        // was silently dropped after the scanout already changed.
        let event = if f.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            Some(dev.reserve_flip_event().ok_or(DrmError::NoMemory)?)
        } else {
            None
        };
        // A legacy page flip changes the framebuffer of the CRTC; it does not
        // enable a disabled pipe (Linux keeps the pipe off for such a flip
        // too). The active check runs inside the commit lock, so a concurrent
        // disable cannot be reverted by the flip.
        dev.commit_with(|state| {
            if state.crtc_active == 0 {
                return Err(DrmError::InvalidInput);
            }
            state.plane_fb_id = fb;
            state.plane_crtc_id = CRTC;
            Ok(())
        })?;
        if let Some(event) = event {
            event.commit(CRTC_ID, f.user_data);
        }
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_GETPLANERES`.
impl Ioctl for DrmModeGetPlaneRes {
    const NAME: &'static str = "MODE_GETPLANERES";

    fn handle(_dev: &DrmDevice, _file: &Arc<DrmFile>, r: &mut Self) -> DrmResult<usize> {
        r.count_planes = report_user_array(r.plane_id_ptr, r.count_planes, &[PLANE_ID])?;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_GETPLANE`.
impl Ioctl for DrmModeGetPlane {
    const NAME: &'static str = "MODE_GETPLANE";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, p: &mut Self) -> DrmResult<usize> {
        if p.plane_id != PLANE_ID {
            return Err(DrmError::InvalidInput);
        }
        let state = dev.kms_state();
        p.crtc_id = state.plane_crtc_id.raw();
        p.fb_id = state.plane_fb_id.raw();
        p.possible_crtcs = 1;
        p.gamma_size = 0;
        p.count_format_types =
            report_user_array(p.format_type_ptr, p.count_format_types, SUPPORTED_FORMATS)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_ATOMIC`.
impl Ioctl for DrmModeAtomic {
    const NAME: &'static str = "MODE_ATOMIC";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, a: &mut Self) -> DrmResult<usize> {
        // A client that never negotiated the atomic capability is told the path
        // is unavailable so it falls back to the legacy one.
        if !file.has_cap(CLIENT_CAP_ATOMIC) {
            return Err(DrmError::OpNotSupported);
        }
        if !atomic_flags_valid(a.flags, a.reserved) {
            return Err(DrmError::InvalidInput);
        }
        let n = a.count_objs as usize;
        if n > MAX_ATOMIC_OBJECTS {
            return Err(DrmError::NoMemory);
        }
        let objs: Vec<u32> = a
            .objs_ptr
            .load_vm_vec(n)
            .map_err(|_| DrmError::BadAddress)?;
        let counts: Vec<u32> = a
            .count_props_ptr
            .load_vm_vec(n)
            .map_err(|_| DrmError::BadAddress)?;
        let total = counts
            .iter()
            .try_fold(0usize, |total, count| total.checked_add(*count as usize));
        let Some(total) = total.filter(|total| *total <= MAX_ATOMIC_PROPERTIES) else {
            return Err(DrmError::NoMemory);
        };
        let props: Vec<u32> = a
            .props_ptr
            .load_vm_vec(total)
            .map_err(|_| DrmError::BadAddress)?;
        let values: Vec<u64> = a
            .prop_values_ptr
            .load_vm_vec(total)
            .map_err(|_| DrmError::BadAddress)?;

        // The object/property walk is replayed against the state the commit
        // lock protects, so no concurrent commit can be silently reverted.
        let apply_all = |state: &mut KmsState| -> DrmResult<()> {
            let mut index = 0;
            for (object_index, &object_id) in objs.iter().enumerate() {
                let object = ObjectType::of_id(object_id).ok_or(DrmError::NotFound)?;
                for _ in 0..counts[object_index] {
                    let prop = PropertyId::from_raw(props[index]);
                    let value = values[index];
                    index += 1;
                    if !apply(state, object, prop, value, dev)? {
                        return Err(DrmError::NotFound);
                    }
                }
            }
            Ok(())
        };
        if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
            dev.validate_with(apply_all)?;
            return Ok(0);
        }
        let event = if a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            Some(dev.reserve_flip_event().ok_or(DrmError::NoMemory)?)
        } else {
            None
        };
        dev.commit_with(apply_all)?;
        if let Some(event) = event {
            event.commit(CRTC_ID, a.user_data);
        }
        Ok(0)
    }
}
