// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DRM userspace struct definitions and ioctl encoding helpers.
//!
//! See Linux's `include/uapi/drm/drm.h` for the canonical definitions —
//! everything here is layout-compatible with that header.

use core::ffi::c_int;

use bytemuck::{AnyBitPattern, NoUninit};
use posix_types::{UserPtr, UserRead, UserWrite};

/// Size constants referenced by struct field types.
pub(crate) const DRM_MODE_NAME_LEN: usize = 32;
pub(crate) const DRM_PROP_NAME_LEN: usize = 32;

// ======== struct definitions ========

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmAuth {
    pub magic: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeDirtyFb {
    pub fb_id: u32,
    pub flags: u32,
    pub color: u32,
    pub num_clips: u32,
    pub clips_ptr: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmPrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVersion {
    pub version_major: c_int,
    pub version_minor: c_int,
    pub version_patchlevel: c_int,
    pub abi_pad: u32,
    pub name_len: usize,
    pub name: UserPtr<u8>,
    pub date_len: usize,
    pub date: UserPtr<u8>,
    pub desc_len: usize,
    pub desc: UserPtr<u8>,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmUnique {
    pub unique_len: usize,
    pub unique: UserPtr<u8>,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSetVersion {
    pub drm_di_major: c_int,
    pub drm_di_minor: c_int,
    pub drm_dd_major: c_int,
    pub drm_dd_minor: c_int,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmGetCap {
    pub capability: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSetClientCap {
    pub capability: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCardRes {
    pub fb_id_ptr: UserPtr<u32>,
    pub crtc_id_ptr: UserPtr<u32>,
    pub connector_id_ptr: UserPtr<u32>,
    pub encoder_id_ptr: UserPtr<u32>,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub kind: u32,
    pub name: [u8; DRM_MODE_NAME_LEN],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCrtc {
    pub set_connectors_ptr: UserPtr<u32>,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmModeModeInfo,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeGetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeGetConnector {
    pub encoders_ptr: UserPtr<u32>,
    pub modes_ptr: UserPtr<DrmModeModeInfo>,
    pub props_ptr: UserPtr<u32>,
    pub prop_values_ptr: UserPtr<u64>,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}

/// Legacy `struct drm_mode_fb_cmd`, as still issued by non-atomic clients.
///
/// Linux keeps both this and [`DrmModeFbCmd2`]: atomic clients use the latter,
/// while older mode-setting code (Xorg's `modesetting` driver among them) uses
/// this one. 7 x u32 = 28 bytes, which is exactly the `0x1c` size field encoded
/// in `DRM_IOCTL_MODE_ADDFB` (`0xc01c64ae`) - two independent confirmations of
/// the layout.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeFbCmd {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    pub handle: u32,
}

/// `struct drm_mode_cursor`, the original cursor request.
///
/// 7 x u32 = 28 bytes = 0x1c, which is the size field encoded in
/// `DRM_IOCTL_MODE_CURSOR` (`0xc01c64a3`).  `width`/`height` are part of the
/// structure in `include/uapi/drm/drm_mode.h` and cannot be turned into padding:
/// dropping them shifts `handle` and changes the command word, so a client that
/// speaks the uapi would miss the handler entirely.
///
/// [`DrmModeCursor2`] adds the two hotspot offsets and is 36 bytes, matching
/// `DRM_IOCTL_MODE_CURSOR2` (`0xc02464bb`).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCursor {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
}

/// `struct drm_mode_cursor2`, which adds the hotspot offsets.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCursor2 {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
    pub hot_x: i32,
    pub hot_y: i32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeFbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub abi_pad: u32,
    pub modifier: [u64; 4],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeMapDumb {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeDestroyDumb {
    pub handle: u32,
}

/// `struct drm_mode_connector_set_property`, the legacy connector-only setter.
///
/// `value` comes first and the structure is 16 bytes with no trailing padding, so
/// `DRM_IOCTL_MODE_SETPROPERTY` carries size 0x10.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeConnectorSetProperty {
    pub value: u64,
    pub prop_id: u32,
    pub connector_id: u32,
}

/// `struct drm_mode_obj_set_property`, the object-typed setter.
///
/// `value` at 0, `prop_id` at 8, `obj_id` at 12, `obj_type` at 16, and the uapi's
/// 4 trailing pad bytes at 20, spelled out as `abi_pad` instead of left implicit.
/// The hole cannot stay implicit because `handle_raw` writes the whole request
/// back once the handler succeeds: an implicit hole would hand uninitialised
/// kernel memory to user space and break what `UserWrite` promises.
/// `DrmVersion`, `DrmModeFbCmd2`, `DrmModeGetPlaneRes` and
/// `DrmModeObjGetProperties` name their padding for the same reason.
///
/// The size pins below keep that layout true: the size is part of the command
/// word, so a layout change that went unnoticed would leave the handler
/// registered under a number no client sends.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeObjSetProperty {
    pub value: u64,
    pub prop_id: u32,
    pub obj_id: u32,
    pub obj_type: u32,
    pub abi_pad: u32,
}

// Pinned by matching array lengths rather than `assert!`: a mismatch is a type
// error, so it needs no exception to the workspace's `-D warnings` policy.
const _: [(); 16] = [(); core::mem::size_of::<DrmModeConnectorSetProperty>()];
const _: [(); 24] = [(); core::mem::size_of::<DrmModeObjSetProperty>()];

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeGetPlaneRes {
    pub plane_id_ptr: UserPtr<u32>,
    pub count_planes: u32,
    pub abi_pad: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeGetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: UserPtr<u32>,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeObjGetProperties {
    pub props_ptr: UserPtr<u32>,
    pub prop_values_ptr: UserPtr<u64>,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
    pub abi_pad: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModePropertyEnum {
    pub value: u64,
    pub name: [u8; DRM_PROP_NAME_LEN],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeGetProperty {
    pub values_ptr: UserPtr<u64>,
    pub enum_blob_ptr: UserPtr<DrmModePropertyEnum>,
    pub prop_id: u32,
    pub flags: u32,
    pub name: [u8; DRM_PROP_NAME_LEN],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCrtcPageFlip {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmWaitVblank {
    pub rep_type: u32,
    pub sequence: u32,
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, NoUninit)]
pub(crate) struct DrmEvent {
    pub event_type: u32,
    pub length: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, NoUninit)]
pub(crate) struct DrmEventVblank {
    pub base: DrmEvent,
    pub user_data: u64,
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub crtc_id: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeAtomic {
    pub flags: u32,
    pub count_objs: u32,
    pub objs_ptr: UserPtr<u32>,
    pub count_props_ptr: UserPtr<u32>,
    pub props_ptr: UserPtr<u32>,
    pub prop_values_ptr: UserPtr<u64>,
    pub reserved: u64,
    pub user_data: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCreateBlob {
    pub data: UserPtr<u8>,
    pub length: u32,
    pub blob_id: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeDestroyBlob {
    pub blob_id: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeGetBlob {
    pub blob_id: u32,
    pub length: u32,
    pub data: UserPtr<u8>,
}
