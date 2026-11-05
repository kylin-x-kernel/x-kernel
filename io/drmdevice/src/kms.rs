// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KMS objects, properties and commits (`drm_crtc.c`).
//!
//! There is one KMS state, one property table and one commit path:
//!
//! * [`KmsState`] is plain data. Legacy `SETCRTC` builds one and commits it
//!   through the same entry point `MODE_ATOMIC` uses, so a legacy modeset is
//!   visible to atomic clients and vice versa.
//! * [`PROPS`] is the only description of a property: id, wire flags, the value
//!   reader and the optional writer. `OBJ_GETPROPERTIES`, `GETPROPERTY` and the
//!   write path all consume the same table, so a property cannot be readable but
//!   unwritable, or report metadata that disagrees with its writer.
//! * [`DrmDevice::commit`] validates, publishes the state and presents the
//!   resulting scanout exactly once.

use alloc::sync::Arc;

use crate::{model::file::CLIENT_CAP_ATOMIC, prelude::*};

/// `DRM_IOCTL_MODE_SETCRTC`: the wire request is a `drm_mode_crtc`.
#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmModeSetCrtc(pub(crate) DrmModeCrtc);

/// The card's single CRTC.
pub(crate) const CRTC: CrtcId = CrtcId::from_raw(CRTC_ID);

/// Object class addressed by property ioctls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjectType {
    /// CRTC.
    Crtc,
    /// Connector.
    Connector,
    /// Plane.
    Plane,
}

impl ObjectType {
    /// Decodes the `DRM_MODE_OBJECT_*` word from a request.
    pub(crate) const fn from_wire(value: u32) -> Option<Self> {
        match value {
            DRM_MODE_OBJECT_CRTC => Some(Self::Crtc),
            DRM_MODE_OBJECT_CONNECTOR => Some(Self::Connector),
            DRM_MODE_OBJECT_PLANE => Some(Self::Plane),
            _ => None,
        }
    }

    /// Derives the class from an object id, as `MODE_ATOMIC` must.
    pub(crate) const fn of_id(id: u32) -> Option<Self> {
        match id {
            CRTC_ID => Some(Self::Crtc),
            CONNECTOR_ID => Some(Self::Connector),
            PLANE_ID => Some(Self::Plane),
            _ => None,
        }
    }
}

/// The committed KMS state of the card.
///
/// Wire-form values are kept where userspace reads them back through a
/// property; typed ids are kept where the driver indexes a registry.
#[derive(Clone, Default)]
pub(crate) struct KmsState {
    /// CRTC `ACTIVE`.
    pub crtc_active: u64,
    /// CRTC `MODE_ID` blob: the id is the wire value, the blob keeps the bytes
    /// alive for as long as the state references it.
    pub crtc_mode: Option<(BlobId, Arc<Blob>)>,
    /// Connector `CRTC_ID`.
    pub conn_crtc_id: CrtcId,
    /// Plane `FB_ID`.
    pub plane_fb_id: FbId,
    /// Plane `CRTC_ID`.
    pub plane_crtc_id: CrtcId,
    /// Plane `SRC_X`.
    pub plane_src_x: u64,
    /// Plane `SRC_Y`.
    pub plane_src_y: u64,
    /// Plane `SRC_W`.
    pub plane_src_w: u64,
    /// Plane `SRC_H`.
    pub plane_src_h: u64,
    /// Plane `CRTC_X`.
    pub plane_crtc_x: i64,
    /// Plane `CRTC_Y`.
    pub plane_crtc_y: i64,
    /// Plane `CRTC_W`.
    pub plane_crtc_w: u64,
    /// Plane `CRTC_H`.
    pub plane_crtc_h: u64,
}

/// How a property is described on the wire.
pub(crate) enum PropKind {
    /// Enumerated values with names.
    Enum(&'static [DrmModePropertyEnum]),
    /// Inclusive numeric range.
    Range {
        /// Minimum.
        min: u64,
        /// Maximum.
        max: u64,
    },
    /// Id of another object.
    Object,
    /// Id of a blob.
    Blob,
}

/// Reads one property out of a state snapshot.
pub(crate) type PropRead = fn(&KmsState, &DrmDevice) -> u64;
/// Applies one property value to a state copy.
pub(crate) type PropWrite = fn(&mut KmsState, u64, &DrmDevice) -> DrmResult<()>;

/// One property: the single description of its id, metadata and accessors.
pub(crate) struct PropDesc {
    /// Wire id.
    pub id: PropertyId,
    /// Wire name.
    pub name: &'static str,
    /// Wire flags.
    pub flags: u32,
    /// Object class the property belongs to.
    pub object: ObjectType,
    /// Wire value description.
    pub kind: PropKind,
    /// Reads the property out of a state snapshot.
    pub read: PropRead,
    /// Applies a value, or `None` when the property is immutable.
    pub write: Option<PropWrite>,
}

const fn enum_entry(value: u64, name: &[u8]) -> DrmModePropertyEnum {
    let mut entry = DrmModePropertyEnum {
        value,
        name: [0; DRM_PROP_NAME_LEN],
    };
    let n = if name.len() < DRM_PROP_NAME_LEN - 1 {
        name.len()
    } else {
        DRM_PROP_NAME_LEN - 1
    };
    let mut i = 0;
    while i < n {
        entry.name[i] = name[i];
        i += 1;
    }
    entry
}

const PLANE_TYPE_ENUMS: &[DrmModePropertyEnum] = &[
    enum_entry(0, b"Overlay"),
    enum_entry(1, b"Primary"),
    enum_entry(2, b"Cursor"),
];

const fn range(name: &'static str, min: u64, max: u64) -> PropKind {
    let _ = name;
    PropKind::Range { min, max }
}

const fn read_plane_type(state: &KmsState, _dev: &DrmDevice) -> u64 {
    let _ = state;
    DRM_PLANE_TYPE_PRIMARY
}
const fn read_plane_fb(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_fb_id.raw() as u64
}
const fn read_plane_crtc(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_crtc_id.raw() as u64
}
const fn read_src_x(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_src_x
}
const fn read_src_y(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_src_y
}
const fn read_src_w(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_src_w
}
const fn read_src_h(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_src_h
}
const fn read_crtc_x(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_crtc_x as u64
}
const fn read_crtc_y(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_crtc_y as u64
}
const fn read_crtc_w(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_crtc_w
}
const fn read_crtc_h(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.plane_crtc_h
}
fn read_in_formats(_state: &KmsState, dev: &DrmDevice) -> u64 {
    dev.ensure_in_formats_blob().map_or(0, |id| id.raw() as u64)
}
const fn read_crtc_active(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.crtc_active
}
fn read_crtc_mode(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state
        .crtc_mode
        .as_ref()
        .map_or(0, |(id, _)| id.raw() as u64)
}
const fn read_conn_crtc(state: &KmsState, _dev: &DrmDevice) -> u64 {
    state.conn_crtc_id.raw() as u64
}

fn write_plane_fb(state: &mut KmsState, value: u64, dev: &DrmDevice) -> DrmResult<()> {
    let fb = FbId::from_raw(value as u32);
    if !fb.is_none() && !dev.has_framebuffer(fb) {
        return Err(DrmError::InvalidInput);
    }
    state.plane_fb_id = fb;
    Ok(())
}

const fn write_plane_crtc(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    let crtc = CrtcId::from_raw(value as u32);
    if !crtc.is_none() && crtc.raw() != CRTC_ID {
        return Err(DrmError::InvalidInput);
    }
    state.plane_crtc_id = crtc;
    Ok(())
}

fn checked_i32(value: u64) -> DrmResult<i32> {
    let v = value as i64;
    if (i32::MIN as i64..=i32::MAX as i64).contains(&v) {
        Ok(v as i32)
    } else {
        Err(DrmError::InvalidInput)
    }
}

fn write_crtc_x(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    state.plane_crtc_x = checked_i32(value)? as i64;
    Ok(())
}
fn write_crtc_y(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    state.plane_crtc_y = checked_i32(value)? as i64;
    Ok(())
}
const fn write_u64(target: &mut u64, value: u64) -> DrmResult<()> {
    *target = value;
    Ok(())
}
fn write_src_x(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    write_u64(&mut state.plane_src_x, value)
}
fn write_src_y(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    write_u64(&mut state.plane_src_y, value)
}
fn write_src_w(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    write_u64(&mut state.plane_src_w, value)
}
fn write_src_h(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    write_u64(&mut state.plane_src_h, value)
}
fn write_crtc_w(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    write_u64(&mut state.plane_crtc_w, value)
}
fn write_crtc_h(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    write_u64(&mut state.plane_crtc_h, value)
}
fn write_crtc_active(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    if value > 1 {
        return Err(DrmError::InvalidInput);
    }
    state.crtc_active = value;
    Ok(())
}

fn write_crtc_mode(state: &mut KmsState, value: u64, dev: &DrmDevice) -> DrmResult<()> {
    let id = BlobId::from_raw(value as u32);
    if id.is_none() {
        state.crtc_mode = None;
        return Ok(());
    }
    let blob = dev.blob(id).ok_or(DrmError::InvalidInput)?;
    let mode = mode_from_bytes(blob.bytes().as_slice()).ok_or(DrmError::InvalidInput)?;
    state.crtc_mode = Some((id, blob));
    let _ = mode;
    Ok(())
}

fn write_conn_crtc(state: &mut KmsState, value: u64, _dev: &DrmDevice) -> DrmResult<()> {
    let crtc = CrtcId::from_raw(value as u32);
    if !crtc.is_none() && crtc.raw() != CRTC_ID {
        return Err(DrmError::InvalidInput);
    }
    state.conn_crtc_id = crtc;
    Ok(())
}

/// Every property this driver exposes.
///
/// The order is the order `OBJ_GETPROPERTIES` reports, and the table is the
/// only place a property id, its metadata and its accessors are written down.
pub(crate) static PROPS: &[PropDesc] = &[
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_TYPE),
        name: "type",
        flags: DRM_MODE_PROP_ENUM | DRM_MODE_PROP_IMMUTABLE,
        object: ObjectType::Plane,
        kind: PropKind::Enum(PLANE_TYPE_ENUMS),
        read: read_plane_type,
        write: None,
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_FB_ID),
        name: "FB_ID",
        flags: DRM_MODE_PROP_OBJECT | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: PropKind::Object,
        read: read_plane_fb,
        write: Some(write_plane_fb),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_CRTC_ID),
        name: "CRTC_ID",
        flags: DRM_MODE_PROP_OBJECT | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: PropKind::Object,
        read: read_plane_crtc,
        write: Some(write_plane_crtc),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_SRC_X),
        name: "SRC_X",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: range("SRC_X", 0, u32::MAX as u64),
        read: read_src_x,
        write: Some(write_src_x),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_SRC_Y),
        name: "SRC_Y",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: range("SRC_Y", 0, u32::MAX as u64),
        read: read_src_y,
        write: Some(write_src_y),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_SRC_W),
        name: "SRC_W",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: range("SRC_W", 0, u32::MAX as u64),
        read: read_src_w,
        write: Some(write_src_w),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_SRC_H),
        name: "SRC_H",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: range("SRC_H", 0, u32::MAX as u64),
        read: read_src_h,
        write: Some(write_src_h),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_CRTC_X),
        name: "CRTC_X",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_SIGNED_RANGE | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: range("CRTC_X", i32::MIN as i64 as u64, i32::MAX as i64 as u64),
        read: read_crtc_x,
        write: Some(write_crtc_x),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_CRTC_Y),
        name: "CRTC_Y",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_SIGNED_RANGE | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: range("CRTC_Y", i32::MIN as i64 as u64, i32::MAX as i64 as u64),
        read: read_crtc_y,
        write: Some(write_crtc_y),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_CRTC_W),
        name: "CRTC_W",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: range("CRTC_W", 0, u32::MAX as u64),
        read: read_crtc_w,
        write: Some(write_crtc_w),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_CRTC_H),
        name: "CRTC_H",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Plane,
        kind: range("CRTC_H", 0, u32::MAX as u64),
        read: read_crtc_h,
        write: Some(write_crtc_h),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_PLANE_IN_FORMATS),
        name: "IN_FORMATS",
        flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_IMMUTABLE,
        object: ObjectType::Plane,
        kind: PropKind::Blob,
        read: read_in_formats,
        write: None,
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_CRTC_ACTIVE),
        name: "ACTIVE",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Crtc,
        kind: range("ACTIVE", 0, 1),
        read: read_crtc_active,
        write: Some(write_crtc_active),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_CRTC_MODE_ID),
        name: "MODE_ID",
        flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Crtc,
        kind: PropKind::Blob,
        read: read_crtc_mode,
        write: Some(write_crtc_mode),
    },
    PropDesc {
        id: PropertyId::from_raw(PROP_CONN_CRTC_ID),
        name: "CRTC_ID",
        flags: DRM_MODE_PROP_OBJECT | DRM_MODE_PROP_ATOMIC,
        object: ObjectType::Connector,
        kind: PropKind::Object,
        read: read_conn_crtc,
        write: Some(write_conn_crtc),
    },
];

/// Looks a property up by wire id.
pub(crate) fn property(id: PropertyId) -> Option<&'static PropDesc> {
    PROPS.iter().find(|prop| prop.id == id)
}

/// Properties of one object class, in table order.
pub(crate) fn properties_of(object: ObjectType) -> impl Iterator<Item = &'static PropDesc> {
    PROPS.iter().filter(move |prop| prop.object == object)
}

/// Applies one property value to `state`.
///
/// Returns `Ok(false)` when the property is not attached to the object.  Linux
/// reports both an unknown object and an unattached property as `ENOENT`.
/// A property that *is* attached but immutable is different: Linux rejects the
/// write itself with `EINVAL`, so do not let the caller turn it into `ENOENT`.
pub(crate) fn apply(
    state: &mut KmsState,
    object: ObjectType,
    prop: PropertyId,
    value: u64,
    dev: &DrmDevice,
) -> DrmResult<bool> {
    let Some(desc) = property(prop) else {
        return Ok(false);
    };
    if desc.object != object {
        return Ok(false);
    }
    let Some(write) = desc.write else {
        return Err(DrmError::InvalidInput);
    };
    write(state, value, dev)?;
    Ok(true)
}

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

pub(crate) fn mode_blob_bytes(mode: &DrmModeModeInfo) -> Vec<u8> {
    // `DrmModeModeInfo` is padding-free, so encoding it field by field is both
    // exact and free of any reliance on `unsafe` casts.
    let mut bytes = Vec::with_capacity(core::mem::size_of::<DrmModeModeInfo>());
    bytes.extend_from_slice(&mode.clock.to_ne_bytes());
    for field in [
        mode.hdisplay,
        mode.hsync_start,
        mode.hsync_end,
        mode.htotal,
        mode.hskew,
        mode.vdisplay,
        mode.vsync_start,
        mode.vsync_end,
        mode.vtotal,
        mode.vscan,
    ] {
        bytes.extend_from_slice(&field.to_ne_bytes());
    }
    bytes.extend_from_slice(&mode.vrefresh.to_ne_bytes());
    bytes.extend_from_slice(&mode.flags.to_ne_bytes());
    bytes.extend_from_slice(&mode.kind.to_ne_bytes());
    bytes.extend_from_slice(&mode.name);
    bytes
}

/// Decodes the bytes of a `MODE_ID` blob.
pub(crate) fn mode_from_bytes(bytes: &[u8]) -> Option<DrmModeModeInfo> {
    fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    }
    fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_ne_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
    }
    if bytes.len() != core::mem::size_of::<DrmModeModeInfo>() {
        return None;
    }
    let mut name = [0u8; DRM_MODE_NAME_LEN];
    name.copy_from_slice(bytes.get(36..36 + DRM_MODE_NAME_LEN)?);
    Some(DrmModeModeInfo {
        clock: u32_at(bytes, 0)?,
        hdisplay: u16_at(bytes, 4)?,
        hsync_start: u16_at(bytes, 6)?,
        hsync_end: u16_at(bytes, 8)?,
        htotal: u16_at(bytes, 10)?,
        hskew: u16_at(bytes, 12)?,
        vdisplay: u16_at(bytes, 14)?,
        vsync_start: u16_at(bytes, 16)?,
        vsync_end: u16_at(bytes, 18)?,
        vtotal: u16_at(bytes, 20)?,
        vscan: u16_at(bytes, 22)?,
        vrefresh: u32_at(bytes, 24)?,
        flags: u32_at(bytes, 28)?,
        kind: u32_at(bytes, 32)?,
        name,
    })
}

impl Ioctl for DrmModeCardRes {
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

impl Ioctl for DrmModeCrtc {
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

impl Ioctl for DrmModeSetCrtc {
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

impl Ioctl for DrmModeGetEncoder {
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

impl Ioctl for DrmModeGetConnector {
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

impl Ioctl for DrmModeCrtcPageFlip {
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

impl Ioctl for DrmModeGetPlaneRes {
    fn handle(_dev: &DrmDevice, _file: &Arc<DrmFile>, r: &mut Self) -> DrmResult<usize> {
        r.count_planes = report_user_array(r.plane_id_ptr, r.count_planes, &[PLANE_ID])?;
        Ok(0)
    }
}

impl Ioctl for DrmModeGetPlane {
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

impl Ioctl for DrmModeConnectorSetProperty {
    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, req: &mut Self) -> DrmResult<usize> {
        if req.connector_id != CONNECTOR_ID {
            return Err(DrmError::NotFound);
        }
        set_one_property(dev, ObjectType::Connector, req.prop_id, req.value)
    }
}

/// Applies one property write and commits the result.
fn set_one_property(
    dev: &DrmDevice,
    object: ObjectType,
    prop_id: u32,
    value: u64,
) -> DrmResult<usize> {
    // Read, mutate and publish under one commit lock; reading the state first
    // would let a concurrent commit be reverted by this one.
    dev.commit_with(|state| {
        if !apply(state, object, PropertyId::from_raw(prop_id), value, dev)? {
            // Known object, unknown property: Linux leaves its `-EINVAL`
            // initialiser in place.
            return Err(DrmError::InvalidInput);
        }
        Ok(())
    })?;
    Ok(0)
}

/// Resolve an object ID for `OBJ_GETPROPERTIES` / `OBJ_SETPROPERTY`.
///
/// Linux treats `DRM_MODE_OBJECT_ANY` (zero) as a wildcard: the ID alone picks
/// the object. libdrm's `drmModeObjectGetProperties()` exposes that form, and
/// generic users such as wlroots use it to read a property without first
/// knowing the object type. A non-zero type must still agree with the ID.
fn object_for(obj_id: u32, obj_type: u32) -> DrmResult<ObjectType> {
    let object = ObjectType::of_id(obj_id).ok_or(DrmError::NotFound)?;
    if obj_type != 0 && ObjectType::from_wire(obj_type) != Some(object) {
        return Err(DrmError::NotFound);
    }
    Ok(object)
}

impl Ioctl for DrmModeObjSetProperty {
    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, req: &mut Self) -> DrmResult<usize> {
        let object = object_for(req.obj_id, req.obj_type)?;
        set_one_property(dev, object, req.prop_id, req.value)
    }
}

impl Ioctl for DrmModeObjGetProperties {
    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, q: &mut Self) -> DrmResult<usize> {
        let object = object_for(q.obj_id, q.obj_type)?;
        let state = dev.kms_state();
        let ids: Vec<u32> = properties_of(object).map(|prop| prop.id.raw()).collect();
        let values: Vec<u64> = properties_of(object)
            .map(|prop| (prop.read)(&state, dev))
            .collect();
        report_user_array(q.props_ptr, q.count_props, &ids)?;
        report_user_array(q.prop_values_ptr, q.count_props, &values)?;
        q.count_props = ids.len() as u32;
        Ok(0)
    }
}

impl Ioctl for DrmModeGetProperty {
    fn handle(_dev: &DrmDevice, _file: &Arc<DrmFile>, g: &mut Self) -> DrmResult<usize> {
        let desc = property(PropertyId::from_raw(g.prop_id)).ok_or(DrmError::NotFound)?;
        g.flags = desc.flags;
        g.name = [0; DRM_PROP_NAME_LEN];
        let name = desc.name.as_bytes();
        let n = name.len().min(DRM_PROP_NAME_LEN - 1);
        g.name[..n].copy_from_slice(&name[..n]);
        match desc.kind {
            PropKind::Enum(enums) => {
                g.count_values = enums.len() as u32;
                g.count_enum_blobs = report_user_array(g.enum_blob_ptr, g.count_enum_blobs, enums)?;
            }
            PropKind::Range { min, max } => {
                g.count_values = report_user_array(g.values_ptr, g.count_values, &[min, max])?;
                g.count_enum_blobs = 0;
            }
            PropKind::Object | PropKind::Blob => {
                g.count_values = 0;
                g.count_enum_blobs = 0;
            }
        }
        Ok(0)
    }
}

impl Ioctl for DrmWaitVblank {
    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, request: &mut Self) -> DrmResult<usize> {
        let is_relative = request.rep_type & DRM_VBLANK_RELATIVE != 0;
        let current = dev.vblank_sequence();
        let target = if is_relative {
            current.wrapping_add(request.sequence)
        } else {
            request.sequence
        };
        let raw_wait = target.wrapping_sub(current);
        let wait_count = if raw_wait == 0 || raw_wait >= i32::MAX as u32 {
            1
        } else {
            raw_wait
        };
        const FRAME_PERIOD_NS: u64 = ktime_types::NANOS_PER_SEC / 60;
        let delay =
            ktime_types::TimeSpan::from_nanos(FRAME_PERIOD_NS.saturating_mul(wait_count as u64));
        ktask::sleep(delay);
        dev.advance_vblank(wait_count);
        let now = khal::time::monotonic_time().span_since_origin();
        *request = DrmWaitVblank {
            rep_type: 0,
            sequence: dev.vblank_sequence(),
            tv_sec: now.as_secs() as i64,
            tv_usec: now.subsec_micros() as i64,
        };
        Ok(0)
    }
}

/// Upper bound on objects in one atomic request.
///
/// Userspace names at most one object of each kind in this driver, so a larger
/// count can only be malformed. Rejecting it before `load_vm_vec` keeps a
/// malformed count from driving a huge kernel allocation.
const MAX_ATOMIC_OBJECTS: usize = 1024;

/// Upper bound on the total properties in one atomic request.
const MAX_ATOMIC_PROPERTIES: usize = 4096;

/// Whether an atomic request uses only flags Linux accepts.
///
/// `TEST_ONLY` is a validation-only request, so asking to queue a page-flip
/// event in the same call is contradictory; Linux rejects that combination
/// with `EINVAL` rather than silently dropping the event.
pub(crate) fn atomic_flags_valid(flags: u32, reserved: u64) -> bool {
    let known = DRM_MODE_ATOMIC_TEST_ONLY
        | DRM_MODE_ATOMIC_NONBLOCK
        | DRM_MODE_ATOMIC_ALLOW_MODESET
        | DRM_MODE_PAGE_FLIP_EVENT;
    flags & !known == 0
        && reserved == 0
        && !(flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 && flags & DRM_MODE_PAGE_FLIP_EVENT != 0)
}

impl Ioctl for DrmModeAtomic {
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

impl Ioctl for DrmModeCursor {
    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, cursor: &mut Self) -> DrmResult<usize> {
        set_cursor(
            dev,
            file,
            CursorRequest {
                crtc_id: cursor.crtc_id,
                flags: cursor.flags,
                width: cursor.width,
                height: cursor.height,
                handle: cursor.handle,
            },
        )
    }
}

impl Ioctl for DrmModeCursor2 {
    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, cursor: &mut Self) -> DrmResult<usize> {
        // The hotspot only matters to a hardware cursor plane, which this driver
        // does not expose; validating it would reject requests the device
        // treats as a plain move.
        set_cursor(
            dev,
            file,
            CursorRequest {
                crtc_id: cursor.crtc_id,
                flags: cursor.flags,
                width: cursor.width,
                height: cursor.height,
                handle: cursor.handle,
            },
        )
    }
}

/// One legacy cursor request, common to `MODE_CURSOR` and `MODE_CURSOR2`.
struct CursorRequest {
    crtc_id: u32,
    flags: u32,
    width: u32,
    height: u32,
    handle: u32,
}

/// Answers a legacy cursor request.
///
/// The bound display exposes a single primary plane, so there is no hardware
/// cursor to program. A bare move is answered with success -- libdrm-based
/// clients such as Xorg's modesetting driver issue it once the CRTC is up and
/// treat a failure as an initialisation error -- but a request that installs an
/// image is refused with `EOPNOTSUPP`. Accepting the image would tell the client
/// its hardware cursor works, and it would then stop drawing the pointer itself
/// while nothing appeared on screen.
fn set_cursor(dev: &DrmDevice, file: &Arc<DrmFile>, request: CursorRequest) -> DrmResult<usize> {
    if request.crtc_id != CRTC_ID {
        return Err(DrmError::InvalidInput);
    }
    if request.flags & !(DRM_MODE_CURSOR_BO | DRM_MODE_CURSOR_MOVE) != 0 {
        return Err(DrmError::InvalidInput);
    }
    if request.flags & DRM_MODE_CURSOR_BO != 0 {
        // `handle == 0` hides the cursor and carries no image, so it is a
        // successful no-op even without a cursor plane.
        if request.handle == 0 {
            return Ok(0);
        }
        // A real image is validated first so a malformed request is reported as
        // such, then refused: with no cursor plane the driver cannot show it,
        // and reporting success would make the client stop drawing its own
        // pointer.
        let object = file
            .handle(DumbHandle::from_raw(request.handle))
            .ok_or(DrmError::InvalidInput)?;
        if request.width == 0 || request.height == 0 {
            return Err(DrmError::InvalidInput);
        }
        if object.width() == 0 || object.height() == 0 {
            return Err(DrmError::InvalidInput);
        }
        return Err(DrmError::OpNotSupported);
    }
    let _ = dev;
    Ok(0)
}
