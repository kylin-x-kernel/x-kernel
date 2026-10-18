// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DRM driver constants — ioctl numbers, capability IDs, object IDs,
//! property IDs, format FourCCs, flags, limits, and mode timing.

// ======== ioctl-number encoding ========

const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;

const fn ioc(dir: u32, ty: u8, nr: u8, size: u16) -> u32 {
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
pub(crate) const fn io(ty: u8, nr: u8) -> u32 {
    ioc(0, ty, nr, 0)
}

// ======== driver identity ========

pub(crate) const DRIVER_NAME: &str = "simpledrm";
pub(crate) const DRIVER_DATE: &str = "2026-06-02";
pub(crate) const DRIVER_DESC: &str = "X-Kernel simple DRM driver";
pub(crate) const DRIVER_VERSION_MAJOR: i32 = 1;
pub(crate) const DRIVER_VERSION_MINOR: i32 = 0;
pub(crate) const DRIVER_VERSION_PATCHLEVEL: i32 = 0;

// ======== ioctl numbers ========

pub(crate) const DRM_TYPE: u8 = b'd';

pub(crate) const DRM_IOCTL_SET_MASTER: u32 = io(DRM_TYPE, 0x1e);
pub(crate) const DRM_IOCTL_DROP_MASTER: u32 = io(DRM_TYPE, 0x1f);

// ======== capability IDs ========

pub(crate) const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
pub(crate) const DRM_CAP_CURSOR_WIDTH: u64 = 0x8;
pub(crate) const DRM_CAP_CURSOR_HEIGHT: u64 = 0x9;
pub(crate) const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
pub(crate) const DRM_CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
pub(crate) const DRM_CAP_ADDFB2_MODIFIERS: u64 = 0x10;

/// Cursor size reported via GET_CAP. The driver exposes a single primary
/// plane and no cursor commands, so this is a client-compatibility value,
/// not a hardware capability: libdrm-based clients treat 0 as "cursor
/// unsupported" and bail out during init. 64x64 matches the virtio-gpu
/// spec-level cursor dimensions.
pub(crate) const DRM_CURSOR_COMPAT_SIZE: u64 = 64;

// ======== SET_CLIENT_CAP capabilities ========

// Values are Linux's (`include/uapi/drm/drm.h`); the numbering is part of the
// uapi contract, because clients index their own state by it.
pub(crate) const DRM_CLIENT_CAP_STEREO_3D: u64 = 1;
pub(crate) const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub(crate) const DRM_CLIENT_CAP_ATOMIC: u64 = 3;

// ======== format modifiers ========

pub(crate) const DRM_MODE_FB_MODIFIERS: u32 = 0x2;

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

// ======== connector / encoder types ========

pub(crate) const DRM_MODE_CONNECTED: u32 = 1;
pub(crate) const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
pub(crate) const DRM_MODE_ENCODER_VIRTUAL: u32 = 5;

// ======== plane / property flags ========

pub(crate) const DRM_PLANE_TYPE_PRIMARY: u64 = 1;

pub(crate) const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
pub(crate) const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub(crate) const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub(crate) const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub(crate) const DRM_MODE_PROP_OBJECT: u32 = 1 << 6;
pub(crate) const DRM_MODE_PROP_ATOMIC: u32 = 0x8000_0000;

// ======== page flip / vblank / event flags ========

pub(crate) const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
pub(crate) const DRM_VBLANK_RELATIVE: u32 = 0x1;
pub(crate) const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

// ======== atomic commit flags ========

pub(crate) const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
pub(crate) const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
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

pub(crate) const PROP_CRTC_ACTIVE: u32 = 0x200;
pub(crate) const PROP_CRTC_MODE_ID: u32 = 0x201;

pub(crate) const PROP_CONN_CRTC_ID: u32 = 0x300;

pub(crate) const PLANE_PROPS: &[u32] = &[
    PROP_PLANE_TYPE,
    PROP_PLANE_FB_ID,
    PROP_PLANE_CRTC_ID,
    PROP_PLANE_SRC_X,
    PROP_PLANE_SRC_Y,
    PROP_PLANE_SRC_W,
    PROP_PLANE_SRC_H,
    PROP_PLANE_CRTC_X,
    PROP_PLANE_CRTC_Y,
    PROP_PLANE_CRTC_W,
    PROP_PLANE_CRTC_H,
    PROP_PLANE_IN_FORMATS,
];
pub(crate) const CRTC_PROPS: &[u32] = &[PROP_CRTC_ACTIVE, PROP_CRTC_MODE_ID];
pub(crate) const CONN_PROPS: &[u32] = &[PROP_CONN_CRTC_ID];

// ======== buffer / blob limits ========

pub(crate) const FIRST_DUMB_HANDLE: u32 = 1;
pub(crate) const FIRST_FB_ID: u32 = 1;
pub(crate) const DUMB_BUFFER_MAX_SIZE: usize = 8 * 1024 * 1024;
pub(crate) const DUMB_BUFFER_OFFSET_STRIDE: u64 = DUMB_BUFFER_MAX_SIZE as u64;

pub(crate) const SUPPORTED_FORMATS: &[u32] = &[DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888];

pub(crate) const MAX_EVENTS: usize = 128;
pub(crate) const FIRST_BLOB_ID: u32 = 0x1000;
pub(crate) const MAX_BLOB_BYTES: usize = 64 * 1024;

// ======== CVT-RBv1 mode timing ========

pub(crate) const CVT_RB_HFRONT_PORCH: u16 = 48;
pub(crate) const CVT_RB_HSYNC_WIDTH: u16 = 32;
pub(crate) const CVT_RB_HBACK_PORCH: u16 = 80;
pub(crate) const CVT_RB_VFRONT_PORCH: u16 = 3;
pub(crate) const CVT_RB_VSYNC_WIDTH: u16 = 8;
pub(crate) const CVT_RB_VBACK_PORCH: u16 = 6;
pub(crate) const DEFAULT_VREFRESH: u32 = 60;
