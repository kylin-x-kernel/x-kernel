// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Userspace ABI regressions for X-Kernel's DRM property-setting ioctls.
//!
//! `DRM_IOCTL_MODE_SETPROPERTY` (0xAB) and `DRM_IOCTL_MODE_OBJ_SETPROPERTY` (0xBA)
//! are the only non-atomic way to write a KMS property.  Their command words are
//! derived from the size of a userspace structure, so "the structure a client
//! sends" and "the number the driver dispatches on" are a contract that only a
//! real call can verify: a layout change moves the command word, the call falls
//! through to `EOPNOTSUPP`, and nothing fails to build.  That is how
//! `MODE_CURSOR` shipped broken earlier in this series.

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("DRM property regressions require a 64-bit Linux userspace target");

#[cfg(all(test, target_os = "linux", target_pointer_width = "64"))]
mod tests {
    use std::{
        ffi::{c_int, c_ulong},
        fs::{File, OpenOptions},
        io,
        mem::size_of,
        os::fd::AsRawFd,
        ptr,
    };

    /// `_IOWR(0xAB, struct drm_mode_connector_set_property)`, 16 bytes.
    const SETPROPERTY: c_ulong = 0xc010_64ab;
    /// `_IOWR(0xBA, struct drm_mode_obj_set_property)`, 24 bytes.
    const OBJ_SETPROPERTY: c_ulong = 0xc018_64ba;
    /// `_IOWR(0xA0, struct drm_mode_card_res)`, 64 bytes.
    const GET_RESOURCES: c_ulong = 0xc040_64a0;
    /// `_IOWR(0xB9, struct drm_mode_obj_get_properties)`, 32 bytes.
    const OBJ_GET_PROPERTIES: c_ulong = 0xc020_64b9;

    /// `DRM_MODE_OBJECT_CONNECTOR`, the only object the legacy form can name.
    const CONNECTOR: u32 = 0xc0c0_c0c0;

    const ENOENT: i32 = 2;
    const EINVAL: i32 = 22;

    /// An id and a property id no driver defines, used for the negative cases:
    /// those assertions only hold if the kernel validates instead of storing
    /// whatever it is handed.
    const UNKNOWN: u32 = 0xdead_beef;

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    #[repr(C)]
    struct ConnectorSetProperty {
        value: u64,
        prop_id: u32,
        connector_id: u32,
    }

    #[repr(C)]
    struct ObjSetProperty {
        value: u64,
        prop_id: u32,
        obj_id: u32,
        obj_type: u32,
        abi_pad: u32,
    }

    #[repr(C)]
    struct CardRes {
        fb_id_ptr: *mut u32,
        crtc_id_ptr: *mut u32,
        connector_id_ptr: *mut u32,
        encoder_id_ptr: *mut u32,
        count_fbs: u32,
        count_crtcs: u32,
        count_connectors: u32,
        count_encoders: u32,
        min_width: u32,
        max_width: u32,
        min_height: u32,
        max_height: u32,
    }

    #[repr(C)]
    struct ObjGetProperties {
        props_ptr: *mut u32,
        prop_values_ptr: *mut u64,
        count_props: u32,
        obj_id: u32,
        obj_type: u32,
        abi_pad: u32,
    }

    /// Reproduce `_IOWR(DRM_IOCTL_BASE + nr, struct)` so the constants above can be
    /// re-derived from the local `repr(C)` layout.
    fn drm_iowr(nr: c_ulong, size: c_ulong) -> c_ulong {
        const READ_WRITE: c_ulong = 3;
        (READ_WRITE << 30) | (size << 16) | (c_ulong::from(b'd') << 8) | nr
    }

    fn open_card() -> File {
        let path = std::env::var("DRM_DEVICE").unwrap_or_else(|_| "/dev/dri/card0".into());
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("cannot open isolated DRM device {path}: {error}"))
    }

    fn ioctl_arg<T>(file: &File, command: c_ulong, value: &mut T) -> io::Result<()> {
        // SAFETY: callers pass the `repr(C)` request matching `command`, and its
        // storage stays live for the duration of the call.
        let result = unsafe { ioctl(file.as_raw_fd(), command, ptr::from_mut(value)) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// The legacy form: it names the connector, not an object type.
    fn set_property(file: &File, connector_id: u32, prop_id: u32, value: u64) -> io::Result<()> {
        let mut request = ConnectorSetProperty {
            value,
            prop_id,
            connector_id,
        };
        ioctl_arg(file, SETPROPERTY, &mut request)
    }

    /// The object-typed form: id and type travel together and must agree.
    fn obj_set_property(
        file: &File,
        obj_type: u32,
        obj_id: u32,
        prop_id: u32,
        value: u64,
    ) -> io::Result<()> {
        let mut request = ObjSetProperty {
            value,
            prop_id,
            obj_id,
            obj_type,
            abi_pad: 0,
        };
        ioctl_arg(file, OBJ_SETPROPERTY, &mut request)
    }

    /// One CRTC and one connector, which is all this driver exposes.
    fn resources(file: &File) -> (u32, u32) {
        let mut crtc_id = 0u32;
        let mut connector_id = 0u32;
        let mut request = CardRes {
            fb_id_ptr: ptr::null_mut(),
            crtc_id_ptr: ptr::from_mut(&mut crtc_id),
            connector_id_ptr: ptr::from_mut(&mut connector_id),
            encoder_id_ptr: ptr::null_mut(),
            count_fbs: 0,
            count_crtcs: 1,
            count_connectors: 1,
            count_encoders: 0,
            min_width: 0,
            max_width: 0,
            min_height: 0,
            max_height: 0,
        };
        ioctl_arg(file, GET_RESOURCES, &mut request).expect("GET_RESOURCES");
        assert_eq!(request.count_crtcs, 1);
        assert_eq!(request.count_connectors, 1);
        (crtc_id, connector_id)
    }

    /// The connector's single property together with the value it currently
    /// holds, so the test can put it back and leave the device as it found it.
    fn connector_property(file: &File, connector_id: u32) -> (u32, u64) {
        let mut prop_ids = [0u32; 1];
        let mut prop_values = [0u64; 1];
        let mut request = ObjGetProperties {
            props_ptr: prop_ids.as_mut_ptr(),
            prop_values_ptr: prop_values.as_mut_ptr(),
            count_props: 1,
            obj_id: connector_id,
            obj_type: CONNECTOR,
            abi_pad: 0,
        };
        ioctl_arg(file, OBJ_GET_PROPERTIES, &mut request).expect("OBJ_GET_PROPERTIES");
        assert_eq!(
            request.count_props, 1,
            "a connector must expose exactly its CRTC_ID property"
        );
        (prop_ids[0], prop_values[0])
    }

    // One sequential case owns the device throughout, so independent opens cannot
    // interfere with one another through a parallel test runner.
    #[test]
    fn property_entry_points_are_reachable() {
        assert_eq!(size_of::<ConnectorSetProperty>(), 16);
        assert_eq!(size_of::<ObjSetProperty>(), 24);
        assert_eq!(size_of::<CardRes>(), 64);
        assert_eq!(size_of::<ObjGetProperties>(), 32);
        assert_eq!(
            drm_iowr(0xab, size_of::<ConnectorSetProperty>() as c_ulong),
            SETPROPERTY,
            "a layout change would move MODE_SETPROPERTY off the number clients send"
        );
        assert_eq!(
            drm_iowr(0xba, size_of::<ObjSetProperty>() as c_ulong),
            OBJ_SETPROPERTY,
            "a layout change would move MODE_OBJ_SETPROPERTY off the number clients send"
        );

        let file = open_card();
        let (crtc_id, connector_id) = resources(&file);
        let (prop_id, current) = connector_property(&file, connector_id);
        assert_ne!(prop_id, 0);

        // An unknown connector is not silently accepted, and the legacy form has
        // no object type to fall back on, so ENOENT is the whole answer.
        assert_eq!(
            set_property(&file, UNKNOWN, prop_id, u64::from(crtc_id))
                .unwrap_err()
                .raw_os_error(),
            Some(ENOENT)
        );
        // A known connector carrying an unknown property is EINVAL, not ENOENT:
        // Linux's drm_mode_obj_set_property_ioctl() only answers ENOENT when the
        // object lookup fails, and leaves `ret` at its -EINVAL initialiser once
        // the object is found but the property id is not.
        assert_eq!(
            set_property(&file, connector_id, UNKNOWN, 0)
                .unwrap_err()
                .raw_os_error(),
            Some(EINVAL)
        );
        // CRTC_ID is a checked object reference, so a value that is neither 0 nor
        // the live CRTC must be refused instead of stored.
        assert_eq!(
            set_property(&file, connector_id, prop_id, u64::from(UNKNOWN))
                .unwrap_err()
                .raw_os_error(),
            Some(EINVAL)
        );
        // Success path: restating the current value must be accepted.
        set_property(&file, connector_id, prop_id, current)
            .expect("restating the connector's current value succeeds");

        // The object-typed form carries the type too, and the type has to agree
        // with the id: the same property reached through the CRTC id is not the
        // connector's property.
        assert_eq!(
            obj_set_property(&file, CONNECTOR, crtc_id, prop_id, current)
                .unwrap_err()
                .raw_os_error(),
            Some(ENOENT)
        );
        assert_eq!(
            obj_set_property(&file, CONNECTOR, UNKNOWN, prop_id, current)
                .unwrap_err()
                .raw_os_error(),
            Some(ENOENT)
        );
        // Unknown property on a known connector: EINVAL, as for the legacy form.
        assert_eq!(
            obj_set_property(&file, CONNECTOR, connector_id, UNKNOWN, 0)
                .unwrap_err()
                .raw_os_error(),
            Some(EINVAL)
        );
        assert_eq!(
            obj_set_property(&file, CONNECTOR, connector_id, prop_id, u64::from(UNKNOWN))
                .unwrap_err()
                .raw_os_error(),
            Some(EINVAL)
        );
        obj_set_property(&file, CONNECTOR, connector_id, prop_id, current)
            .expect("the object form writes the same property");

        // Leave the connector on the value the test found it with.
        set_property(&file, connector_id, prop_id, current).expect("restore the connector value");
    }
}
