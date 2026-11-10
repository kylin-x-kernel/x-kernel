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

/// One damage rectangle of `MODE_DIRTYFB`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmClipRect {
    pub x1: i16,
    pub y1: i16,
    pub x2: i16,
    pub y2: i16,
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

/// `DRM_IOCTL_MODE_GETFB2`.
///
/// The same wire structure as `ADDFB2`, but the direction is reversed: the
/// client names an `fb_id` and the kernel fills geometry and per-plane handles.
#[repr(transparent)]
#[derive(Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeGetFb2(pub(crate) DrmModeFbCmd2);

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

/// `struct drm_gem_close`, the GEM spelling of the dumb-buffer release.
///
/// `DRM_IOCTL_GEM_CLOSE` is `DRM_IOW(0x09, struct drm_gem_close)`: 8 bytes, and
/// Linux answers `EINVAL` for a non-zero `pad` before it looks at the handle.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmGemClose {
    pub handle: u32,
    pub pad: u32,
}

/// `struct drm_gem_flink`: `DRM_IOCTL_GEM_FLINK` request and reply.
///
/// `DRM_IOWR(0x0A, struct drm_gem_flink)`: the caller names a handle, the
/// kernel returns the object's global name.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmGemFlink {
    pub handle: u32,
    /// The global name, a `__u32` in `struct drm_gem_flink`.
    pub name: u32,
}

/// `struct drm_gem_open`: `DRM_IOCTL_GEM_OPEN` request and reply.
///
/// `DRM_IOWR(0x0B, struct drm_gem_open)`: the caller names a global name, the
/// kernel returns a handle of the calling file and the object's byte size.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmGemOpen {
    pub name: u32,
    /// Returned handle for the object.
    pub handle: u32,
    /// `obj->size` in bytes.
    pub size: u64,
}

/// `struct drm_mode_closefb`, the checked form of `DRM_IOCTL_MODE_RMFB`.
///
/// `DRM_IOCTL_MODE_CLOSEFB` is `DRM_IOWR(0xD0, struct drm_mode_closefb)`.
/// Linux checks `pad` first (`EINVAL`), then requires the framebuffer to exist
/// (`ENOENT`), and refuses to close one that is still bound (`EBUSY`).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCloseFb {
    pub fb_id: u32,
    pub pad: u32,
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
    /// Trailing padding: the three `__u32` fields end at byte 28 and the
    /// structure aligns to 8, so `sizeof` is 32 (`0xc020_64b9`).
    pub abi_pad: u32,
}

const _: [(); 32] = [(); core::mem::size_of::<DrmModeObjGetProperties>()];

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

/// `struct drm_mode_crtc_page_flip_target`: the wire form of
/// `DRM_IOCTL_MODE_PAGE_FLIP`.
///
/// `DRM_MODE_PAGE_FLIP_TARGET_*` requests need a driver that reports its own
/// vblank; this card rejects the target flags rather than silently flipping
/// immediately.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCrtcPageFlip {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub abi_pad: u32,
    pub user_data: u64,
}

// `_IOWR('d', 0xB0, struct drm_mode_crtc_page_flip)` is 24 bytes
// (`0xc018_64b0`). The 32-byte `page_flip_user_data` form belongs to the
// out-fence property, not to this ioctl.
const _: [(); 24] = [(); core::mem::size_of::<DrmModeCrtcPageFlip>()];

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

/// `struct drm_syncobj_create`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjCreate {
    pub handle: u32,
    pub flags: u32,
}

/// `struct drm_syncobj_destroy`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjDestroy {
    pub handle: u32,
    pub abi_pad: u32,
}

/// `struct drm_syncobj_handle`: the handle/fd transfer request.
///
/// Both directions of the transfer use this structure, because the wire layout
/// is symmetric; only the ioctl number differs.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
    pub abi_pad: u32,
}

/// `struct drm_syncobj_array`, for signal, reset and wait.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjArray {
    pub handles: UserPtr<u32>,
    pub count_handles: u32,
    pub abi_pad: u32,
}

/// `struct drm_syncobj_timeline_array`, for timeline signal and wait.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjTimelineArray {
    pub handles: UserPtr<u32>,
    pub points: UserPtr<u64>,
    pub count_handles: u32,
    pub flags: u32,
}

/// `struct drm_syncobj_eventfd`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjEventfd {
    pub handle: u32,
    pub flags: u32,
    pub point: u64,
    pub fd: i32,
    pub abi_pad: u32,
}

/// `struct drm_syncobj_wait` and `struct drm_syncobj_timeline_wait`.
///
/// The two wire structures are identical; only the ioctl numbers differ.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjWait {
    pub handles: UserPtr<u32>,
    pub timeout_nsec: i64,
    pub count_handles: u32,
    pub flags: u32,
    pub first_signaled: u32,
    pub abi_pad: u32,
    pub dead_line_seqno: u64,
}

/// `struct drm_syncobj_transfer`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjTransfer {
    pub src_handle: u32,
    pub dst_handle: u32,
    pub src_point: u64,
    pub dst_point: u64,
    pub flags: u32,
    pub abi_pad: u32,
}

/// `struct drm_syncobj_query_args`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjQuery {
    pub handles: UserPtr<u32>,
    pub points: UserPtr<u64>,
    pub count_handles: u32,
    pub flags: u32,
}

const _: [(); 8] = [(); core::mem::size_of::<DrmSyncobjCreate>()];
const _: [(); 8] = [(); core::mem::size_of::<DrmSyncobjDestroy>()];
const _: [(); 16] = [(); core::mem::size_of::<DrmSyncobjHandle>()];
const _: [(); 16] = [(); core::mem::size_of::<DrmSyncobjArray>()];
const _: [(); 24] = [(); core::mem::size_of::<DrmSyncobjTimelineArray>()];
const _: [(); 24] = [(); core::mem::size_of::<DrmSyncobjEventfd>()];
const _: [(); 40] = [(); core::mem::size_of::<DrmSyncobjWait>()];
const _: [(); 32] = [(); core::mem::size_of::<DrmSyncobjTransfer>()];
const _: [(); 24] = [(); core::mem::size_of::<DrmSyncobjQuery>()];

/// `struct sync_file_info`, as `sync_file.c` writes it.
///
/// The structure is 56 bytes: a 32-byte name, status/flags/num_fences/pad and
/// the fence-info pointer. Its size is encoded in `SYNC_IOC_FILE_INFO`
/// (`_IOWR('>', 4, struct sync_file_info)` is `0xc038_3e04`), so the layout has
/// to match Linux byte for byte.
#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncFileInfo {
    /// The reported name, `strlen(name) + 1` bytes on the wire.
    pub name: [u8; 32],
    /// `sync_file_status`.
    pub status: i32,
    /// Fence flags.
    pub flags: u32,
    /// Record count on input (the caller's capacity) and on output.
    pub num_fences: u32,
    /// Padding before the pointer.
    pub abi_pad: u32,
    /// The caller's `sync_fence_info` buffer.
    pub sync_fence_info: UserPtr<u8>,
}

/// `struct sync_fence_info`: one fence of a `sync_file_info` answer.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncFenceInfo {
    pub obj_name: [u8; 32],
    pub driver_name: [u8; 32],
    pub status: i32,
    /// `sync_fence_info.flags` (there is no sequence number in the UAPI; a
    /// client that read `seqno` here would be reading a field Linux never
    /// writes).
    pub flags: u32,
    pub timestamp_ns: u64,
}

/// `struct sync_set_deadline`: the payload of `SYNC_IOC_SET_DEADLINE`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncSetDeadline {
    /// Absolute CLOCK_MONOTONIC deadline in nanoseconds.
    pub deadline_ns: u64,
    /// Reserved; must be zero.
    pub abi_pad: u64,
}

const _: [(); 16] = [(); core::mem::size_of::<DrmSyncSetDeadline>()];

/// `struct sync_merge_data`: the payload of `SYNC_IOC_MERGE`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncMergeData {
    pub name: [u8; 32],
    pub fd2: i32,
    pub fence: i32,
    pub flags: u32,
    pub abi_pad: u32,
}

const _: [(); 48] = [(); core::mem::size_of::<DrmSyncMergeData>()];

/// `struct dma_buf_export_sync_file`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DmaBufExportSyncFile {
    pub flags: u32,
    pub fd: i32,
}

/// `struct dma_buf_import_sync_file`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DmaBufImportSyncFile {
    pub flags: u32,
    pub fd: i32,
}

const _: [(); 8] = [(); core::mem::size_of::<DmaBufExportSyncFile>()];
const _: [(); 8] = [(); core::mem::size_of::<DmaBufImportSyncFile>()];
const _: [(); 48] = [(); core::mem::offset_of!(DrmSyncFileInfo, sync_fence_info)];
const _: [(); 56] = [(); core::mem::size_of::<DrmSyncFileInfo>()];
const _: [(); 80] = [(); core::mem::size_of::<DrmSyncFenceInfo>()];

/// `struct dma_buf_sync`.
///
/// Exactly one `__u64`; the ioctl number encodes that size
/// (`_IOW('b', 0, struct dma_buf_sync)` is `0x4008_6200`), so any padding here
/// would make the command unrecognisable to a Linux-built client.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DmaBufSync {
    pub flags: u64,
}

const _: [(); 8] = [(); core::mem::size_of::<DmaBufSync>()];

/// `DRM_IOCTL_SYNCOBJ_SIGNAL`: signal a set of binary syncobjs.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjSignal(pub(crate) DrmSyncobjArray);

/// `DRM_IOCTL_SYNCOBJ_RESET`: clear a set of binary syncobjs.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjReset(pub(crate) DrmSyncobjArray);

/// `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT`: wait for timeline points.
///
/// `struct drm_syncobj_timeline_wait` is 40 bytes like `drm_syncobj_wait`, but
/// the extra `points` word shifts every later field by eight bytes: a client
/// built against the real header would otherwise have its `timeout_nsec` read
/// as `points`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjTimelineWait {
    pub handles: UserPtr<u32>,
    /// One timeline point per handle.
    pub points: UserPtr<u64>,
    pub timeout_nsec: i64,
    pub count_handles: u32,
    pub flags: u32,
    pub first_signaled: u32,
    pub abi_pad: u32,
    pub dead_line_seqno: u64,
}

const _: [(); 48] = [(); core::mem::size_of::<DrmSyncobjTimelineWait>()];

/// `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE`: import a sync_file into a syncobj.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSyncobjFdToHandle(pub(crate) DrmSyncobjHandle);

// The request sizes of the commands this driver implements, as a C compiler
// computes them from `include/uapi/drm/`. Every one is encoded into its ioctl
// number, so a shape change here is an ABI break, not a refactor.
const _: () = {
    assert!(core::mem::size_of::<DrmVersion>() == 64);
    assert!(core::mem::size_of::<DrmUnique>() == 16);
    assert!(core::mem::size_of::<DrmSetVersion>() == 16);
    assert!(core::mem::size_of::<DrmGemClose>() == 8);
    assert!(core::mem::size_of::<DrmGemFlink>() == 8);
    assert!(core::mem::size_of::<DrmGemOpen>() == 16);
    assert!(core::mem::size_of::<DrmGetCap>() == 16);
    assert!(core::mem::size_of::<DrmSetClientCap>() == 16);
    assert!(core::mem::size_of::<DrmPrimeHandle>() == 12);
    assert!(core::mem::size_of::<DrmModeFbCmd>() == 28);
    assert!(core::mem::size_of::<DrmModeFbCmd2>() == 104);
    assert!(core::mem::size_of::<DrmModeObjGetProperties>() == 32);
    assert!(core::mem::size_of::<DrmModeObjSetProperty>() == 24);
    assert!(core::mem::size_of::<DrmModeAtomic>() == 56);
    assert!(core::mem::size_of::<DrmModeCursor2>() == 36);
    assert!(core::mem::size_of::<DrmModeCreateDumb>() == 32);
    assert!(core::mem::size_of::<DrmModeMapDumb>() == 16);
    assert!(core::mem::size_of::<DrmSyncFileInfo>() == 56);
};

/// `struct drm_client`, the `GET_CLIENT` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmClient {
    pub idx: c_int,
    pub auth: c_int,
    pub pid: u64,
    pub uid: u64,
    pub magic: u64,
    pub iocs: u64,
}

/// `struct drm_stats`, the `GET_STATS` argument: the legacy counters, all zero.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct DrmStatData {
    pub value: u64,
    pub kind: c_int,
    pub abi_pad: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmStats {
    pub count: u64,
    pub data: [DrmStatData; 15],
}

/// `struct drm_mode_crtc_lut`, the `GETGAMMA`/`SETGAMMA` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeCrtcLut {
    pub crtc_id: u32,
    pub gamma_size: u32,
    pub red: UserPtr<u16>,
    pub green: UserPtr<u16>,
    pub blue: UserPtr<u16>,
}

/// `struct drm_crtc_get_sequence`, the `CRTC_GET_SEQUENCE` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmCrtcGetSequence {
    pub crtc_id: u32,
    pub active: u32,
    pub sequence: u64,
    pub sequence_ns: i64,
}

/// `struct drm_crtc_queue_sequence`, the `CRTC_QUEUE_SEQUENCE` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmCrtcQueueSequence {
    pub crtc_id: u32,
    pub flags: u32,
    pub sequence: u64,
    pub user_data: u64,
}

/// `struct drm_mode_set_plane`, the `MODE_SETPLANE` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeSetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: c_int,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
}

/// `struct drm_block`, the `BLOCK`/`UNBLOCK` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmBlock {
    pub unused: c_int,
}

/// `struct drm_draw`, the `ADD_DRAW`/`RM_DRAW` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmDraw {
    pub handle: u32,
}

/// `struct drm_update_draw`, the `UPDATE_DRAW` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmUpdateDraw {
    pub handle: u32,
    pub kind: u32,
    pub num: u32,
    pub abi_pad: u32,
    pub data: u64,
}

/// `struct drm_lock`, the `FINISH` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmLock {
    pub context: c_int,
    pub flags: u32,
}

/// `struct drm_mode_mode_cmd`, the deprecated `ATTACHMODE`/`DETACHMODE` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmModeModeCmd {
    pub connector_id: u32,
    pub abi_pad: u32,
    pub mode: DrmModeModeInfo,
}

/// `struct drm_set_client_name`, the `SET_CLIENT_NAME` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSetClientName {
    pub name_len: u64,
    pub name: UserPtr<u8>,
}

/// `struct drm_gem_change_handle`, the `GEM_CHANGE_HANDLE` argument.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmGemChangeHandle {
    pub handle: u32,
    pub new_handle: u32,
}

const _: () = {
    assert!(core::mem::size_of::<DrmClient>() == 40);
    assert!(core::mem::size_of::<DrmStats>() == 248);
    assert!(core::mem::size_of::<DrmModeCrtcLut>() == 32);
    assert!(core::mem::size_of::<DrmCrtcGetSequence>() == 24);
    assert!(core::mem::size_of::<DrmCrtcQueueSequence>() == 24);
    assert!(core::mem::size_of::<DrmModeSetPlane>() == 48);
    assert!(core::mem::size_of::<DrmBlock>() == 4);
    assert!(core::mem::size_of::<DrmDraw>() == 4);
    assert!(core::mem::size_of::<DrmUpdateDraw>() == 24);
    assert!(core::mem::size_of::<DrmLock>() == 8);
    assert!(core::mem::size_of::<DrmModeModeCmd>() == 76);
    assert!(core::mem::size_of::<DrmSetClientName>() == 16);
    assert!(core::mem::size_of::<DrmGemChangeHandle>() == 8);
};
