// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Userspace ABI regressions for the DRM render node, framebuffer dirty flush
//! and reference-counted dumb-buffer release.

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("DRM render-node regressions require a 64-bit Linux userspace target");

#[cfg(all(test, target_os = "linux", target_pointer_width = "64"))]
mod tests {
    use std::{
        ffi::{c_int, c_ulong, c_void},
        fs::OpenOptions,
        io,
        mem::size_of,
        os::fd::AsRawFd,
    };

    const EACCES: i32 = 13;
    const EINVAL: i32 = 22;
    const ENOENT: i32 = 2;

    const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
    const DRM_CAP_DUMB_PREFER_SHADOW: u64 = 0x4;
    const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
    const DRM_RENDER_MINOR_BASE: u32 = 128;
    /// The card's single CRTC (`CRTC_ID` in the driver).
    const CRTC_ID: u32 = 0x10;
    /// The card's single connector.
    const CONNECTOR_ID: u32 = 0x30;

    /// `_IO('d', nr)`: a command with no request structure.
    const fn io_nr(nr: u32) -> c_ulong {
        (('d' as c_ulong) << 8) | nr as c_ulong
    }

    const fn ioc(direction: c_ulong, nr: u32, size: usize) -> c_ulong {
        (direction << 30) | ((size as c_ulong) << 16) | (('d' as c_ulong) << 8) | nr as c_ulong
    }

    /// `_IOWR('d', nr, T)`: the command number is derived from the request size,
    /// so a layout mistake changes the number instead of silently reinterpreting
    /// the buffer.
    const fn iowr<T>(nr: u32) -> c_ulong {
        ioc(3, nr, size_of::<T>())
    }

    const fn iow<T>(nr: u32) -> c_ulong {
        ioc(1, nr, size_of::<T>())
    }

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    #[repr(C)]
    #[derive(Default)]
    struct GetCap {
        capability: u64,
        value: u64,
    }

    #[repr(C)]
    #[derive(Default)]
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
    #[derive(Default)]
    struct DestroyDumb {
        handle: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct GemClose {
        handle: u32,
        pad: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct FbCmd2 {
        fb_id: u32,
        width: u32,
        height: u32,
        pixel_format: u32,
        flags: u32,
        handles: [u32; 4],
        pitches: [u32; 4],
        offsets: [u32; 4],
        modifier: [u64; 4],
    }

    #[repr(C)]
    #[derive(Default)]
    struct RmFb {
        fb_id: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct DirtyFb {
        fb_id: u32,
        flags: u32,
        color: u32,
        num_clips: u32,
        clips_ptr: *mut c_void,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SetCrtc {
        set_connectors_ptr: u64,
        count_connectors: u32,
        crtc_id: u32,
        fb_id: u32,
        x: u32,
        y: u32,
        gamma_size: u32,
        mode_valid: u32,
        mode: ModeInfo,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ModeInfo {
        clock: u32,
        hdisplay: u16,
        hsync_start: u16,
        hsync_end: u16,
        htotal: u16,
        hskew: u16,
        vdisplay: u16,
        vsync_start: u16,
        vsync_end: u16,
        vtotal: u16,
        vscan: u16,
        vrefresh: u32,
        flags: u32,
        kind: u32,
        name: [u8; 32],
    }

    #[repr(C)]
    #[derive(Default)]
    struct CrtcPageFlip {
        crtc_id: u32,
        fb_id: u32,
        flags: u32,
        reserved: u32,
        user_data: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct PrimeHandle {
        handle: u32,
        flags: u32,
        fd: i32,
    }

    const SET_MASTER: c_ulong = io_nr(0x1e);
    const GET_CAP: c_ulong = iowr::<GetCap>(0x0c);
    const GEM_CLOSE: c_ulong = iow::<GemClose>(0x09);
    const PRIME_HANDLE_TO_FD: c_ulong = iowr::<PrimeHandle>(0x2d);
    const FB_CMD2: c_ulong = iowr::<FbCmd2>(0xB8);
    const RMFB: c_ulong = iowr::<RmFb>(0xAF);
    const DIRTYFB: c_ulong = iowr::<DirtyFb>(0xB1);
    const PAGE_FLIP: c_ulong = iowr::<CrtcPageFlip>(0xB0);
    const SETCRTC: c_ulong = iowr::<SetCrtc>(0xA2);
    const CREATE_DUMB: c_ulong = iowr::<CreateDumb>(0xB2);
    const DESTROY_DUMB: c_ulong = iowr::<DestroyDumb>(0xB4);

    fn ioctl_raw(fd: c_int, request: c_ulong, arg: *mut c_void) -> io::Result<c_int> {
        // SAFETY: `arg` points at one of the `#[repr(C)]` request structures
        // above for the duration of the call, and the request number was derived
        // from that structure's size.
        let result = unsafe { ioctl(fd, request, arg) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    }

    /// Issues a command that takes no request structure and returns its errno.
    fn ioctl_no_arg(fd: c_int, request: c_ulong) -> i32 {
        match ioctl_raw(fd, request, core::ptr::null_mut()) {
            Ok(_) => 0,
            Err(error) => error.raw_os_error().unwrap_or(0),
        }
    }

    fn errno_of<T>(fd: c_int, request: c_ulong, arg: &mut T) -> i32 {
        match ioctl_raw(fd, request, arg as *mut T as *mut c_void) {
            Ok(_) => 0,
            Err(error) => error.raw_os_error().unwrap_or(0),
        }
    }

    fn device_path() -> String {
        std::env::var("DRM_DEVICE").unwrap_or_else(|_| String::from("/dev/dri/card0"))
    }

    fn render_path() -> String {
        std::env::var("DRM_RENDER_NODE").unwrap_or_else(|_| String::from("/dev/dri/renderD128"))
    }

    fn open_node(path: &str) -> std::fs::File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|error| panic!("cannot open {path}: {error}"))
    }

    /// Binds `fb_id` to the CRTC and puts it on screen (SETCRTC + PAGE_FLIP).
    fn present(fd: c_int, fb_id: u32) {
        let mut set = SetCrtc {
            set_connectors_ptr: &CONNECTOR_ID as *const u32 as u64,
            count_connectors: 1,
            crtc_id: CRTC_ID,
            fb_id,
            ..Default::default()
        };
        ioctl_raw(fd, SETCRTC, &mut set as *mut SetCrtc as *mut c_void)
            .expect("SETCRTC must enable the CRTC with the framebuffer");
        let mut flip = CrtcPageFlip {
            crtc_id: CRTC_ID,
            fb_id,
            ..Default::default()
        };
        ioctl_raw(fd, PAGE_FLIP, &mut flip as *mut CrtcPageFlip as *mut c_void)
            .expect("PAGE_FLIP must present the framebuffer");
    }

    fn create_dumb(fd: c_int) -> CreateDumb {
        let mut request = CreateDumb {
            width: 64,
            height: 64,
            bpp: 32,
            ..Default::default()
        };
        ioctl_raw(
            fd,
            CREATE_DUMB,
            &mut request as *mut CreateDumb as *mut c_void,
        )
        .expect("CREATE_DUMB must succeed on a modesetting node");
        assert_ne!(request.handle, 0);
        request
    }

    fn add_fb(fd: c_int, dumb: &CreateDumb) -> u32 {
        let mut request = FbCmd2 {
            width: dumb.width,
            height: dumb.height,
            pixel_format: DRM_FORMAT_XRGB8888,
            handles: [dumb.handle, 0, 0, 0],
            pitches: [dumb.pitch, 0, 0, 0],
            ..Default::default()
        };
        ioctl_raw(fd, FB_CMD2, &mut request as *mut FbCmd2 as *mut c_void)
            .expect("ADDFB2 must accept a matching dumb buffer");
        assert_ne!(request.fb_id, 0);
        request.fb_id
    }

    /// Rendering clients reach the card through `renderD*`, which Linux limits to
    /// the commands marked `DRM_RENDER_ALLOW`. The distinction between "node
    /// refuses" (EACCES) and "request refuses" (EINVAL) is what this checks.
    #[test]
    fn render_node_permission_matrix() {
        let path = render_path();
        let render = open_node(&path);
        let fd = render.as_raw_fd();

        let mut cap = GetCap {
            capability: DRM_CAP_DUMB_BUFFER,
            value: 0,
        };
        ioctl_raw(fd, GET_CAP, &mut cap as *mut GetCap as *mut c_void)
            .expect("GET_CAP is render-allowed");
        assert_eq!(cap.value, 1);

        // Shadow-buffered scanout must be advertised, or Xorg's modesetting
        // driver disables ShadowFB, stops sending DIRTYFB and the session goes
        // stale.
        let mut shadow = GetCap {
            capability: DRM_CAP_DUMB_PREFER_SHADOW,
            value: 0,
        };
        ioctl_raw(fd, GET_CAP, &mut shadow as *mut GetCap as *mut c_void)
            .expect("GET_CAP is render-allowed");
        assert_eq!(shadow.value, 1, "PREFER_SHADOW must be 1");

        let mut dumb = CreateDumb {
            width: 64,
            height: 64,
            bpp: 32,
            ..Default::default()
        };
        assert_eq!(
            errno_of(fd, CREATE_DUMB, &mut dumb),
            EACCES,
            "CREATE_DUMB is not render-allowed"
        );

        // PRIME is render-allowed, so the node accepts the command and the
        // request fails on the unknown handle instead of on the node. Linux
        // reports that failed `drm_gem_object_lookup` as ENOENT
        // (`drm_gem_prime_handle_to_fd`), which is what distinguishes "node
        // rejects the command" (EACCES) from "the handle does not exist".
        let mut prime = PrimeHandle {
            handle: 0xdead_beef,
            ..Default::default()
        };
        assert_eq!(errno_of(fd, PRIME_HANDLE_TO_FD, &mut prime), ENOENT);

        // SET_MASTER is not part of the render allowlist. `ioctl_no_arg` is the
        // only raw-ioctl wrapper, so the security note's single-unsafe-site
        // claim stays true.
        assert_eq!(ioctl_no_arg(fd, SET_MASTER), EACCES);
    }

    /// `fb_id == 0` is not special: like any unknown id it is `ENOENT`, which
    /// is what Xorg checks. A registered framebuffer flushes without rebinding.
    #[test]
    fn dirtyfb_flushes_without_rebinding() {
        let card = open_node(&device_path());
        let fd = card.as_raw_fd();

        let mut empty = DirtyFb::default();
        assert_eq!(
            errno_of(fd, DIRTYFB, &mut empty),
            ENOENT,
            "fb_id == 0 must return ENOENT like Linux"
        );

        let dumb = create_dumb(fd);
        let fb_id = add_fb(fd, &dumb);

        let mut known = DirtyFb {
            fb_id,
            ..Default::default()
        };
        ioctl_raw(fd, DIRTYFB, &mut known as *mut DirtyFb as *mut c_void)
            .expect("a registered framebuffer must flush");

        // Clip protocol: a count without a pointer is EINVAL, as is an odd
        // count for ANNOTATE_COPY.
        let mut count_only = DirtyFb {
            fb_id,
            num_clips: 2,
            ..Default::default()
        };
        assert_eq!(errno_of(fd, DIRTYFB, &mut count_only), EINVAL);
        let mut odd_copy = DirtyFb {
            fb_id,
            flags: 0x1,
            num_clips: 1,
            clips_ptr: 0x1000 as *mut c_void,
            ..Default::default()
        };
        assert_eq!(errno_of(fd, DIRTYFB, &mut odd_copy), EINVAL);

        let mut missing = DirtyFb {
            fb_id: fb_id + 1000,
            ..Default::default()
        };
        assert_eq!(errno_of(fd, DIRTYFB, &mut missing), ENOENT);

        let mut remove = RmFb { fb_id };
        ioctl_raw(fd, RMFB, &mut remove as *mut RmFb as *mut c_void).expect("RMFB must succeed");
        let mut destroy = DestroyDumb {
            handle: dumb.handle,
        };
        ioctl_raw(
            fd,
            DESTROY_DUMB,
            &mut destroy as *mut DestroyDumb as *mut c_void,
        )
        .expect("the last framebuffer is gone, so the handle can be destroyed");
    }

    /// The close-the-handle-first sequence must not pull the backing out from
    /// under a resource that is still on screen: the buffer is parked until the
    /// framebuffer stops naming it, and the next frame still presents.
    #[test]
    fn dumb_release_outlives_the_scanout() {
        let card = open_node(&device_path());
        let fd = card.as_raw_fd();

        let dumb = create_dumb(fd);
        let fb_id = add_fb(fd, &dumb);
        // Bind the framebuffer, so the resource is the scanout target when the
        // handle below is destroyed.
        present(fd, fb_id);

        // A dirty flush of the bound framebuffer must reach the backend: this is
        // the one path that exercises the display adapter's flush forwarder.
        let mut dirty = DirtyFb {
            fb_id,
            ..Default::default()
        };
        ioctl_raw(fd, DIRTYFB, &mut dirty as *mut DirtyFb as *mut c_void)
            .expect("dirty flush of the bound framebuffer");

        let mut destroy = DestroyDumb {
            handle: dumb.handle,
        };
        ioctl_raw(
            fd,
            DESTROY_DUMB,
            &mut destroy as *mut DestroyDumb as *mut c_void,
        )
        .expect("closing a handle that a framebuffer still names must succeed");

        // A second handle created after the first one was parked, then the
        // framebuffer teardown: the parked resource must be retired without
        // disturbing the resource that is about to be presented.
        let next = create_dumb(fd);
        let next_fb = add_fb(fd, &next);
        present(fd, next_fb);

        let mut remove = RmFb { fb_id };
        ioctl_raw(fd, RMFB, &mut remove as *mut RmFb as *mut c_void)
            .expect("removing the framebuffer releases the parked resource");
    }

    /// GEM handles are per open file: another description's handle is not found,
    /// and closing the creating file does not remove a framebuffer this file
    /// registered.
    #[test]
    fn handles_are_scoped_to_the_opening_file() {
        let card = open_node(&device_path());
        let fd = card.as_raw_fd();
        let other = open_node(&device_path());
        let other_fd = other.as_raw_fd();

        let foreign = create_dumb(other_fd);
        let mut close = GemClose {
            handle: foreign.handle,
            pad: 0,
        };
        assert_eq!(
            errno_of(fd, GEM_CLOSE, &mut close),
            EINVAL,
            "a handle created by another file is not in this file's table"
        );

        let mine = create_dumb(fd);
        let fb_id = add_fb(fd, &mine);
        drop(other);
        present(fd, fb_id);
        let mut remove = RmFb { fb_id };
        ioctl_raw(fd, RMFB, &mut remove as *mut RmFb as *mut c_void).expect("RMFB must succeed");
    }

    /// The render minor is `128 + card index`. Enumerating `/dev/dri` catches a
    /// projection mistake here instead of in a compositor's device probe.
    #[test]
    fn every_card_has_a_matching_render_node() {
        let mut cards = Vec::new();
        for entry in std::fs::read_dir("/dev/dri").expect("/dev/dri must be present") {
            let name = entry.expect("readable directory entry").file_name();
            let name = name.to_string_lossy();
            if let Some(index) = name.strip_prefix("card") {
                cards.push(index.parse::<u32>().expect("card index must be numeric"));
            }
        }
        assert!(!cards.is_empty(), "/dev/dri must contain at least one card");
        for index in cards {
            let render = format!("/dev/dri/renderD{}", DRM_RENDER_MINOR_BASE + index);
            assert!(
                std::path::Path::new(&render).exists(),
                "{render} must exist for card{index}"
            );
        }
    }
}
