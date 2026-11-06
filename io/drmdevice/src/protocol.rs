// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Card identity and client negotiation: `VERSION`, `SET_VERSION`, `GET_UNIQUE`,
//! `GET_CAP` and `SET_CLIENT_CAP`.
//!
//! These are the only commands that describe the driver itself rather than an
//! object, so they sit next to the device model instead of with a subsystem.

use alloc::sync::Arc;

use crate::prelude::*;

/// Copies a VERSION string field, reporting the full length.
///
/// A length-only or truncated query still reports the complete length, and a
/// null destination with zero capacity is a pure query.
fn copy_version_field(user_ptr: UserPtr<u8>, capacity: usize, value: &str) -> DrmResult<usize> {
    let copy_len = capacity.min(value.len());
    if copy_len != 0 && !user_ptr.is_null() {
        user_ptr
            .write_vm_slice(&value.as_bytes()[..copy_len])
            .map_err(|_| DrmError::BadAddress)?;
    }
    Ok(value.len())
}

/// `DRM_IOCTL_VERSION`.
impl Ioctl for DrmVersion {
    const NAME: &'static str = "VERSION";

    fn handle(_dev: &DrmDevice, _file: &Arc<DrmFile>, version: &mut Self) -> DrmResult<usize> {
        version.version_major = DRIVER_VERSION_MAJOR;
        version.version_minor = DRIVER_VERSION_MINOR;
        version.version_patchlevel = DRIVER_VERSION_PATCHLEVEL;
        version.name_len = copy_version_field(version.name, version.name_len, DRIVER_NAME)?;
        version.date_len = copy_version_field(version.date, version.date_len, DRIVER_DATE)?;
        version.desc_len = copy_version_field(version.desc, version.desc_len, DRIVER_DESC)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_SET_VERSION`.
impl Ioctl for DrmSetVersion {
    const NAME: &'static str = "SET_VERSION";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, set_version: &mut Self) -> DrmResult<usize> {
        let requested = *set_version;
        // Linux reports the supported versions even for lower-version requests
        // and validation errors, not the input pair.
        set_version.drm_di_major = 1;
        set_version.drm_di_minor = 4;
        set_version.drm_dd_major = DRIVER_VERSION_MAJOR;
        set_version.drm_dd_minor = DRIVER_VERSION_MINOR;

        let masters = dev.master.lock();
        if !masters
            .active
            .as_ref()
            .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(file)))
        {
            return Err(DrmError::PermissionDenied);
        }
        let binding = file.auth.lock();
        // Interface major -1 is a query; its minor is ignored.
        if requested.drm_di_major != -1 {
            if requested.drm_di_major != 1 || !(0..=4).contains(&requested.drm_di_minor) {
                return Err(DrmError::InvalidInput);
            }
            if requested.drm_di_minor >= 1 {
                *binding.master.unique.lock() = Some(driver_unique_name());
            }
        }
        // The identity is established before the driver version is validated,
        // matching Linux's ordering; the error still reports the versions above.
        if requested.drm_dd_major != -1
            && (requested.drm_dd_major != DRIVER_VERSION_MAJOR
                || requested.drm_dd_minor < 0
                || requested.drm_dd_minor > DRIVER_VERSION_MINOR)
        {
            return Err(DrmError::InvalidInput);
        }
        Ok(0)
    }
}

/// `DRM_IOCTL_GET_UNIQUE`.
impl Ioctl for DrmUnique {
    const NAME: &'static str = "GET_UNIQUE";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, unique: &mut Self) -> DrmResult<usize> {
        let identity = {
            let _masters = dev.master.lock();
            let binding = file.auth.lock();
            binding.master.unique.lock().clone()
        };
        let identity = identity.as_deref().unwrap_or("");
        // GET_UNIQUE copies only when the complete identity fits.
        if !identity.is_empty() && unique.unique_len >= identity.len() {
            unique
                .unique
                .write_vm_slice(identity.as_bytes())
                .map_err(|_| DrmError::BadAddress)?;
        }
        unique.unique_len = identity.len();
        Ok(0)
    }
}

/// Value `GET_CAP` reports for one capability.
///
/// Pure so the table can be checked without a card. Capabilities without a
/// working call behind them stay absent: there is no entry for `DRM_CAP_PRIME`
/// (PRIME is a handle round trip) or for the virgl/execbuf capabilities.
pub(crate) fn capability(capability: u64, caps: BackendCaps) -> u64 {
    match capability {
        DRM_CAP_DUMB_BUFFER => u64::from(caps.dumb),
        // The scanout path is shadow-buffered: the host resource is a copy of
        // guest memory rather than a direct scanout, which is exactly what this
        // bit tells the X server. Reporting 0 makes Xorg's modesetting driver
        // disable ShadowFB, stop sending DIRTYFB, and leave the session stale.
        // Linux's virtio-gpu sets `mode_config.prefer_shadow = 1` for the same
        // reason.
        DRM_CAP_DUMB_PREFER_SHADOW => 1,
        // The driver exposes a single primary plane and no cursor commands, so
        // these are client-compatibility values: libdrm-based clients treat 0 as
        // "cursor unsupported" and bail out during init.
        DRM_CAP_CURSOR_WIDTH | DRM_CAP_CURSOR_HEIGHT => DRM_CURSOR_COMPAT_SIZE,
        DRM_CAP_TIMESTAMP_MONOTONIC => 1,
        DRM_CAP_CRTC_IN_VBLANK_EVENT => 1,
        DRM_CAP_ADDFB2_MODIFIERS => 1,
        _ => 0,
    }
}

/// `DRM_IOCTL_GET_CAP`.
impl Ioctl for DrmGetCap {
    const NAME: &'static str = "GET_CAP";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, cap: &mut Self) -> DrmResult<usize> {
        cap.value = capability(cap.capability, dev.caps());
        Ok(0)
    }
}

/// `DRM_IOCTL_SET_CLIENT_CAP`.
impl Ioctl for DrmSetClientCap {
    const NAME: &'static str = "SET_CLIENT_CAP";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, cap: &mut Self) -> DrmResult<usize> {
        // Mirror Linux `drm_setclientcap()`: an unknown capability is
        // EOPNOTSUPP, an out-of-range boolean is EINVAL, and the granted value
        // is remembered per file.
        //
        // `STEREO_3D` and `UNIVERSAL_PLANES` are accepted even though nothing
        // here implements stereoscopy or gated plane enumeration: both are
        // client-side flags in Linux, and a driver that answered EOPNOTSUPP
        // would make a probing client give up. Only `ATOMIC` changes what this
        // driver acts on.
        match cap.capability {
            DRM_CLIENT_CAP_STEREO_3D | DRM_CLIENT_CAP_UNIVERSAL_PLANES | DRM_CLIENT_CAP_ATOMIC => {}
            _ => return Err(DrmError::OpNotSupported),
        }
        if cap.value > 1 {
            return Err(DrmError::InvalidInput);
        }
        if cap.value == 1 {
            file.grant_cap(cap.capability);
        } else {
            file.revoke_cap(cap.capability);
        }
        Ok(0)
    }
}
