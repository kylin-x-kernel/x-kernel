// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DRM userspace struct definitions and ioctl encoding helpers.
//!
//! See Linux's `include/uapi/drm/drm.h` for the canonical definitions —
//! everything here is layout-compatible with that header.

#![allow(dead_code)]

use core::ffi::c_int;

use bytemuck::{AnyBitPattern, NoUninit};
use posix_types::{UserPtr, UserRead, UserWrite};

/// Size constants referenced by struct field types.
pub const DRM_MODE_NAME_LEN: usize = 32;
pub const DRM_PROP_NAME_LEN: usize = 32;

// ======== struct definitions ========

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmAuth {
    pub magic: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeDirtyFB {
    pub fb_id: u32,
    pub flags: u32,
    pub color: u32,
    pub num_clips: u32,
    pub clips_ptr: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmPrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmVersion {
    pub version_major: c_int,
    pub version_minor: c_int,
    pub version_patchlevel: c_int,
    pub name_len: usize,
    pub name: UserPtr<u8>,
    pub date_len: usize,
    pub date: UserPtr<u8>,
    pub desc_len: usize,
    pub desc: UserPtr<u8>,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmUnique {
    pub unique_len: usize,
    pub unique: UserPtr<u8>,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmSetVersion {
    pub drm_di_major: c_int,
    pub drm_di_minor: c_int,
    pub drm_dd_major: c_int,
    pub drm_dd_minor: c_int,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmGetCap {
    pub capability: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmSetClientCap {
    pub capability: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeCardRes {
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
pub struct DrmModeModeInfo {
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
pub struct DrmModeCrtc {
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
pub struct DrmModeGetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeGetConnector {
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

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeFbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeCreateDumb {
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
pub struct DrmModeMapDumb {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeDestroyDumb {
    pub handle: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeGetPlaneRes {
    pub plane_id_ptr: UserPtr<u32>,
    pub count_planes: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeGetPlane {
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
pub struct DrmModeObjGetProperties {
    pub props_ptr: UserPtr<u32>,
    pub prop_values_ptr: UserPtr<u64>,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModePropertyEnum {
    pub value: u64,
    pub name: [u8; DRM_PROP_NAME_LEN],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeGetProperty {
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
pub struct DrmModeCrtcPageFlip {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmWaitVblank {
    pub rep_type: u32,
    pub sequence: u32,
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, NoUninit)]
pub struct DrmEvent {
    pub event_type: u32,
    pub length: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, NoUninit)]
pub struct DrmEventVblank {
    pub base: DrmEvent,
    pub user_data: u64,
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub crtc_id: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeAtomic {
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
pub struct DrmModeCreateBlob {
    pub data: UserPtr<u8>,
    pub length: u32,
    pub blob_id: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeDestroyBlob {
    pub blob_id: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DrmModeGetBlob {
    pub blob_id: u32,
    pub length: u32,
    pub data: UserPtr<u8>,
}
