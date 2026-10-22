// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Userspace ABI regressions for X-Kernel's DRM handle and framebuffer teardown.
//!
//! `DRM_IOCTL_GEM_CLOSE` (0x09) and `DRM_IOCTL_MODE_CLOSEFB` (0xD0) are the two
//! teardown calls a libdrm client actually uses.  Their command words are derived
//! from the size of a userspace structure, so a layout change moves the number the
//! client sends and the call falls through to `EOPNOTSUPP` with nothing failing to
//! build -- the same hazard the `drm-set-property` regression covers for the
//! property setters.

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("DRM GEM_CLOSE regressions require a 64-bit Linux userspace target");

#[cfg(all(test, target_os = "linux", target_pointer_width = "64"))]
mod tests {
    use std::{
        ffi::{c_int, c_ulong, c_void},
        fs::{File, OpenOptions},
        io,
        mem::size_of,
        os::fd::AsRawFd,
        ptr,
    };

    /// `_IOW(0x09, struct drm_gem_close)`, 8 bytes.
    const GEM_CLOSE: c_ulong = 0x4008_6409;
    /// `_IOWR(0xD0, struct drm_mode_closefb)`, 8 bytes.
    const CLOSEFB: c_ulong = 0xc008_64d0;
    /// `_IOWR(0xB2, struct drm_mode_create_dumb)`, 32 bytes.
    const CREATE_DUMB: c_ulong = 0xc020_64b2;
    /// `_IOWR(0xB4, struct drm_mode_destroy_dumb)`, 4 bytes.
    const DESTROY_DUMB: c_ulong = 0xc004_64b4;
    /// `_IOWR(0xB8, struct drm_mode_fb_cmd2)`, 104 bytes.
    const ADDFB2: c_ulong = 0xc068_64b8;
    /// `_IOWR(0xA2, struct drm_mode_crtc)`, 104 bytes.
    const SETCRTC: c_ulong = 0xc068_64a2;
    /// `_IOWR(0xA0, struct drm_mode_card_res)`, 64 bytes.
    const GET_RESOURCES: c_ulong = 0xc040_64a0;

    const XRGB8888: u32 = 0x3432_5258;
    const UNKNOWN: u32 = 0xdead_beef;

    const ENOENT: i32 = 2;
    const EBUSY: i32 = 16;
    const EINVAL: i32 = 22;

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn mmap(
            address: *mut c_void,
            length: usize,
            protection: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn mprotect(address: *mut c_void, length: usize, protection: c_int) -> c_int;
        fn munmap(address: *mut c_void, length: usize) -> c_int;
    }

    #[repr(C)]
    struct CreateDumb {
        height: u32,
        width: u32,
        bpp: u32,
        flags: u32,
        handle: u32,
        pitch: u32,
        size: u64,
    }

    #[repr(C)]
    struct GemClose {
        handle: u32,
        pad: u32,
    }

    #[repr(C)]
    struct CloseFb {
        fb_id: u32,
        pad: u32,
    }

    #[repr(C)]
    struct FbCmd2 {
        fb_id: u32,
        width: u32,
        height: u32,
        pixel_format: u32,
        flags: u32,
        handles: [u32; 4],
        pitches: [u32; 4],
        offsets: [u32; 4],
        abi_pad: u32,
        modifiers: [u64; 4],
    }

    #[repr(C)]
    struct ModeCrtc {
        set_connectors_ptr: *mut u32,
        count_connectors: u32,
        crtc_id: u32,
        fb_id: u32,
        x: u32,
        y: u32,
        gamma_size: u32,
        mode_valid: u32,
        mode: [u8; 68],
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

    /// Reproduce `_IOW`/`_IOWR(DRM_IOCTL_BASE + nr, struct)` so the hard-coded
    /// command words above can be re-derived from the local `repr(C)` sizes.
    fn drm_ioc(direction: c_ulong, nr: c_ulong, size: c_ulong) -> c_ulong {
        (direction << 30) | (size << 16) | (c_ulong::from(b'd') << 8) | nr
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

    fn create_dumb(file: &File, side: u32) -> CreateDumb {
        let mut request = CreateDumb {
            height: side,
            width: side,
            bpp: 32,
            flags: 0,
            handle: 0,
            pitch: 0,
            size: 0,
        };
        ioctl_arg(file, CREATE_DUMB, &mut request).expect("CREATE_DUMB");
        assert_ne!(request.handle, 0);
        assert_eq!(request.pitch, side * 4);
        assert_eq!(request.size, u64::from(request.pitch) * u64::from(side));
        request
    }

    fn addfb2(file: &File, dumb: &CreateDumb) -> u32 {
        let mut request = FbCmd2 {
            fb_id: 0,
            width: dumb.width,
            height: dumb.height,
            pixel_format: XRGB8888,
            flags: 0,
            handles: [dumb.handle, 0, 0, 0],
            pitches: [dumb.pitch, 0, 0, 0],
            offsets: [0; 4],
            abi_pad: 0,
            modifiers: [0; 4],
        };
        ioctl_arg(file, ADDFB2, &mut request).expect("ADDFB2");
        assert_ne!(request.fb_id, 0);
        request.fb_id
    }

    fn setcrtc(file: &File, crtc_id: u32, connector_id: u32, fb_id: u32) -> io::Result<()> {
        let mut connectors = [connector_id];
        let mut request = ModeCrtc {
            set_connectors_ptr: connectors.as_mut_ptr(),
            count_connectors: 1,
            crtc_id,
            fb_id,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: [0; 68],
        };
        ioctl_arg(file, SETCRTC, &mut request)
    }

    fn unbind_crtc(file: &File, crtc_id: u32) -> io::Result<()> {
        let mut request = ModeCrtc {
            set_connectors_ptr: ptr::null_mut(),
            count_connectors: 0,
            crtc_id,
            fb_id: 0,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: [0; 68],
        };
        ioctl_arg(file, SETCRTC, &mut request)
    }

    fn gem_close(file: &File, handle: u32, pad: u32) -> io::Result<()> {
        let mut request = GemClose { handle, pad };
        ioctl_arg(file, GEM_CLOSE, &mut request)
    }

    fn gem_close_from_read_only_memory(file: &File, handle: u32) -> io::Result<()> {
        const PAGE_SIZE: usize = 4096;
        const PROT_READ: c_int = 1;
        const PROT_WRITE: c_int = 2;
        const MAP_PRIVATE: c_int = 2;
        const MAP_ANONYMOUS: c_int = 0x20;
        // SAFETY: mmap creates a fresh writable anonymous page; its result is
        // checked before any access, and no other test shares this mapping.
        let mapping = unsafe {
            mmap(
                ptr::null_mut(),
                PAGE_SIZE,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if mapping as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the fresh mapping is writable, page-aligned, and larger than
        // the 8-byte GemClose request. No references alias its storage.
        unsafe { ptr::write(mapping.cast::<GemClose>(), GemClose { handle, pad: 0 }) };
        // SAFETY: mapping is live and PAGE_SIZE is the length passed to mmap.
        if unsafe { mprotect(mapping, PAGE_SIZE, PROT_READ) } != 0 {
            let error = io::Error::last_os_error();
            // SAFETY: this is the same live mapping returned by mmap.
            let _ = unsafe { munmap(mapping, PAGE_SIZE) };
            return Err(error);
        }
        // SAFETY: GEM_CLOSE consumes the initialized repr(C) request from
        // this live read-only mapping, which remains allocated during ioctl.
        let result = unsafe { ioctl(file.as_raw_fd(), GEM_CLOSE, mapping) };
        let ioctl_error = (result < 0).then(io::Error::last_os_error);
        // SAFETY: this is the same live mapping returned by mmap.
        if unsafe { munmap(mapping, PAGE_SIZE) } != 0 && ioctl_error.is_none() {
            return Err(io::Error::last_os_error());
        }
        ioctl_error.map_or(Ok(()), Err)
    }

    fn close_fb(file: &File, fb_id: u32, pad: u32) -> io::Result<()> {
        let mut request = CloseFb { fb_id, pad };
        ioctl_arg(file, CLOSEFB, &mut request)
    }

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
        (crtc_id, connector_id)
    }

    // One sequential case owns the device throughout, so independent opens cannot
    // interfere with one another through a parallel test runner.
    #[test]
    fn gem_close_and_closefb() {
        assert_eq!(size_of::<GemClose>(), 8);
        assert_eq!(size_of::<CloseFb>(), 8);
        assert_eq!(size_of::<CreateDumb>(), 32);
        assert_eq!(size_of::<FbCmd2>(), 104);
        assert_eq!(size_of::<ModeCrtc>(), 104);
        assert_eq!(size_of::<CardRes>(), 64);
        assert_eq!(drm_ioc(1, 0x09, size_of::<GemClose>() as c_ulong), GEM_CLOSE);
        assert_eq!(drm_ioc(3, 0xD0, size_of::<CloseFb>() as c_ulong), CLOSEFB);
        assert_eq!(drm_ioc(3, 0xB2, size_of::<CreateDumb>() as c_ulong), CREATE_DUMB);
        assert_eq!(drm_ioc(3, 0xB8, size_of::<FbCmd2>() as c_ulong), ADDFB2);
        assert_eq!(drm_ioc(3, 0xA2, size_of::<ModeCrtc>() as c_ulong), SETCRTC);

        let file = open_card();
        let (crtc_id, connector_id) = resources(&file);

        // GEM_CLOSE: the pad field is validated before the handle is looked at,
        // and an unknown handle is EINVAL (drm_gem_handle_delete), not ENOENT.
        assert_eq!(gem_close(&file, UNKNOWN, 0).unwrap_err().raw_os_error(), Some(EINVAL));
        assert_eq!(gem_close(&file, UNKNOWN, 1).unwrap_err().raw_os_error(), Some(EINVAL));

        // `pad` really is checked before the handle: a live handle is refused
        // with a non-zero pad and accepted once the pad is cleared, which an
        // implementation that looked the handle up first could not tell apart.
        let pad_dumb = create_dumb(&file, 16);
        assert_eq!(
            gem_close(&file, pad_dumb.handle, 1).unwrap_err().raw_os_error(),
            Some(EINVAL)
        );
        gem_close_from_read_only_memory(&file, pad_dumb.handle)
            .expect("live handle closes from a read-only request with pad cleared");
        assert_eq!(
            gem_close_from_read_only_memory(&file, pad_dumb.handle)
                .unwrap_err()
                .raw_os_error(),
            Some(EINVAL),
            "the read-only request closes the handle exactly once"
        );

        let dumb = create_dumb(&file, 64);
        let fb_id = addfb2(&file, &dumb);

        // CLOSEFB: pad first, then the lookup, then the in-use check.
        assert_eq!(close_fb(&file, fb_id, 1).unwrap_err().raw_os_error(), Some(EINVAL));
        assert_eq!(close_fb(&file, UNKNOWN, 0).unwrap_err().raw_os_error(), Some(ENOENT));

        // Bound to the legacy CRTC, the framebuffer is refused instead of being
        // pulled out from under the scanout.
        setcrtc(&file, crtc_id, connector_id, fb_id).expect("SETCRTC binds the framebuffer");
        assert_eq!(close_fb(&file, fb_id, 0).unwrap_err().raw_os_error(), Some(EBUSY));

        unbind_crtc(&file, crtc_id).expect("SETCRTC clears the legacy CRTC");
        close_fb(&file, fb_id, 0).expect("unbound framebuffer closes");
        assert_eq!(close_fb(&file, fb_id, 0).unwrap_err().raw_os_error(), Some(ENOENT));

        // GEM_CLOSE releases the dumb buffer; the second close reports the miss.
        gem_close(&file, dumb.handle, 0).expect("dumb buffer closes");
        assert_eq!(gem_close(&file, dumb.handle, 0).unwrap_err().raw_os_error(), Some(EINVAL));

        // Closing the handle must not pull the scanout out from under a
        // framebuffer that still names the resource: the framebuffer keeps
        // working until it is closed too.  Closing it then only *starts* the
        // retirement -- the parked buffer is released once the host switches to
        // another scanout target, which the driver does on the next present.
        let parked = create_dumb(&file, 32);
        let parked_fb = addfb2(&file, &parked);
        gem_close(&file, parked.handle, 0).expect("handle closes while its fb is registered");
        setcrtc(&file, crtc_id, connector_id, parked_fb)
            .expect("the framebuffer still works after its handle closed");
        assert_eq!(
            close_fb(&file, parked_fb, 0).unwrap_err().raw_os_error(),
            Some(EBUSY)
        );
        unbind_crtc(&file, crtc_id).expect("SETCRTC clears the legacy CRTC");
        close_fb(&file, parked_fb, 0).expect("last reference enters retirement");

        // The legacy spelling keeps its silent-success behaviour.
        let mut unknown = DumbHandle { handle: UNKNOWN };
        ioctl_arg(&file, DESTROY_DUMB, &mut unknown).expect("DESTROY_DUMB on an unknown handle");
    }

    #[repr(C)]
    struct DumbHandle {
        handle: u32,
    }
}
