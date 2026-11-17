// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DRM driver constants — ioctl numbers, capability IDs, object IDs,
//! property IDs, format FourCCs, flags, limits, and mode timing.

// ======== ioctl-number encoding ========

pub(crate) const IOC_READ: u32 = 2;
pub(crate) const IOC_WRITE: u32 = 1;

/// One ioctl number built from its direction, type, sequence number and
/// argument size, as `_IOC` computes it in `asm-generic/ioctl.h`.
pub(crate) const fn ioc(dir: u32, ty: u8, nr: u8, size: u16) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | (nr as u32)
}
#[inline]
pub(crate) const fn iowr<T>(ty: u8, nr: u8) -> u32 {
    ioc(
        IOC_READ | IOC_WRITE,
        ty,
        nr,
        core::mem::size_of::<T>() as u16,
    )
}
#[inline]
pub(crate) const fn iow<T>(ty: u8, nr: u8) -> u32 {
    ioc(IOC_WRITE, ty, nr, core::mem::size_of::<T>() as u16)
}
#[inline]
pub(crate) const fn ior<T>(ty: u8, nr: u8) -> u32 {
    ioc(IOC_READ, ty, nr, core::mem::size_of::<T>() as u16)
}
#[inline]
pub(crate) const fn io(ty: u8, nr: u8) -> u32 {
    ioc(0, ty, nr, 0)
}

// ======== driver identity ========

/// Smallest scanout the connector can be asked to produce.
pub(crate) const MIN_SCANOUT_WIDTH: u32 = 32;
pub(crate) const MIN_SCANOUT_HEIGHT: u32 = 32;

/// Largest scanout the connector can be asked to produce.
///
/// A KMS client sizes its screen from this envelope at startup and cannot grow
/// past it later, so it must describe the range the host may resize within and
/// not the size the host happens to scan out today. The values match Linux's
/// virtio-gpu mode-config limits.
pub(crate) const MAX_SCANOUT_WIDTH: u32 = 8192;
pub(crate) const MAX_SCANOUT_HEIGHT: u32 = 8192;

/// `DRM_IOCTL_VERSION` major.
///
/// Linux's virtio-gpu reports `DRIVER_MAJOR = 0`; Mesa's virgl winsys rejects
/// any driver whose major is non-zero (`virgl_drm_get_version`), which is the
/// handshake that selects the virgl-capable DRI driver.
pub(crate) const DRIVER_VERSION_MAJOR: i32 = 0;
pub(crate) const DRIVER_VERSION_MINOR: i32 = 1;
pub(crate) const DRIVER_VERSION_PATCHLEVEL: i32 = 0;

// ======== ioctl numbers ========

pub(crate) const DRM_TYPE: u8 = b'd';

// ======== capability IDs ========

pub(crate) const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
pub(crate) const DRM_CAP_DUMB_PREFER_SHADOW: u64 = 0x4;
pub(crate) const DRM_CAP_CURSOR_WIDTH: u64 = 0x8;
pub(crate) const DRM_CAP_CURSOR_HEIGHT: u64 = 0x9;
pub(crate) const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
pub(crate) const DRM_CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
pub(crate) const DRM_CAP_ADDFB2_MODIFIERS: u64 = 0x10;

/// `DRM_CLOEXEC`: install the descriptor close-on-exec.
pub(crate) const DRM_CLOEXEC: u32 = 0o2000000;

/// `DRM_RDWR`: export the buffer with read/write access.
///
/// `drm.h` defines it as `O_RDWR`, so the value is 2. A client that intends to
/// read *and* write the exported buffer passes it together with `DRM_CLOEXEC`
/// (Mesa's `gbm_bo_get_fd_for_plane` passes `0x80002`); `drm_prime.c`
/// `drm_prime_handle_to_fd_ioctl` admits exactly these two flags.
pub(crate) const DRM_RDWR: u32 = 2;

/// `DRM_CAP_SYNCOBJ`: binary `SYNCOBJ_*` commands are implemented.
pub(crate) const DRM_CAP_SYNCOBJ: u64 = 0x13;
/// `DRM_CAP_PRIME`: the driver supports dma-buf import and export.
pub(crate) const DRM_CAP_PRIME: u64 = 0x5;
/// `DRM_CAP_SYNCOBJ_TIMELINE`: timeline `SYNCOBJ_*` commands are implemented.
pub(crate) const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
/// `DRM_CAP_VBLANK_HIGH_CRTC`.
pub(crate) const DRM_CAP_VBLANK_HIGH_CRTC: u64 = 0x2;
/// `DRM_CAP_DUMB_PREFERRED_DEPTH`.
pub(crate) const DRM_CAP_DUMB_PREFERRED_DEPTH: u64 = 0x3;
/// `DRM_CAP_ASYNC_PAGE_FLIP`.
pub(crate) const DRM_CAP_ASYNC_PAGE_FLIP: u64 = 0x7;
/// `DRM_CAP_PAGE_FLIP_TARGET`.
pub(crate) const DRM_CAP_PAGE_FLIP_TARGET: u64 = 0x11;
/// `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`.
pub(crate) const DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP: u64 = 0x15;

/// `DRM_CAP_PRIME` bits: import and export through dma-buf.
pub(crate) const DRM_PRIME_CAP_IMPORT: u64 = 0x1;
pub(crate) const DRM_PRIME_CAP_EXPORT: u64 = 0x2;

// ======== syncobj query flags ========

pub(crate) const DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED: u32 = 1 << 0;
pub(crate) const DRM_SYNCOBJ_QUERY_FLAGS_LAST_SIGNALED: u32 = 1 << 1;

// ======== syncobj flags ========

pub(crate) const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1 << 0;
/// `DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE`: the descriptor is a
/// sync_file whose fence becomes the object's value.
pub(crate) const DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE: u32 = 1 << 0;
/// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE`: export a sync_file
/// instead of the syncobj object descriptor.
pub(crate) const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE: u32 = 1 << 0;
pub(crate) const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1 << 0;
pub(crate) const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;
pub(crate) const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE: u32 = 1 << 2;

/// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE`: publish `deadline_nsec` on the
/// fences the wait parks on.
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    unitless_time_integer,
    reason = "CG-011-time-unit-names: Linux UAPI name `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE` from \
              `include/uapi/drm/drm.h`"
)]
pub(crate) const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE: u32 = 1 << 3;

/// Upper bound on objects one `SYNCOBJ_*` request may address.
///
/// Linux bounds the per-object arrays only by user memory; the driver bounds
/// the walk so a hostile count cannot drive an unbounded kernel loop after the
/// arrays were copied.
pub(crate) const DRM_SYNCOBJ_MAX_HANDLES: u32 = 1024;

// ======== dma-buf ioctl numbers (DMA_BUF_BASE 'b') ========

/// `_IOW('b', 0, struct dma_buf_sync)`.
pub(crate) const DMA_BUF_IOCTL_SYNC_CMD: u8 = 0x0;
/// `_IOW('b', 2, struct dma_buf_export_sync_file)`.
pub(crate) const DMA_BUF_IOCTL_EXPORT_SYNC_FILE_CMD: u8 = 0x2;
/// `_IOW('b', 3, struct dma_buf_import_sync_file)`.
pub(crate) const DMA_BUF_IOCTL_IMPORT_SYNC_FILE_CMD: u8 = 0x3;

pub(crate) const DMA_BUF_BASE: u8 = b'b';

/// `DMA_BUF_SYNC_VALID_FLAGS_MASK`: `DMA_BUF_SYNC_RW | DMA_BUF_SYNC_END`.
pub(crate) const DMA_BUF_SYNC_VALID_FLAGS_MASK: u64 = 3 | (1 << 2);
/// `DMA_BUF_NAME_LEN`: the longest name `DMA_BUF_SET_NAME` accepts.
pub(crate) const DMA_BUF_NAME_LEN: usize = 32;

/// Sequence number of `DMA_BUF_SET_NAME`, which Linux spells once per
/// user-pointer width (`_IOW('b', 1, const char *)` and the `__u32`/`__u64`
/// variants). There is no size-query command: a client reads the size from
/// `fstat`'s `st_size`, which the descriptor reports.
pub(crate) const DMA_BUF_BASE_SEQ_1: u8 = 0x1;

/// `DMA_BUF_IOCTL_SYNC_END`: publish completed CPU access. The start direction is
/// the absence of this bit, because `DMA_BUF_SYNC_START` is zero.
pub(crate) const DMA_BUF_SYNC_END: u64 = 1 << 2;
/// `DMA_BUF_SYNC_RW`.
pub(crate) const DMA_BUF_SYNC_RW: u64 = 3;
/// `DMA_BUF_SYNC_READ`.
pub(crate) const DMA_BUF_SYNC_READ: u64 = 1;
const _: () = assert!(DMA_BUF_SYNC_READ == 1);
/// `DMA_BUF_SYNC_WRITE`.
pub(crate) const DMA_BUF_SYNC_WRITE: u64 = 2;
/// `DMA_BUF_SYNC_WAIT`. It is part of `DMA_BUF_SYNC_VALID_FLAGS_MASK` only in
/// the sense that a wait is the default; Linux defines it as a zero bit.
pub(crate) const DMA_BUF_SYNC_WAIT: u64 = 0;
const _: () = assert!(DMA_BUF_SYNC_WAIT == 0);
const _: () = assert!(DMA_BUF_SYNC_VALID_FLAGS_MASK == 0x7);
/// `DMA_BUF_SYNC_NOWAIT`: a legacy DMA-BUF API bit outside the validation
/// mask, so Linux rejects it; it is kept for completeness of the ABI table.
pub(crate) const DMA_BUF_SYNC_NOWAIT: u64 = 1 << 4;
const _: () = assert!(DMA_BUF_SYNC_NOWAIT & DMA_BUF_SYNC_VALID_FLAGS_MASK == 0);

// ======== sync_file ioctl numbers (SYNC_IOC_MAGIC '>') ========

// ======== MODE_DIRTYFB flags ========
pub(crate) const DRM_MODE_FB_DIRTY_ANNOTATE_COPY: u32 = 0x01;
pub(crate) const DRM_MODE_FB_DIRTY_ANNOTATE_FILL: u32 = 0x02;
/// Every flag bit the ABI defines; unknown bits are masked off, not rejected.
pub(crate) const DRM_MODE_FB_DIRTY_FLAGS: u32 =
    DRM_MODE_FB_DIRTY_ANNOTATE_COPY | DRM_MODE_FB_DIRTY_ANNOTATE_FILL;
/// Upper bound on clip rectangles, `DRM_MODE_FB_DIRTY_MAX_CLIPS` in `drm.h`.
pub(crate) const DRM_MODE_FB_DIRTY_MAX_CLIPS: u32 = 256;

/// `DRM_CRTC_SEQUENCE_RELATIVE`.
pub(crate) const DRM_CRTC_SEQUENCE_RELATIVE: u32 = 0x1;
/// `DRM_CRTC_SEQUENCE_NEXT_ON_MISS`.
pub(crate) const DRM_CRTC_SEQUENCE_NEXT_ON_MISS: u32 = 0x2;

/// `DRM_MODE_TYPE_PREFERRED`, the type bit of the connector's primary mode.
pub(crate) const DRM_MODE_TYPE_PREFERRED: u8 = 0x8;
/// `DRM_MODE_FLAG_INTERLACE`.
pub(crate) const DRM_MODE_FLAG_INTERLACE: u16 = 1 << 4;

// ======== SET_CLIENT_CAP capabilities ========

// Values are Linux's (`include/uapi/drm/drm.h`); the numbering is part of the
// uapi contract, because clients index their own state by it.
pub(crate) const DRM_CLIENT_CAP_STEREO_3D: u64 = 1;
pub(crate) const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub(crate) const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
pub(crate) const DRM_CLIENT_CAP_ASPECT_RATIO: u64 = 4;
pub(crate) const DRM_CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;
pub(crate) const DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;

// ======== format modifiers ========

pub(crate) const DRM_MODE_FB_MODIFIERS: u32 = 0x2;
/// `DRM_MODE_FB_INTERLACED`: the framebuffer is interlaced.
pub(crate) const DRM_MODE_FB_INTERLACED: u32 = 0x1;
/// Every `ADDFB2` flag Linux knows (`drm_internal_framebuffer_create`).
pub(crate) const DRM_MODE_FB_FLAGS: u32 = DRM_MODE_FB_INTERLACED | DRM_MODE_FB_MODIFIERS;

pub(crate) const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
pub(crate) const DRM_FORMAT_MOD_LINEAR: u64 = 0;

// ======== MODE_CURSOR flags ========

// Linux `DRM_MODE_CURSOR_BO` / `_MOVE` (include/uapi/drm/drm_mode.h).
/// Install or replace the cursor image (`handle` is meaningful).
pub(crate) const DRM_MODE_CURSOR_BO: u32 = 0x01;
/// Move the cursor to `x` / `y`.
pub(crate) const DRM_MODE_CURSOR_MOVE: u32 = 0x02;

// ======== format FourCCs ========

pub(crate) const DRM_FORMAT_XRGB8888: u32 =
    (b'X' as u32) | ((b'R' as u32) << 8) | ((b'2' as u32) << 16) | ((b'4' as u32) << 24);
pub(crate) const DRM_FORMAT_ARGB8888: u32 =
    (b'A' as u32) | ((b'R' as u32) << 8) | ((b'2' as u32) << 16) | ((b'4' as u32) << 24);

// ======== object type tags ========

pub(crate) const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
pub(crate) const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
pub(crate) const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;
pub(crate) const DRM_MODE_OBJECT_FB: u32 = 0xfbfb_fbfb;

// ======== connector / encoder types ========

pub(crate) const DRM_MODE_CONNECTED: u32 = 1;
/// `DRM_MODE_DISCONNECTED`: the connector has nothing attached.
pub(crate) const DRM_MODE_DISCONNECTED: u32 = 2;
pub(crate) const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
pub(crate) const DRM_MODE_ENCODER_VIRTUAL: u32 = 5;

// ======== plane / property flags ========

pub(crate) const DRM_PLANE_TYPE_PRIMARY: u64 = 1;

pub(crate) const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
pub(crate) const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub(crate) const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub(crate) const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub(crate) const DRM_MODE_PROP_OBJECT: u32 = 1 << 6;
pub(crate) const DRM_MODE_PROP_SIGNED_RANGE: u32 = 1 << 7;
pub(crate) const DRM_MODE_PROP_ATOMIC: u32 = 0x8000_0000;

// ======== page flip / vblank / event flags ========

pub(crate) const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
pub(crate) const DRM_VBLANK_RELATIVE: u32 = 0x1;
/// `DRM_VBLANK_HIGH_CRTC`: the CRTC is the low bits of `sequence`.
pub(crate) const DRM_VBLANK_HIGH_CRTC: u32 = 0x2;
/// `DRM_VBLANK_NONBLOCK`: answer immediately instead of sleeping.
pub(crate) const DRM_VBLANK_NONBLOCK: u32 = 0x4;
/// `DRM_VBLANK_ABSOLUTE_BLANKED`: an absolute request whose sequence starts a
/// blank.
pub(crate) const DRM_VBLANK_ABSOLUTE_BLANKED: u32 = 0x8;
/// `DRM_VBLANK_RELATIVE_BLANKED`: the same, counted from this file.
pub(crate) const DRM_VBLANK_RELATIVE_BLANKED: u32 = 0x10;
pub(crate) const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

// ======== atomic commit flags ========

pub(crate) const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
pub(crate) const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
/// `DRM_MODE_PAGE_FLIP_ASYNC`: accepted only by a device that supports async
/// flips. `MODE_ATOMIC` rejects the bit with `EINVAL` when it does, exactly as
/// `drm_mode_atomic_ioctl` does for a device without `async_page_flip`.
pub(crate) const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 0x02;
/// `DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE`.
pub(crate) const DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE: u32 = 0x04;
/// `DRM_MODE_PAGE_FLIP_TARGET_RELATIVE`.
pub(crate) const DRM_MODE_PAGE_FLIP_TARGET_RELATIVE: u32 = 0x08;
/// `DRM_MODE_PAGE_FLIP_TARGET`.
pub(crate) const DRM_MODE_PAGE_FLIP_TARGET: u32 =
    DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE | DRM_MODE_PAGE_FLIP_TARGET_RELATIVE;
/// `DRM_MODE_PAGE_FLIP_FLAGS`: the flag bits `MODE_PAGE_FLIP` understands.
///
/// This card never flips asynchronously, so `DRM_MODE_PAGE_FLIP_ASYNC` is in
/// Linux's mask but not in this driver's: the ioctl refuses it the same way
/// Linux refuses it on a device without `async_page_flip`.
pub(crate) const DRM_MODE_PAGE_FLIP_FLAGS: u32 =
    DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_PAGE_FLIP_TARGET;
pub(crate) const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;

// ======== fixed object IDs ========

pub(crate) const CRTC_ID: u32 = 0x10;
pub(crate) const ENCODER_ID: u32 = 0x20;
pub(crate) const CONNECTOR_ID: u32 = 0x30;
pub(crate) const PLANE_ID: u32 = 0x40;

// ======== property IDs ========
// Layout: 0x1xx = plane, 0x2xx = CRTC, 0x3xx = connector.

pub(crate) const PROP_PLANE_TYPE: u32 = 0x100;
pub(crate) const PROP_PLANE_FB_ID: u32 = 0x101;
pub(crate) const PROP_PLANE_CRTC_ID: u32 = 0x102;
pub(crate) const PROP_PLANE_SRC_X: u32 = 0x103;
pub(crate) const PROP_PLANE_SRC_Y: u32 = 0x104;
pub(crate) const PROP_PLANE_SRC_W: u32 = 0x105;
pub(crate) const PROP_PLANE_SRC_H: u32 = 0x106;
pub(crate) const PROP_PLANE_CRTC_X: u32 = 0x107;
pub(crate) const PROP_PLANE_CRTC_Y: u32 = 0x108;
pub(crate) const PROP_PLANE_CRTC_W: u32 = 0x109;
pub(crate) const PROP_PLANE_CRTC_H: u32 = 0x10A;
pub(crate) const PROP_PLANE_IN_FORMATS: u32 = 0x10B;
pub(crate) const PROP_PLANE_IN_FENCE_FD: u32 = 0x10C;
/// `FB_DAMAGE_CLIPS` (plane blob).
pub(crate) const PROP_PLANE_FB_DAMAGE_CLIPS: u32 = 0x10D;

pub(crate) const PROP_CRTC_ACTIVE: u32 = 0x200;
pub(crate) const PROP_CRTC_MODE_ID: u32 = 0x201;
pub(crate) const PROP_CRTC_OUT_FENCE_PTR: u32 = 0x202;

pub(crate) const PROP_CONN_CRTC_ID: u32 = 0x300;
/// `max bpc` (connector colour depth).
pub(crate) const PROP_CONN_MAX_BPC: u32 = 0x301;
/// `EDID` (connector descriptor blob).
pub(crate) const PROP_CONN_EDID: u32 = 0x302;
/// `DPMS` (connector power state).
pub(crate) const PROP_CONN_DPMS: u32 = 0x303;
/// `link-status` (connector link health as the client reports it).
pub(crate) const PROP_CONN_LINK_STATUS: u32 = 0x304;
/// `non-desktop` (whether the connector shows a desktop).
pub(crate) const PROP_CONN_NON_DESKTOP: u32 = 0x305;

/// `DRM_MODE_DPMS_ON`.
pub(crate) const DRM_MODE_DPMS_ON: u64 = 0;
/// `DRM_MODE_DPMS_STANDBY`.
pub(crate) const DRM_MODE_DPMS_STANDBY: u64 = 1;
/// `DRM_MODE_DPMS_SUSPEND`.
pub(crate) const DRM_MODE_DPMS_SUSPEND: u64 = 2;
/// `DRM_MODE_DPMS_OFF`.
pub(crate) const DRM_MODE_DPMS_OFF: u64 = 3;
/// Every `DRM_MODE_DPMS_*` value.
pub(crate) const DRM_MODE_DPMS_MAX: u64 = DRM_MODE_DPMS_OFF;

/// `DRM_MODE_LINK_STATUS_GOOD`.
pub(crate) const DRM_MODE_LINK_STATUS_GOOD: u64 = 1;
/// `DRM_MODE_LINK_STATUS_BAD`.
pub(crate) const DRM_MODE_LINK_STATUS_BAD: u64 = 2;

// ======== buffer / blob limits ========

pub(crate) const FIRST_DUMB_HANDLE: u32 = 1;

/// First 3D context id handed to a client.
pub(crate) const FIRST_CONTEXT_ID: u32 = 1;
pub(crate) const FIRST_FB_ID: u32 = 1;
pub(crate) const DUMB_BUFFER_MAX_SIZE: usize = 8 * 1024 * 1024;

pub(crate) const SUPPORTED_FORMATS: &[u32] = &[DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888];

pub(crate) const MAX_EVENTS: usize = 128;
pub(crate) const FIRST_BLOB_ID: u32 = 0x1000;
pub(crate) const MAX_BLOB_BYTES: usize = 64 * 1024;

// ======== CVT-RBv1 mode timing ========

// The IN_FORMATS blob encodes the format set as a single 64-bit mask, so the
// table must never grow past the width it is shifted into.
const _: () = assert!(SUPPORTED_FORMATS.len() <= 64);
