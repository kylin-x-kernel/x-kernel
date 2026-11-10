// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Card identity and client negotiation: `VERSION`, `SET_VERSION`, `GET_UNIQUE`,
//! `GET_CAP` and `SET_CLIENT_CAP`.
//!
//! These are the only commands that describe the driver itself rather than an
//! object, so they sit next to the device model instead of with a subsystem.

use alloc::{string::String, sync::Arc, vec::Vec};

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

    fn handle(
        dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        version: &mut Self,
    ) -> DrmResult<usize> {
        version.version_major = DRIVER_VERSION_MAJOR;
        version.version_minor = DRIVER_VERSION_MINOR;
        version.version_patchlevel = DRIVER_VERSION_PATCHLEVEL;
        let info = dev.driver_info();
        version.name_len = copy_version_field(version.name, version.name_len, info.name)?;
        version.date_len = copy_version_field(version.date, version.date_len, info.date)?;
        version.desc_len = copy_version_field(version.desc, version.desc_len, info.desc)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_SET_VERSION`.
impl Ioctl for DrmSetVersion {
    const NAME: &'static str = "SET_VERSION";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        set_version: &mut Self,
    ) -> DrmResult<usize> {
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
                *binding.master.unique.lock() = Some(driver_unique_name(dev.driver_info().name));
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

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        unique: &mut Self,
    ) -> DrmResult<usize> {
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
/// Pure so the table can be checked without a card. A capability is reported
/// only when a working call stands behind it: `DRM_CAP_PRIME` is set because
/// `PRIME_HANDLE_TO_FD` returns a dma-buf descriptor and `PRIME_FD_TO_HANDLE`
/// imports one, and the syncobj capabilities are set because the whole
/// `SYNCOBJ_*` family is in the ioctl table. The virgl/execbuf capabilities
/// have no entry at all.
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
        // Linux reports `mode_config.cursor_width/height`, which this card
        // never sets: there is no cursor plane, so the honest answer is 0 and
        // a client falls back to its software cursor. A non-zero value would
        // tell the client a hardware cursor exists.
        DRM_CAP_CURSOR_WIDTH | DRM_CAP_CURSOR_HEIGHT => 0,
        DRM_CAP_TIMESTAMP_MONOTONIC => 1,
        // Linux answers 1 unconditionally: the kernel always fills the CRTC
        // id into vblank events (`drm_crtc_send_vblank_event`), and clients
        // such as wlroots treat 0 as "cannot use the DRM backend at all".
        // The timeline itself may be hardware- or timer-driven; the event
        // layout is the same.
        DRM_CAP_CRTC_IN_VBLANK_EVENT => 1,
        DRM_CAP_ADDFB2_MODIFIERS => 1,
        // Import and export are one implementation: a dma-buf descriptor this
        // card exports is imported through the same reservation, so both bits
        // are true together.
        DRM_CAP_PRIME => DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT,
        DRM_CAP_SYNCOBJ => 1,
        DRM_CAP_SYNCOBJ_TIMELINE => 1,
        // Known capabilities this card does not implement answer 0, as Linux
        // does, so a probing client can tell "absent" from "unknown".
        DRM_CAP_VBLANK_HIGH_CRTC => 1,
        DRM_CAP_DUMB_PREFERRED_DEPTH => 24,
        DRM_CAP_ASYNC_PAGE_FLIP => 0,
        DRM_CAP_PAGE_FLIP_TARGET => 0,
        DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP => 0,
        // `drm_getcap` answers `EINVAL` for a capability this kernel does not
        // know, so a client can tell "0" from "not implemented".
        _ => {
            log::debug!("drm: GET_CAP unknown capability {capability:#x}");
            u64::MAX
        }
    }
}

/// `DRM_IOCTL_GET_CAP`.
impl Ioctl for DrmGetCap {
    const NAME: &'static str = "GET_CAP";

    fn handle(
        dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        cap: &mut Self,
    ) -> DrmResult<usize> {
        let value = capability(cap.capability, dev.caps());
        if value == u64::MAX {
            return Err(DrmError::InvalidInput);
        }
        cap.value = value;
        Ok(0)
    }
}

/// `DRM_IOCTL_SET_CLIENT_CAP`.
impl Ioctl for DrmSetClientCap {
    const NAME: &'static str = "SET_CLIENT_CAP";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        cap: &mut Self,
    ) -> DrmResult<usize> {
        // Mirror Linux `drm_setclientcap()`: an unknown capability is
        // EOPNOTSUPP, an out-of-range boolean is EINVAL, and the granted value
        // is remembered per file.
        //
        // `STEREO_3D` and `UNIVERSAL_PLANES` are accepted even though nothing
        // here implements stereoscopy or gated plane enumeration: both are
        // client-side flags in Linux, and a driver that answered EOPNOTSUPP
        // would make a probing client give up. Only `ATOMIC` changes what this
        // driver acts on.
        //
        // `ASPECT_RATIO`, `WRITEBACK_CONNECTORS` and `CURSOR_PLANE_HOTSPOT` are
        // client-side flags as well: nothing in this driver depends on them, and
        // a client that sets them must be able to keep going.
        match cap.capability {
            DRM_CLIENT_CAP_STEREO_3D
            | DRM_CLIENT_CAP_UNIVERSAL_PLANES
            | DRM_CLIENT_CAP_ATOMIC
            | DRM_CLIENT_CAP_ASPECT_RATIO
            | DRM_CLIENT_CAP_WRITEBACK_CONNECTORS
            | DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT => {}
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

/// The longest name `SET_CLIENT_NAME` accepts (`DRM_CLIENT_NAME_MAX_LEN`).
const CLIENT_NAME_MAX_LEN: usize = 64;

/// `DRM_IOCTL_GET_CLIENT`: the hollowed-out client enumeration.
///
/// Linux answers index 0 with the calling client and stops the enumeration
/// with `EINVAL`; the caller's authentication is the one bit anything still
/// reads this command for.
impl Ioctl for DrmClient {
    const NAME: &'static str = "GET_CLIENT";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        client: &mut Self,
    ) -> DrmResult<usize> {
        if client.idx != 0 {
            return Err(DrmError::InvalidInput);
        }
        client.auth = core::ffi::c_int::from(file.is_authenticated());
        client.pid = u64::from(kprocess::current_user_thread().pid());
        client.uid = 0;
        client.magic = 0;
        client.iocs = 0;
        Ok(0)
    }
}

/// `DRM_IOCTL_GET_STATS`: the legacy counters, cleared.
///
/// `drm_getstats` zeroes the request so userspace does not eat stack garbage;
/// nothing counts the legacy statistics any more.
impl Ioctl for DrmStats {
    const NAME: &'static str = "GET_STATS";

    fn handle(
        _dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        _stats: &mut Self,
    ) -> DrmResult<usize> {
        Ok(0)
    }
}

/// `DRM_IOCTL_SET_CLIENT_NAME`: attaches a debug name to the calling file.
///
/// The name is length-bounded and must be printable without whitespace, which
/// is what `drm_validate_value_string` enforces.
impl Ioctl for DrmSetClientName {
    const NAME: &'static str = "SET_CLIENT_NAME";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        let len = usize::try_from(request.name_len).map_err(|_| DrmError::InvalidInput)?;
        if len > CLIENT_NAME_MAX_LEN {
            return Err(DrmError::InvalidInput);
        }
        let mut name = Vec::new();
        if len != 0 {
            name = request
                .name
                .load_vm_vec(len)
                .map_err(|_| DrmError::BadAddress)?;
            if name.iter().any(|byte| !byte.is_ascii_graphic()) {
                return Err(DrmError::InvalidInput);
            }
        }
        *file.client_name.lock() = String::from_utf8_lossy(&name).into_owned();
        Ok(0)
    }
}
