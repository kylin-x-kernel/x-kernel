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

pub const DRIVER_NAME: &str = "simpledrm";
pub const DRIVER_DATE: &str = "2026-06-02";
pub const DRIVER_DESC: &str = "X-Kernel simple DRM driver";
pub const DRIVER_VERSION_MAJOR: i32 = 1;
pub const DRIVER_VERSION_MINOR: i32 = 0;
pub const DRIVER_VERSION_PATCHLEVEL: i32 = 0;

// ======== ioctl numbers ========

pub const DRM_TYPE: u8 = b'd';

pub const DRM_IOCTL_SET_MASTER: u32 = io(DRM_TYPE, 0x1e);
pub const DRM_IOCTL_DROP_MASTER: u32 = io(DRM_TYPE, 0x1f);

// ======== capability IDs ========

pub const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
pub const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
pub const DRM_CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
pub const DRM_CAP_ADDFB2_MODIFIERS: u64 = 0x10;

// ======== format modifiers ========

pub const DRM_MODE_FB_MODIFIERS: u32 = 0x2;
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

// ======== format FourCCs ========

pub const DRM_FORMAT_XRGB8888: u32 =
    (b'X' as u32) | ((b'R' as u32) << 8) | ((b'2' as u32) << 16) | ((b'4' as u32) << 24);
pub const DRM_FORMAT_ARGB8888: u32 =
    (b'A' as u32) | ((b'R' as u32) << 8) | ((b'2' as u32) << 16) | ((b'4' as u32) << 24);

// ======== object type tags ========

pub const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
pub const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;

// ======== connector / encoder types ========

pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
pub const DRM_MODE_ENCODER_VIRTUAL: u32 = 5;

// ======== plane / property flags ========

pub const DRM_PLANE_TYPE_PRIMARY: u64 = 1;

pub const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
pub const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub const DRM_MODE_PROP_OBJECT: u32 = 1 << 6;
pub const DRM_MODE_PROP_ATOMIC: u32 = 0x8000_0000;

// ======== page flip / vblank / event flags ========

pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
pub const DRM_VBLANK_RELATIVE: u32 = 0x1;
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

// ======== atomic commit flags ========

pub const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
pub const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
pub const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;

// ======== fixed object IDs ========

pub const CRTC_ID: u32 = 0x10;
pub const ENCODER_ID: u32 = 0x20;
pub const CONNECTOR_ID: u32 = 0x30;
pub const PLANE_ID: u32 = 0x40;

// ======== property IDs ========
// Layout: 0x1xx = plane, 0x2xx = CRTC, 0x3xx = connector.

pub const PROP_PLANE_TYPE: u32 = 0x100;
pub const PROP_PLANE_FB_ID: u32 = 0x101;
pub const PROP_PLANE_CRTC_ID: u32 = 0x102;
pub const PROP_PLANE_SRC_X: u32 = 0x103;
pub const PROP_PLANE_SRC_Y: u32 = 0x104;
pub const PROP_PLANE_SRC_W: u32 = 0x105;
pub const PROP_PLANE_SRC_H: u32 = 0x106;
pub const PROP_PLANE_CRTC_X: u32 = 0x107;
pub const PROP_PLANE_CRTC_Y: u32 = 0x108;
pub const PROP_PLANE_CRTC_W: u32 = 0x109;
pub const PROP_PLANE_CRTC_H: u32 = 0x10A;
pub const PROP_PLANE_IN_FORMATS: u32 = 0x10B;

pub const PROP_CRTC_ACTIVE: u32 = 0x200;
pub const PROP_CRTC_MODE_ID: u32 = 0x201;

pub const PROP_CONN_CRTC_ID: u32 = 0x300;

pub const PLANE_PROPS: &[u32] = &[
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
pub const CRTC_PROPS: &[u32] = &[PROP_CRTC_ACTIVE, PROP_CRTC_MODE_ID];
pub const CONN_PROPS: &[u32] = &[PROP_CONN_CRTC_ID];

// ======== buffer / blob limits ========

pub const FIRST_DUMB_HANDLE: u32 = 1;
pub const FIRST_FB_ID: u32 = 1;
pub const DUMB_BUFFER_MAX_SIZE: usize = 8 * 1024 * 1024;
pub const DUMB_BUFFER_OFFSET_STRIDE: u64 = DUMB_BUFFER_MAX_SIZE as u64;

pub const SUPPORTED_FORMATS: &[u32] = &[DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888];

pub const MAX_EVENTS: usize = 128;
pub const FIRST_BLOB_ID: u32 = 0x1000;
pub const MAX_BLOB_BYTES: usize = 64 * 1024;

// ======== CVT-RBv1 mode timing ========

pub const CVT_RB_HFRONT_PORCH: u16 = 48;
pub const CVT_RB_HSYNC_WIDTH: u16 = 32;
pub const CVT_RB_HBACK_PORCH: u16 = 80;
pub const CVT_RB_VFRONT_PORCH: u16 = 3;
pub const CVT_RB_VSYNC_WIDTH: u16 = 8;
pub const CVT_RB_VBACK_PORCH: u16 = 6;
pub const DEFAULT_VREFRESH: u32 = 60;
