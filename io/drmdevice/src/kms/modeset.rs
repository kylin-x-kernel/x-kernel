// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The KMS query, modeset and atomic ioctls (`drm_crtc.c`, `drm_atomic.c`).
//!
//! Every handler here reaches the committed state through the card's commit
//! path ([`DrmDevice::commit_with`] and friends) and reads it back through
//! [`DrmDevice::kms_state`]; none of them touch the state directly.

use alloc::sync::Arc;

use super::props::PROPS;
use crate::{
    decimal::write_decimal,
    model::{file::CLIENT_CAP_ATOMIC, flip::OutFence},
    prelude::*,
    sync::fence_list::FenceList,
};

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

/// Synthesizes the connector's single mode from the visible display size.
///
/// The name tracks the pixel size instead of being a constant: a host resize
/// changes both the dimensions and the name, so a client that re-reads the
/// mode list can tell the new mode from the one it had cached, the same way an
/// EDID-derived mode is named after its timing.
pub(crate) fn current_mode(dev: &DrmDevice) -> Option<DrmModeModeInfo> {
    let (width, height) = display_resolution(dev);
    mode_for_size(width, height)
}

/// Builds the connector's single mode for one visible size.
fn mode_for_size(width: u32, height: u32) -> Option<DrmModeModeInfo> {
    let hdisplay = u16::try_from(width).ok()?;
    let vdisplay = u16::try_from(height).ok()?;
    if hdisplay == 0 || vdisplay == 0 {
        return None;
    }
    let htotal = hdisplay.checked_add(160)?;
    let vtotal = vdisplay.checked_add(30)?;
    let vrefresh = 60;
    // The UAPI clock is in kHz. Multiplying in Hz needs a wider intermediate.
    let clock = (u64::from(htotal) * u64::from(vtotal) * u64::from(vrefresh) / 1000)
        .try_into()
        .ok()?;
    let mut name = [0u8; DRM_MODE_NAME_LEN];
    let mut cursor = write_decimal(&mut name, 0, width);
    if cursor < name.len() {
        name[cursor] = b'x';
        cursor += 1;
    }
    let _ = write_decimal(&mut name, cursor, height);
    Some(DrmModeModeInfo {
        clock,
        hdisplay,
        hsync_start: hdisplay + 8,
        hsync_end: hdisplay + 40,
        htotal,
        hskew: 0,
        vdisplay,
        vsync_start: vdisplay + 3,
        vsync_end: vdisplay + 6,
        vtotal,
        vscan: 0,
        vrefresh,
        flags: 0,
        kind: 0,
        name,
    })
}

/// `DRM_IOCTL_MODE_GETRESOURCES`.
impl Ioctl for DrmModeCardRes {
    const NAME: &'static str = "MODE_GETRESOURCES";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, r: &mut Self) -> DrmResult<usize> {
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

    fn handle(
        dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        crtc: &mut Self,
    ) -> DrmResult<usize> {
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
        let mode = state
            .crtc_mode
            .as_ref()
            .and_then(|(_, blob)| mode_from_bytes(blob.bytes().as_slice()))
            .or_else(|| current_mode(dev));
        crtc.mode_valid = u32::from(mode.is_some());
        crtc.mode = mode.unwrap_or_default();
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

    fn handle(
        dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        crtc: &mut Self,
    ) -> DrmResult<usize> {
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
            current_mode(dev).ok_or(DrmError::InvalidInput)?
        };
        let (mode_id, mode_blob) =
            dev.create_blob(None, Arc::new(mode_blob_bytes(&mode)), false)?;
        let next = KmsState {
            crtc_active: 1,
            crtc_mode: Some((mode_id, mode_blob.clone())),
            conn_crtc_id: CRTC,
            conn_max_bpc: 8,
            conn_dpms: DRM_MODE_DPMS_ON,
            conn_link_status: DRM_MODE_LINK_STATUS_GOOD,
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
            plane_in_fence: None,
            plane_damage_clips: None,
            req_out_fence_ptr: 0,
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

    fn handle(
        _dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        e: &mut Self,
    ) -> DrmResult<usize> {
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

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, _unit: u32, c: &mut Self) -> DrmResult<usize> {
        if c.connector_id != CONNECTOR_ID {
            return Err(DrmError::NotFound);
        }
        // A host-side resize only shows up in a fresh query: this driver has no
        // hotplug notification path, so the mode list is refreshed here and the
        // display driver reports the change once it is acknowledged. The mode
        // and the geometry below come from the same snapshot, so a concurrent
        // resize cannot make them disagree.
        let _ = dev.refresh_info();
        let connected = dev.is_connected();
        let (width, height) = display_resolution(dev);
        c.encoder_id = ENCODER_ID;
        c.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
        c.connector_type_id = display::VIRTUAL_CONNECTOR_TYPE_ID;
        c.connection = if connected {
            DRM_MODE_CONNECTED
        } else {
            DRM_MODE_DISCONNECTED
        };
        // A display with a descriptor reports its physical size; one without
        // reports zero, which is "unknown" and not an invented DPI.
        let (mm_width, mm_height) = edid_physical_size(dev);
        c.mm_width = mm_width;
        c.mm_height = mm_height;
        c.subpixel = 0;
        // An unplugged connector offers no mode: `drm_mode_getconnector` lists
        // whatever the connector's mode list holds, and a disconnected output
        // holds none, so reporting one would tell a compositor to enable a
        // CRTC nothing is attached to.
        c.count_encoders = report_user_array(c.encoders_ptr, c.count_encoders, &[ENCODER_ID])?;
        // The mode list comes from the display: its EDID detailed timings
        // when it has any, the visible geometry otherwise. This is the
        // `drm_edid_connector_add_modes` / `drm_cvt_mode` split of
        // `virtio_gpu_conn_get_modes`.
        let modes = connector_modes(dev, connected, width, height);
        if !c.modes_ptr.is_null() && c.count_modes > 0 {
            let copy = (c.count_modes as usize).min(modes.len());
            if copy != 0 {
                c.modes_ptr
                    .write_vm_slice(&modes[..copy])
                    .map_err(|_| DrmError::BadAddress)?;
            }
        }
        c.count_modes = modes.len() as u32;
        // The connector's properties travel with the connector: Linux's
        // `drm_mode_getconnector` fills the id/value arrays, not only
        // `OBJ_GETPROPERTIES`.
        let state = dev.kms_state();
        let (props, values) = connector_properties(&state, dev);
        report_user_array(c.props_ptr, c.count_props, &props)?;
        report_user_array(c.prop_values_ptr, c.count_props, &values)?;
        c.count_props = props.len() as u32;
        Ok(0)
    }
}

/// The mode list a connected connector reports.
///
/// The display's EDID detailed timings are the source of truth when the
/// display provides any; the visible geometry is the fallback for a display
/// without a descriptor, and an unconnected connector lists nothing.
fn connector_modes(
    dev: &DrmDevice,
    connected: bool,
    width: u32,
    height: u32,
) -> Vec<DrmModeModeInfo> {
    if !connected {
        return Vec::new();
    }
    if let Some(edid) = dev.edid() {
        let modes = crate::edid::modes(&edid);
        if !modes.is_empty() {
            return modes;
        }
    }
    mode_for_size(width, height).into_iter().collect()
}

/// Snapshots the connector's property ids and their current values.
///
/// The order is the property table's order, the same order
/// `OBJ_GETPROPERTIES` reports, so a client that reads the connector one way
/// and the values the other cannot disagree with itself.
fn connector_properties(state: &KmsState, dev: &DrmDevice) -> (Vec<u32>, Vec<u64>) {
    let mut ids = Vec::new();
    let mut values = Vec::new();
    for desc in PROPS {
        if desc.object != ObjectType::Connector {
            continue;
        }
        ids.push(desc.id.raw());
        values.push((desc.read)(state, dev));
    }
    (ids, values)
}

/// The connector's physical size in millimetres, from its EDID.
///
/// This is byte 21/22 of the base block (centimetres). A device without a
/// descriptor reports zeros, which userspace reads as "unknown".
fn edid_physical_size(dev: &DrmDevice) -> (u32, u32) {
    let Some(edid) = dev.edid() else {
        return (0, 0);
    };
    match (edid.get(21), edid.get(22)) {
        (Some(&width), Some(&height)) => (u32::from(width) * 10, u32::from(height) * 10),
        _ => (0, 0),
    }
}

/// `DRM_IOCTL_MODE_PAGE_FLIP`.
impl Ioctl for DrmModeCrtcPageFlip {
    const NAME: &'static str = "MODE_PAGE_FLIP";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, _unit: u32, f: &mut Self) -> DrmResult<usize> {
        // `drm_mode_page_flip_ioctl` refuses an unknown flag bit, a target
        // other than a page flip, and an async flip on a card without async
        // flips — all with `EINVAL`, before it looks anything up.
        if f.flags & !DRM_MODE_PAGE_FLIP_FLAGS != 0 {
            return Err(DrmError::InvalidInput);
        }
        if f.flags & DRM_MODE_PAGE_FLIP_TARGET != 0 {
            return Err(DrmError::InvalidInput);
        }
        if f.flags & DRM_MODE_PAGE_FLIP_ASYNC != 0 {
            return Err(DrmError::InvalidInput);
        }
        // The legacy structure has no sequence field; its fourth word is
        // reserved and must be zero.
        if f.abi_pad != 0 {
            return Err(DrmError::InvalidInput);
        }
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
            event.commit_with(
                CRTC_ID,
                f.user_data,
                0,
                khal::time::monotonic_time().span_since_origin(),
            );
        }
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_GETPLANERES`.
impl Ioctl for DrmModeGetPlaneRes {
    const NAME: &'static str = "MODE_GETPLANERES";

    fn handle(
        _dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        r: &mut Self,
    ) -> DrmResult<usize> {
        r.count_planes = report_user_array(r.plane_id_ptr, r.count_planes, &[PLANE_ID])?;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_GETPLANE`.
impl Ioctl for DrmModeGetPlane {
    const NAME: &'static str = "MODE_GETPLANE";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, _unit: u32, p: &mut Self) -> DrmResult<usize> {
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

    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: one atomic request validates and applies as a single \
                  unit; splitting it would duplicate rollback bookkeeping"
    )]
    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, a: &mut Self) -> DrmResult<usize> {
        // A client that never negotiated the atomic capability is told the path
        // is unavailable so it falls back to the legacy one.
        // `drm_mode_atomic_ioctl`: a client that never enabled the atomic
        // capability is told `EINVAL` (not `EOPNOTSUPP`; the capability check
        // is a request error), and an async flip is refused on a card that
        // does not flip asynchronously.
        if !file.has_cap(CLIENT_CAP_ATOMIC) {
            return Err(DrmError::InvalidInput);
        }
        if !atomic_flags_valid(a.flags, a.reserved) {
            return Err(DrmError::InvalidInput);
        }
        if a.flags & DRM_MODE_PAGE_FLIP_ASYNC != 0 {
            return Err(DrmError::InvalidInput);
        }
        // `DRM_MODE_ATOMIC_NONBLOCK` selects the completion discipline below:
        // without it the ioctl waits for the flip to reach the display, with
        // it the ioctl returns once the flip is queued. The in-fence and
        // reservation waits and the present stay synchronous either way, as
        // Linux keeps them synchronous under the flag too.
        let n = a.count_objs as usize;
        if n > MAX_ATOMIC_OBJECTS {
            return Err(DrmError::NoMemory);
        }
        // A commit may name no objects at all (niri issues one while cleaning
        // up connectors); a null pointer with a zero count is not a fault.
        let objs: Vec<u32> = if n == 0 {
            Vec::new()
        } else {
            a.objs_ptr
                .load_vm_vec(n)
                .map_err(|_| DrmError::BadAddress)?
        };
        let counts: Vec<u32> = if n == 0 {
            Vec::new()
        } else {
            a.count_props_ptr
                .load_vm_vec(n)
                .map_err(|_| DrmError::BadAddress)?
        };
        let total = counts
            .iter()
            .try_fold(0usize, |total, count| total.checked_add(*count as usize));
        let Some(total) = total.filter(|total| *total <= MAX_ATOMIC_PROPERTIES) else {
            return Err(DrmError::NoMemory);
        };
        let (props, values): (Vec<u32>, Vec<u64>) = if total == 0 {
            (Vec::new(), Vec::new())
        } else {
            (
                a.props_ptr
                    .load_vm_vec(total)
                    .map_err(|_| DrmError::BadAddress)?,
                a.prop_values_ptr
                    .load_vm_vec(total)
                    .map_err(|_| DrmError::BadAddress)?,
            )
        };

        // The object/property walk is replayed against the state the commit
        // lock protects, so no concurrent commit can be silently reverted.
        // It is also where the explicit in-fence is validated: a bad
        // descriptor reports EINVAL, exactly as Linux's `sync_file_get_fence`
        // path does.
        let apply_all = |state: &mut KmsState| -> DrmResult<()> {
            let mut index = 0;
            for (object_index, &object_id) in objs.iter().enumerate() {
                let object = ObjectType::of_id(object_id).ok_or(DrmError::NotFound)?;
                for _ in 0..counts[object_index] {
                    let prop = PropertyId::from_raw(props[index]);
                    let value = values[index];
                    index += 1;
                    match apply(state, object, prop, value, dev) {
                        Ok(true) => {}
                        Ok(false) => {
                            log::debug!(
                                "drm: atomic object {object_id:#x} prop {:#x} not attached",
                                prop.raw()
                            );
                            return Err(DrmError::NotFound);
                        }
                        Err(error) => {
                            log::debug!(
                                "drm: atomic object {object_id:#x} prop {:#x} value {value:#x} \
                                 rejected: {error:?}",
                                prop.raw()
                            );
                            return Err(error);
                        }
                    }
                }
            }
            Ok(())
        };
        // The fences the commit waits for are frozen here: re-applying after a
        // wait would read different property values than the ones the caller
        // sent, and the wait must target the commit the client described.
        let previous = dev.kms_state();
        let mut requested = previous.clone();
        apply_all(&mut requested)?;
        // `drm_atomic_check_only` refuses a commit that needs a full modeset
        // unless the client allowed one. The modeset-shaped changes of this
        // card are `CRTC.ACTIVE` and the mode the `MODE_ID` blob names.
        let needs_modeset = requested.crtc_active != previous.crtc_active
            || requested.crtc_mode.as_ref().map(|(id, _)| id.raw())
                != previous.crtc_mode.as_ref().map(|(id, _)| id.raw());
        if a.flags & DRM_MODE_ATOMIC_ALLOW_MODESET == 0 && needs_modeset {
            return Err(DrmError::InvalidInput);
        }
        let fails = |_dev: &DrmDevice, error: DrmError| -> DrmResult<usize> {
            // The request only mutated its private `requested` snapshot until
            // commit_flip publishes it. A failure must not roll the globally
            // committed state back over another client's successful commit.
            Err(error)
        };
        if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
            // Only the fences are touched by a dry run: the out-fence address
            // is checked to be writable, and a non-null one is set to -1 as
            // Linux does; the in-fence was already resolved by `apply`. A
            // failed check also restores the previous fences, as Linux's
            // `drm_atomic_state` teardown does.
            if let Err(error) = dev.validate_with(apply_all) {
                return fails(dev, error);
            }
            if let Some(slot) = request_out_fence(dev, &props, &values)? {
                // A dry run never returns a fence; Linux reports the commit as
                // successful and leaves -1 in the caller's word.
                slot.write(-1);
            }
            return Ok(0);
        }
        // Both fence sets are awaited before the commit locks anything: the
        // explicit in-fence first, then the framebuffer's implicit writers, as
        // `drm_atomic_commit` orders the plane's explicit fence and
        // `prepare_fb`.
        if let Err(error) = wait_explicit_fence(dev, &requested) {
            return fails(dev, error);
        }
        if let Err(error) = wait_implicit_fences(dev, &requested) {
            return fails(dev, error);
        }
        // The event is reserved before the commit for the same reason as a
        // legacy flip: a full queue must fail the commit, not drop a
        // completion after the scanout already changed.
        let event = if a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            Some(dev.reserve_flip_event().ok_or(DrmError::NoMemory)?)
        } else {
            None
        };
        // The commit's fence exists before the commit, so the commit, its
        // page-flip event and its out-fence share one completion point. The
        // same fence is recorded on the framebuffer's reservation as this
        // card's display work, which is what makes the next writer of that
        // buffer wait for the display.
        let out = OutFence::new(event, a.user_data, dev.driver_info().name);
        let slot = request_out_fence(dev, &props, &values)?;
        let flip = out.into_flip();
        let fence = flip
            .fence_signal
            .as_ref()
            .map(|signal| signal.fence().clone());
        let out_fd = match (&slot, &fence) {
            (Some(slot), Some(fence)) => {
                // Check the user word and allocate the descriptor before the
                // irreversible scanout change; both failures leave the screen
                // untouched. A failed commit below closes the preinstalled fd.
                slot.write_or_fault(-1)?;
                let fd = crate::sync::fence_access::install(fence, &kprocess::current_cred())?;
                Some((slot, fd))
            }
            _ => None,
        };
        if let Some(fence) = &fence {
            dev.reserve_display(requested.plane_fb_id, fence.clone());
        }
        // The commit consumes the flip; keep its completion point so a blocking
        // request can wait for the flip to reach the display.
        let completion = flip
            .fence_signal
            .as_ref()
            .map(|signal| signal.fence().clone());
        if let Err(error) = dev.commit_flip(apply_all, flip) {
            if let Some((_, fd)) = out_fd {
                let _ = kprocess::current_files().close_file(fd);
            }
            return fails(dev, error);
        }
        if let Some((slot, fd)) = out_fd {
            slot.write(fd);
        }
        // `drm_atomic_helper_commit_tail` ends in a vblank wait, so a request
        // without `NONBLOCK` returns only after the flip completed; `NONBLOCK`
        // is what lets the ioctl return after queueing it. A signal or a fence
        // error cannot undo a commit that already programmed the scanout, so
        // neither turns the ioctl into a failure.
        if a.flags & DRM_MODE_ATOMIC_NONBLOCK == 0
            && let Some(fence) = completion
            && let Err(error) =
                crate::sync::wait::wait_for_fences(&FenceList::new(alloc::vec![fence]))
        {
            log::debug!("drm: committed flip did not complete cleanly: {error:?}");
        }
        Ok(0)
    }
}

/// The `OUT_FENCE_PTR` address one atomic request names, if any.
///
/// Linux records the pointer as part of the CRTC state while the properties are
/// applied and validates it once, when the fence descriptor is written. This
/// driver resolves it from the request directly, which keeps the published
/// state free of a userspace address; the value never reaches a commit.
fn request_out_fence(
    _dev: &DrmDevice,
    props: &[u32],
    values: &[u64],
) -> DrmResult<Option<OutFenceSlot>> {
    let mut slot = None;
    for (prop, value) in props.iter().zip(values) {
        if *prop != PROP_CRTC_OUT_FENCE_PTR {
            continue;
        }
        // Linux's atomic property walk lets a repeated property overwrite the
        // earlier value, so the last one wins.
        slot = (*value != 0).then(|| OutFenceSlot::new(*value));
    }
    Ok(slot)
}

/// The userspace word a commit's out-fence descriptor is written to.
///
/// The word is written as `s32`, exactly like `put_user(fd, out_fence_ptr)`:
/// a caller that passes a misaligned or unwritable address gets `EFAULT` from
/// the write, never a partial kernel structure.
struct OutFenceSlot {
    /// The userspace address of the `s32`.
    address: usize,
}

impl OutFenceSlot {
    /// Records one `OUT_FENCE_PTR` value.
    const fn new(address: u64) -> Self {
        Self {
            address: address as usize,
        }
    }

    /// Writes the descriptor number, or `-1` for "no fence".
    ///
    /// A dry run reports "no fence" through this word; a failed write there is
    /// not fatal, as Linux only logs it.
    fn write(&self, fd: i32) {
        let user = UserPtr::<i32>::from(self.address);
        if user.write_vm(fd).is_err() {
            log::debug!("drm: cannot write out-fence {fd} to {:#x}", self.address);
        }
    }

    /// Writes a committed flip's descriptor, failing with `EFAULT` when the
    /// caller's word is not writable.
    ///
    /// `drm_atomic_uapi`'s `setup_out_fence` performs this `put_user` **before**
    /// installing the descriptor, so an unwritable address fails the request
    /// without leaking a fence; this checks the same way by writing first and
    /// installing only after the write succeeded.
    fn write_or_fault(&self, fd: i32) -> DrmResult<()> {
        UserPtr::<i32>::from(self.address)
            .write_vm(fd)
            .map_err(|_| DrmError::BadAddress)
    }
}

/// Waits for the fences one commit must observe before it reaches the screen.
///
/// This is the implicit-fence rule of `drm_gem_plane_helper_prepare_fb`: the
/// framebuffer's reservation must have no outstanding writer, because scanning
/// it out is a read of the same memory. The explicit `IN_FENCE_FD` is not
/// part of this set: Linux gives explicit fencing precedence over implicit
/// fencing, so a client that declared its own ordering is not additionally
/// clamped by the buffer's reservation.
///
/// The call happens before the commit takes the commit lock and before the
/// kernel waits on it, so no lock is held across the sleep.
pub(crate) fn wait_implicit_fences(dev: &DrmDevice, state: &KmsState) -> DrmResult<()> {
    if state.plane_fb_id.is_none() {
        return Ok(());
    }
    let Some(object) = dev
        .framebuffer_object(state.plane_fb_id)
        .map(|(object, ..)| object)
    else {
        return Ok(());
    };
    let fences = object
        .resv()
        .snapshot(crate::sync::dma_resv::ResvUsage::Read);
    crate::sync::wait::wait_for_fences(&fences)
}

/// Waits for the explicit `IN_FENCE_FD` fence of a commit, if there is one.
///
/// Linux attaches the explicit fence to the plane state while the properties
/// are applied and the atomic helper waits for it before the plane is
/// prepared; this driver waits here, in the same order relative to the
/// commit.
pub(crate) fn wait_explicit_fence(dev: &DrmDevice, state: &KmsState) -> DrmResult<()> {
    let _ = dev;
    let Some(fence) = &state.plane_in_fence else {
        return Ok(());
    };
    let fences = FenceList::new(alloc::vec![fence.clone()]);
    crate::sync::wait::wait_for_fences(&fences)
}

/// `DRM_IOCTL_MODE_SETPLANE`.
impl Ioctl for DrmModeSetPlane {
    const NAME: &'static str = "MODE_SETPLANE";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, _unit: u32, p: &mut Self) -> DrmResult<usize> {
        if p.plane_id != PLANE_ID {
            return Err(DrmError::NotFound);
        }
        let crtc = CrtcId::from_raw(p.crtc_id);
        let fb = FbId::from_raw(p.fb_id);
        // A disable names neither a CRTC nor a framebuffer; naming only one is
        // malformed, as `drm_mode_setplane` judges.
        if crtc.is_none() || fb.is_none() {
            if !crtc.is_none() || !fb.is_none() {
                return Err(DrmError::InvalidInput);
            }
            dev.commit_with(|state| {
                state.plane_fb_id = FbId::default();
                state.plane_crtc_id = CrtcId::default();
                Ok(())
            })?;
            return Ok(0);
        }
        if crtc.raw() != CRTC_ID {
            return Err(DrmError::NotFound);
        }
        if !dev.has_framebuffer(fb) {
            return Err(DrmError::NotFound);
        }
        let src_x = u64::from(p.src_x);
        let src_y = u64::from(p.src_y);
        dev.commit_with(|state| {
            state.plane_fb_id = fb;
            state.plane_crtc_id = CRTC;
            state.plane_src_x = src_x;
            state.plane_src_y = src_y;
            state.plane_src_w = u64::from(p.src_w);
            state.plane_src_h = u64::from(p.src_h);
            state.plane_crtc_x = i64::from(p.crtc_x);
            state.plane_crtc_y = i64::from(p.crtc_y);
            state.plane_crtc_w = u64::from(p.crtc_w);
            state.plane_crtc_h = u64::from(p.crtc_h);
            Ok(())
        })?;
        Ok(0)
    }
}

/// The gamma table both gamma commands name.
///
/// The card has no gamma LUT (`GETPLANE` reports `gamma_size == 0`), so both
/// accept only a zero-sized table: Linux answers with the LUT a client asked
/// for and this one has none to give.
fn check_gamma(dev: &DrmDevice, lut: &DrmModeCrtcLut) -> DrmResult<()> {
    if lut.crtc_id != CRTC_ID {
        return Err(DrmError::NotFound);
    }
    if lut.gamma_size != 0 {
        return Err(DrmError::InvalidInput);
    }
    let _ = dev;
    Ok(())
}

/// `DRM_IOCTL_MODE_GETGAMMA`.
impl Ioctl for DrmModeCrtcLut {
    const NAME: &'static str = "MODE_GETGAMMA";

    fn handle(
        dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        lut: &mut Self,
    ) -> DrmResult<usize> {
        check_gamma(dev, lut)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_SETGAMMA`.
#[repr(transparent)]
#[derive(
    Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite,
)]
pub(crate) struct DrmModeSetGamma(pub(crate) DrmModeCrtcLut);

impl Ioctl for DrmModeSetGamma {
    const NAME: &'static str = "MODE_SETGAMMA";

    fn handle(
        dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        lut: &mut Self,
    ) -> DrmResult<usize> {
        check_gamma(dev, &lut.0)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_CRTC_GET_SEQUENCE`.
///
/// The sequence lives on the vblank timeline, and this card has none: the
/// answer is the same `EOPNOTSUPP` a `WAIT_VBLANK` receives, which is what a
/// client that probed `DRM_CAP_CRTC_IN_VBLANK_EVENT` expects.
impl Ioctl for DrmCrtcGetSequence {
    const NAME: &'static str = "CRTC_GET_SEQUENCE";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, _unit: u32, r: &mut Self) -> DrmResult<usize> {
        if r.crtc_id != CRTC_ID {
            return Err(DrmError::NotFound);
        }
        if !dev.vblank_supported() {
            return Err(DrmError::OpNotSupported);
        }
        r.active = u32::from(dev.kms_state().crtc_active != 0);
        let (sequence, timestamp) = dev.vblank_clock().tick_report();
        r.sequence = u64::from(sequence);
        r.sequence_ns = timestamp.as_nanos_u64_saturating() as i64;
        Ok(0)
    }
}

/// `DRM_IOCTL_CRTC_QUEUE_SEQUENCE`: refused for the same reason, since a
/// sequence event needs the timeline it fires on.
impl Ioctl for DrmCrtcQueueSequence {
    const NAME: &'static str = "CRTC_QUEUE_SEQUENCE";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, _unit: u32, r: &mut Self) -> DrmResult<usize> {
        if r.crtc_id != CRTC_ID {
            return Err(DrmError::NotFound);
        }
        if r.flags & !(DRM_CRTC_SEQUENCE_RELATIVE | DRM_CRTC_SEQUENCE_NEXT_ON_MISS) != 0 {
            return Err(DrmError::InvalidInput);
        }
        if !dev.vblank_supported() {
            return Err(DrmError::OpNotSupported);
        }
        Err(DrmError::OpNotSupported)
    }
}
