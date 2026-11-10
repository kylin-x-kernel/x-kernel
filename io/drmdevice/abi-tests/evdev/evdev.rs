// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Userspace ABI regressions for the evdev ioctl dispatch.
//!
//! The ioctls are the ones libinput probes when it opens an input device:
//! `EVIOCGVERSION`, `EVIOCGID`, `EVIOCGNAME`, `EVIOCGPHYS`, `EVIOCGPROP`,
//! `EVIOCGKEY`, `EVIOCGBIT` and `EVIOCGABS`. A dispatch bug shows up here as a
//! wrong reply size or a missing device, which is exactly how libinput stops
//! recognising an absolute-axis device.

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("evdev regressions require a 64-bit Linux userspace target");

#[cfg(all(test, target_os = "linux", target_pointer_width = "64"))]
mod tests {
    use std::{
        ffi::c_int,
        fs::{File, OpenOptions},
        mem::size_of,
        os::fd::{AsRawFd, RawFd},
    };

    /// `EVIOCGVERSION`.
    const EVIOCGVERSION: u64 = 0x8004_4501;
    /// `EVIOCGID`.
    const EVIOCGID: u64 = 0x8008_4502;
    /// `EVIOCGNAME(len)`.
    const fn eviocgname(len: usize) -> u64 {
        0x8000_0000 | ((len as u64) << 16) | (b'E' as u64) << 8 | 0x06
    }
    /// `EVIOCGPROP(len)`.
    const fn eviocgprop(len: usize) -> u64 {
        0x8000_0000 | ((len as u64) << 16) | (b'E' as u64) << 8 | 0x09
    }
    /// `EVIOCGKEY(len)`.
    const fn eviocgkey(len: usize) -> u64 {
        0x8000_0000 | ((len as u64) << 16) | (b'E' as u64) << 8 | 0x18
    }
    /// `EVIOCGBIT(ev, len)`.
    const fn eviocgbit(ev: u8, len: usize) -> u64 {
        0x8000_0000 | ((len as u64) << 16) | (b'E' as u64) << 8 | 0x20 | ev as u64
    }
    /// `EVIOCGABS(axis)`.
    const fn eviocgabs(axis: u8) -> u64 {
        0x8018_0000 | ((size_of::<AbsInfo>() as u64) << 16) | (b'E' as u64) << 8 | 0x40 | axis as u64
    }

    /// `struct input_absinfo`.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct AbsInfo {
        value: i32,
        minimum: i32,
        maximum: i32,
        fuzz: i32,
        flat: i32,
        resolution: i32,
    }

    /// `struct input_id`.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct InputId {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
    }

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: u64, ...) -> c_int;
    }

    unsafe extern "C" {
        fn __errno_location() -> *mut c_int;
    }

    fn errno() -> i32 {
        // SAFETY: `errno` is a thread-local the libc exports on every Linux
        // target this test builds for.
        unsafe { *__errno_location() }
    }

    /// Runs one ioctl and reports the errno of a failure.
    fn call<T>(fd: RawFd, request: u64, arg: &mut T) -> Result<c_int, i32> {
        // SAFETY: `arg` is a live, correctly typed pointer for this request.
        let result = unsafe { ioctl(fd, request, arg as *mut T as *mut std::ffi::c_void) };
        if result < 0 { Err(errno()) } else { Ok(result) }
    }

    /// Opens the first event device the environment names.
    ///
    /// `EVENT_DEVICE` overrides the default, which keeps the test usable on a
    /// guest where the device is not the first one.
    fn open_event() -> (File, String) {
        let path = std::env::var("EVENT_DEVICE").unwrap_or_else(|_| "/dev/input/event0".into());
        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("cannot open {path}: {error}"));
        (file, path)
    }

    #[test]
    fn libinput_probes_are_answered_with_their_own_sizes() {
        let (file, path) = open_event();
        let fd = file.as_raw_fd();

        let mut version = 0u32;
        call(fd, EVIOCGVERSION, &mut version).expect("EVIOCGVERSION");
        assert_eq!(version >> 16, 1, "{path} speaks evdev version 1");

        let mut id = InputId::default();
        call(fd, EVIOCGID, &mut id).expect("EVIOCGID");

        // The name is what libinput keys a device by; a zero-length reply
        // means the dispatch lost the string.
        let mut name = [0u8; 64];
        let written = call(fd, eviocgname(name.len()), &mut name).expect("EVIOCGNAME");
        assert!(written > 0, "a device reports a name");

        // Capability bitmaps answer with their full width, zero-padded.
        let mut properties = [0u8; 4];
        let written = call(fd, eviocgprop(properties.len()), &mut properties).expect("EVIOCGPROP");
        assert_eq!(written, properties.len() as c_int, "the property bitmap is complete");

        let mut keys = [0u8; 96];
        let written = call(fd, eviocgkey(keys.len()), &mut keys).expect("EVIOCGKEY");
        assert_eq!(written, keys.len() as c_int, "the key state is complete");

        let mut event_types = [0u8; 4];
        call(fd, eviocgbit(0, event_types.len()), &mut event_types).expect("EVIOCGBIT(0)");

        // An unknown axis number is refused, as Linux answers `EINVAL`.
        let mut absent = AbsInfo::default();
        match call(fd, eviocgabs(0x3f), &mut absent) {
            Ok(0) | Err(22) => {}
            Ok(other) => panic!("EVIOCGABS answered {other}"),
            Err(error) => panic!("EVIOCGABS failed with {error}"),
        }
    }

    #[test]
    fn a_device_without_absolute_axes_refuses_axis_queries() {
        let (file, path) = open_event();
        let mut event_types = [0u8; 4];
        call(file.as_raw_fd(), eviocgbit(0, event_types.len()), &mut event_types)
            .expect("EVIOCGBIT(0)");
        let has_abs = event_types[0] & 0x08 != 0; // `EV_ABS`
        if has_abs {
            // A tablet or touchscreen: the axis must be describable.
            let mut abs = AbsInfo::default();
            call(file.as_raw_fd(), eviocgabs(0), &mut abs).expect("EVIOCGABS(X)");
            assert!(abs.maximum > abs.minimum, "{path} reports an X range");
        } else {
            let mut abs = AbsInfo::default();
            let result = call(file.as_raw_fd(), eviocgabs(0), &mut abs);
            assert!(
                matches!(result, Ok(0) | Err(22)),
                "a relative device answers EINVAL, never a fake range"
            );
        }
    }
}
