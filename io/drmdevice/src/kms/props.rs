// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The property table and the property ioctls (`drm_property.c`).
//!
//! [`PROPS`] is the only description of a property: id, wire flags, the value
//! reader and the optional writer. `OBJ_GETPROPERTIES`, `GETPROPERTY` and the
//! write path all consume the same table, so a property cannot be readable but
//! unwritable, or report metadata that disagrees with its writer. Writes reach
//! the committed state through [`apply`](super::state::apply) under the commit
//! lock.

use crate::prelude::*;

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

/// `DRM_IOCTL_MODE_CONNECTOR_SETPROPERTY`.
impl Ioctl for DrmModeConnectorSetProperty {
    const NAME: &'static str = "MODE_CONNECTOR_SETPROPERTY";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, req: &mut Self) -> DrmResult<usize> {
        if req.connector_id != CONNECTOR_ID {
            return Err(DrmError::NotFound);
        }
        set_one_property(dev, ObjectType::Connector, req.prop_id, req.value)
    }
}

/// `DRM_IOCTL_MODE_OBJ_SETPROPERTY`.
impl Ioctl for DrmModeObjSetProperty {
    const NAME: &'static str = "MODE_OBJ_SETPROPERTY";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, req: &mut Self) -> DrmResult<usize> {
        let object = object_for(req.obj_id, req.obj_type)?;
        set_one_property(dev, object, req.prop_id, req.value)
    }
}

/// `DRM_IOCTL_MODE_OBJ_GETPROPERTIES`.
impl Ioctl for DrmModeObjGetProperties {
    const NAME: &'static str = "MODE_OBJ_GETPROPERTIES";

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

/// `DRM_IOCTL_MODE_GETPROPERTY`.
impl Ioctl for DrmModeGetProperty {
    const NAME: &'static str = "MODE_GETPROPERTY";

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
