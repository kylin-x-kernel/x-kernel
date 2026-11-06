// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The legacy cursor ioctls (`drm_crtc.c`).

use crate::prelude::*;

/// `DRM_IOCTL_MODE_CURSOR`.
impl Ioctl for DrmModeCursor {
    const NAME: &'static str = "MODE_CURSOR";

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

/// `DRM_IOCTL_MODE_CURSOR2`.
impl Ioctl for DrmModeCursor2 {
    const NAME: &'static str = "MODE_CURSOR2";

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
