// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The committed KMS state and its codec.
//!
//! [`KmsState`] is plain data, cloned under the commit lock; [`apply`] is the
//! one writer of a state copy, and the mode-blob helpers encode and decode the
//! `MODE_ID` payload. The property ioctls (`props.rs`) and the modeset ioctls
//! (`modeset.rs`) both reach the state only through these items.

use alloc::sync::Arc;

use crate::prelude::*;

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
