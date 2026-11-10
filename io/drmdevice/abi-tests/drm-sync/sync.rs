// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Userspace ABI regressions for X-Kernel's DRM synchronization objects.
//!
//! Every command word is written out here instead of being rebuilt from a
//! local structure: a layout drift on either side must show up as
//! `EOPNOTSUPP` from the ioctl, not as a silently different number that still
//! compiles on both sides.

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("DRM synchronization regressions require a 64-bit Linux userspace target");

#[cfg(all(test, target_os = "linux", target_pointer_width = "64"))]
mod tests {
    use std::{
        ffi::{c_int, c_ulong},
        fs::{File, OpenOptions},
        mem::size_of,
        os::fd::{AsRawFd, RawFd},
        ptr,
        time::{Duration, Instant},
    };

    /// `_IOW('d', 0x09, struct drm_gem_close)`.
    const GEM_CLOSE: c_ulong = 0x4008_6409;
    /// `_IOWR('d', 0x0C, struct drm_get_cap)`.
    const GET_CAP: c_ulong = 0xc010_640c;
    /// `_IOWR('d', 0xC0, struct drm_syncobj_create)`.
    const SYNCOBJ_CREATE: c_ulong = 0xc008_64bf;
    /// `_IOWR('d', 0xCB, struct drm_syncobj_eventfd)`, 24 bytes.
    const SYNCOBJ_EVENTFD: c_ulong = 0xc018_64cf;
    /// `_IOWR('d', 0xC1, struct drm_syncobj_destroy)`.
    const SYNCOBJ_DESTROY: c_ulong = 0xc008_64c0;
    /// `_IOWR('d', 0xC2, struct drm_syncobj_array)`.
    const SYNCOBJ_SIGNAL: c_ulong = 0xc010_64c5;
    /// `_IOWR('d', 0xC3, struct drm_syncobj_array)`.
    const SYNCOBJ_RESET: c_ulong = 0xc010_64c4;
    /// `_IOWR('d', 0xC4, struct drm_syncobj_wait)`.
    const SYNCOBJ_WAIT: c_ulong = 0xc028_64c3;
    /// `_IOWR('d', 0xC5, struct drm_syncobj_timeline_array)`.
    const SYNCOBJ_TIMELINE_SIGNAL: c_ulong = 0xc018_64cd;
    /// `_IOWR('d', 0xC6, struct drm_syncobj_wait)`.
    const SYNCOBJ_TIMELINE_WAIT: c_ulong = 0xc030_64ca;
    /// `_IOWR('d', 0xC7, struct drm_syncobj_handle)`.
    const SYNCOBJ_HANDLE_TO_FD: c_ulong = 0xc010_64c1;
    /// `_IOWR('d', 0xC8, struct drm_syncobj_handle)`.
    const SYNCOBJ_FD_TO_HANDLE: c_ulong = 0xc010_64c2;
    /// `_IOWR('d', 0xC9, struct drm_syncobj_transfer)`.
    const SYNCOBJ_TRANSFER: c_ulong = 0xc020_64cc;
    /// `_IOWR('d', 0xCA, struct drm_syncobj_query_args)`.
    const SYNCOBJ_QUERY: c_ulong = 0xc018_64cb;
    /// `_IOWR('d', 0xAE, struct drm_mode_fb_cmd)`.
    const ADDFB: c_ulong = 0xc01c_64ae;
    /// `_IOWR('d', 0xB2, struct drm_mode_create_dumb)`.
    const CREATE_DUMB: c_ulong = 0xc020_64b2;
    /// `_IOWR('d', 0x2D, struct drm_prime_handle)`.
    const PRIME_HANDLE_TO_FD: c_ulong = 0xc00c_642d;
    /// `_IOWR('d', 0x2E, struct drm_prime_handle)`.
    const PRIME_FD_TO_HANDLE: c_ulong = 0xc00c_642e;
    /// `_IOWR('>', 0x04, struct sync_file_info)`.
    const SYNC_IOC_FILE_INFO: c_ulong = 0xc038_3e04;
    /// `_IOWR('>', 3, struct sync_merge_data)`, 48 bytes.
    const SYNC_IOC_MERGE: c_ulong = 0xc030_3e03;
    /// `_IOW('>', 5, struct sync_set_deadline)`, 16 bytes.
    const SYNC_IOC_SET_DEADLINE: c_ulong = 0x4010_3e05;

    #[repr(C)]
    struct SyncMergeData {
        name: [u8; 32],
        fd2: i32,
        fence: i32,
        flags: u32,
        pad: u32,
    }

    #[repr(C)]
    struct SyncSetDeadline {
        deadline_ns: u64,
        pad: u64,
    }

    /// `SYNC_IOC_MERGE`: merges two sync_files and returns the new descriptor.
    fn sync_merge(first: RawFd, second: RawFd, name: &[u8]) -> Result<RawFd, i32> {
        let mut data = SyncMergeData {
            name: [0; 32],
            fd2: second,
            fence: -1,
            flags: 0,
            pad: 0,
        };
        data.name[..name.len().min(31)].copy_from_slice(&name[..name.len().min(31)]);
        call(first, SYNC_IOC_MERGE, &mut data).map_or(Ok(data.fence), Err)
    }

    /// `SYNC_IOC_SET_DEADLINE` with a deadline relative to now.
    fn sync_set_deadline(fd: RawFd, offset_ms: u64) -> Result<(), i32> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let mut request = SyncSetDeadline {
            deadline_ns: now + offset_ms * 1_000_000,
            pad: 0,
        };
        call(fd, SYNC_IOC_SET_DEADLINE, &mut request).map_or(Ok(()), Err)
    }
    /// `_IOW('b', 0x00, struct dma_buf_sync)`.
    const DMA_BUF_IOCTL_SYNC: c_ulong = 0x4008_6200;
    /// `_IOW('b', 0x02, struct dma_buf_export_sync_file)`.
    const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: c_ulong = 0x4008_6202;
    /// `_IOW('b', 0x03, struct dma_buf_import_sync_file)`.
    const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: c_ulong = 0x4008_6203;
    /// `_IOWR('d', 0xBC, struct drm_mode_atomic)`.
    const MODE_ATOMIC: c_ulong = 0xc038_64bc;
    /// `_IOW('d', 0x0D, struct drm_set_client_cap)`.
    const SET_CLIENT_CAP: c_ulong = 0x4010_640d;
    /// `_IOWR('d', 0xB9, struct drm_mode_obj_get_properties)`.
    const OBJ_GETPROPERTIES: c_ulong = 0xc020_64b9;
    /// `_IOWR('d', 0xAA, struct drm_mode_get_property)`.
    const GET_PROPERTY: c_ulong = 0xc040_64aa;

    const DRM_CAP_PRIME: u64 = 0x5;
    const DRM_CAP_SYNCOBJ: u64 = 0xD;
    const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x11;
    const DRM_CLIENT_CAP_ATOMIC: u64 = 3;

    const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1;
    const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1 << 0;

    const ENOENT: i32 = 2;
    const EINVAL: i32 = 22;
    const EBUSY: i32 = 16;
    const EFAULT: i32 = 14;

    /// CRTC object id of the single pipe.
    const CRTC_ID: u32 = 0x10;
    /// Plane object id.
    const PLANE_ID: u32 = 0x40;
    /// Property id of `OUT_FENCE_PTR`.
    const PROP_CRTC_OUT_FENCE_PTR: u32 = 0x202;
    /// Property id of `IN_FENCE_FD`.
    const PROP_PLANE_IN_FENCE_FD: u32 = 0x10C;
    /// Property id of `FB_ID`.
    const PROP_PLANE_FB_ID: u32 = 0x101;

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    /// Closes a descriptor this test owns, failing loudly on an error.
    fn close_fd(fd: RawFd) {
        // SAFETY: every caller owns `fd`.
        if unsafe { close(fd) } != 0 {
            panic!("close({fd}) failed");
        }
    }

    unsafe extern "C" {
        fn close(fd: c_int) -> c_int;
    }

    unsafe extern "C" {
        fn __errno_location() -> *mut c_int;
    }

    unsafe extern "C" {
        fn mmap(addr: *mut std::ffi::c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64)
        -> *mut std::ffi::c_void;
    }

    unsafe extern "C" {
        fn munmap(addr: *mut std::ffi::c_void, len: usize) -> c_int;
    }

    /// `PROT_READ`.
    const PROT_READ: c_int = 1;
    /// `MAP_SHARED`.
    const MAP_SHARED: c_int = 1;
    /// `MAP_FAILED`.
    const MAP_FAILED: *mut std::ffi::c_void = usize::MAX as *mut std::ffi::c_void;

    fn errno() -> i32 {
        // SAFETY: `errno` is a thread-local the libc exports on every Linux
        // target this test builds for.
        unsafe { *__errno_location() }
    }

    fn ret_errno(result: c_int) -> Option<i32> {
        (result < 0).then(errno)
    }

    /// One ioctl with a pointer argument, reporting the errno of a failure.
    fn call<T>(fd: RawFd, request: c_ulong, argument: &mut T) -> Option<i32> {
        // SAFETY: every caller passes the `repr(C)` request matching
        // `request`, and its storage stays live for the call.
        ret_errno(unsafe { ioctl(fd, request, ptr::from_mut(argument)) })
    }

    /// One ioctl with a pointer argument that must succeed.
    fn must<T>(fd: RawFd, request: c_ulong, argument: &mut T) {
        if let Some(error) = call(fd, request, argument) {
            panic!("ioctl {request:#x} failed with errno {error}");
        }
    }

    #[repr(C)]
    #[derive(Default)]
    struct GetCap {
        capability: u64,
        value: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SyncobjCreate {
        handle: u32,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SyncobjDestroy {
        handle: u32,
        pad: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SyncobjArray {
        handles: *const u32,
        count_handles: u32,
        pad: u32,
    }

    /// The timeline wait: the wire form of `struct drm_syncobj_timeline_wait`.
    #[repr(C)]
    #[derive(Default)]
    struct TimelineWait {
        handles: *const u32,
        points: *const u64,
        timeout_nsec: i64,
        count_handles: u32,
        flags: u32,
        first_signaled: u32,
        pad: u32,
        deadline_seqno: u64,
    }

    #[repr(C)]
    #[derive(Default, Debug)]
    struct SyncobjWait {
        handles: *const u32,
        timeout_nsec: i64,
        count_handles: u32,
        flags: u32,
        first_signaled: u32,
        pad: u32,
        deadline_seqno: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SyncobjTimelineArray {
        handles: *const u32,
        points: *const u64,
        count_handles: u32,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SyncobjHandle {
        handle: u32,
        flags: u32,
        fd: i32,
        pad: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SyncobjTransfer {
        src_handle: u32,
        dst_handle: u32,
        src_point: u64,
        dst_point: u64,
        flags: u32,
        pad: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SyncobjQuery {
        handles: *const u32,
        points: *mut u64,
        count_handles: u32,
        flags: u32,
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
    struct FbCmd {
        fb_id: u32,
        width: u32,
        height: u32,
        pitch: u32,
        bpp: u32,
        depth: u32,
        handle: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct PrimeHandle {
        handle: u32,
        flags: u32,
        fd: i32,
    }

    /// `struct sync_file_info` as `linux/sync_file.h` declares it.
    ///
    /// 56 bytes: the ioctl number encodes that size
    /// (`_IOWR('>', 4, struct sync_file_info)` is `0xc038_3e04`). An older
    /// kernel had a 32-byte `driver_name` after the name; current UAPI has
    /// `status` right after it, and a client built against the old header gets
    /// an unknown command instead of a wrong answer.
    #[repr(C)]
    struct SyncFileInfo {
        name: [u8; 32],
        status: i32,
        flags: u32,
        num_fences: u32,
        pad: u32,
        fence_info: *mut SyncFenceInfo,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct SyncFenceInfo {
        obj_name: [u8; 32],
        driver_name: [u8; 32],
        status: i32,
        seqno: u32,
        timestamp_ns: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct DmaBufSync {
        flags: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct DmaBufExportSyncFile {
        flags: u32,
        fd: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct DmaBufImportSyncFile {
        flags: u32,
        fd: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SyncobjEventfd {
        handle: u32,
        flags: u32,
        point: u64,
        fd: i32,
        pad: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct GemClose {
        handle: u32,
        pad: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SetClientCap {
        capability: u64,
        value: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ModeAtomic {
        flags: u32,
        count_objs: u32,
        objs_ptr: *const u32,
        count_props_ptr: *const u32,
        props_ptr: *const u32,
        prop_values_ptr: *const u64,
        reserved: u64,
        user_data: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ObjGetProperties {
        props_ptr: *mut u32,
        prop_values_ptr: *mut u64,
        count_props: u32,
        obj_id: u32,
        obj_type: u32,
        pad: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct GetProperty {
        values_ptr: *mut u64,
        enum_blob_ptr: *mut u8,
        prop_id: u32,
        flags: u32,
        name: [u8; 32],
        count_values: u32,
        count_enum_blobs: u32,
    }

    fn get_cap(fd: RawFd, capability: u64) -> u64 {
        let mut cap = GetCap {
            capability,
            ..Default::default()
        };
        must(fd, GET_CAP, &mut cap);
        cap.value
    }

    fn create_syncobj(fd: RawFd, flags: u32) -> u32 {
        let mut create = SyncobjCreate {
            flags,
            ..Default::default()
        };
        must(fd, SYNCOBJ_CREATE, &mut create);
        assert_ne!(create.handle, 0);
        create.handle
    }

    fn signal(fd: RawFd, handles: &[u32]) {
        let array = SyncobjArray {
            handles: handles.as_ptr(),
            count_handles: handles.len() as u32,
            pad: 0,
        };
        let mut array = array;
        must(fd, SYNCOBJ_SIGNAL, &mut array);
    }

    /// The outcome of one `SYNCOBJ_WAIT`, carrying the reported index.
    type WaitOutcome = Result<u32, i32>;

    /// Waits and reports `first_signaled`, or the errno of a failure.
    /// An absolute CLOCK_MONOTONIC instant `nanos` nanoseconds from now.
    ///
    /// `drm_syncobj_wait.timeout_nsec` is absolute (the UAPI calls it
    /// "absolute timeout"), so every wait in these tests has to name an
    /// instant, not a duration.
    fn deadline_in(nanos: i64) -> i64 {
        // CLOCK_MONOTONIC, not CLOCK_REALTIME: `timeout_nsec` is an instant on
        // the monotonic clock, and the two origins differ by the system's
        // uptime.
        let mut now = ClockTimespec::default();
        // SAFETY: `now` is a live `timespec` and `CLOCK_MONOTONIC` is a valid
        // clock id.
        assert_eq!(unsafe { clock_gettime(CLOCK_MONOTONIC, &mut now) }, 0);
        now.tv_sec * 1_000_000_000 + now.tv_nsec + nanos
    }

    /// `CLOCK_MONOTONIC`.
    const CLOCK_MONOTONIC: c_int = 1;

    #[repr(C)]
    #[derive(Default)]
    struct ClockTimespec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    unsafe extern "C" {
        fn clock_gettime(clock: c_int, time: *mut ClockTimespec) -> c_int;
    }

    fn wait(fd: RawFd, handles: &[u32], flags: u32, timeout_nsec: i64) -> WaitOutcome {
        let mut request = SyncobjWait {
            handles: handles.as_ptr(),
            timeout_nsec,
            count_handles: handles.len() as u32,
            flags,
            ..Default::default()
        };
        call(fd, SYNCOBJ_WAIT, &mut request).map_or(Ok(request.first_signaled), Err)
    }

    /// The size a descriptor reports through `fstat`, as `dma_buf_fops` does.
    fn descriptor_size(fd: RawFd) -> u64 {
        use std::os::fd::FromRawFd;
        // SAFETY: `fd` is an open descriptor for the duration of this call; the
        // duplicate owns it and closes it at the end of the scope.
        let file = unsafe { std::fs::File::from_raw_fd(libc_dup(fd)) };
        file.metadata().expect("fstat on a dma-buf").len()
    }

    unsafe extern "C" {
        #[link_name = "dup"]
        fn libc_dup(fd: c_int) -> c_int;
    }

    fn timeline_signal(fd: RawFd, handles: &[u32], points: &[u64]) {
        let mut array = SyncobjTimelineArray {
            handles: handles.as_ptr(),
            points: points.as_ptr(),
            count_handles: handles.len() as u32,
            flags: 0,
        };
        must(fd, SYNCOBJ_TIMELINE_SIGNAL, &mut array);
    }

    /// One `SYNCOBJ_TIMELINE_WAIT`.
    ///
    /// The handle array and the point array are separate arguments of the
    /// wire structure, so the points are reached through the request's
    /// second array field.
    fn timeline_wait(
        fd: RawFd,
        handles: &[u32],
        points: &[u64],
        timeout_nsec: i64,
    ) -> Result<(), i32> {
        let mut request = TimelineWait {
            handles: handles.as_ptr(),
            points: points.as_ptr(),
            timeout_nsec,
            count_handles: handles.len() as u32,
            flags: 0,
            first_signaled: 0,
            pad: 0,
            deadline_seqno: 0,
        };
        call(fd, SYNCOBJ_TIMELINE_WAIT, &mut request).map_or(Ok(()), Err)
    }

    fn query(fd: RawFd, handles: &[u32]) -> Vec<u64> {
        let mut points = vec![0u64; handles.len()];
        let mut query = SyncobjQuery {
            handles: handles.as_ptr(),
            points: points.as_mut_ptr(),
            count_handles: handles.len() as u32,
            flags: 1,
        };
        must(fd, SYNCOBJ_QUERY, &mut query);
        points
    }

    fn handle_to_fd(fd: RawFd, handle: u32, flags: u32) -> RawFd {
        let mut request = SyncobjHandle {
            handle,
            flags,
            ..Default::default()
        };
        must(fd, SYNCOBJ_HANDLE_TO_FD, &mut request);
        assert!(request.fd >= 0);
        request.fd
    }

    /// Imports `source`, returning the handle the object is named by here.
    fn fd_to_handle(fd: RawFd, handle: u32, flags: u32, source: RawFd) -> Result<u32, i32> {
        let mut request = SyncobjHandle {
            handle,
            flags,
            fd: source,
            ..Default::default()
        };
        call(fd, SYNCOBJ_FD_TO_HANDLE, &mut request).map_or(Ok(request.handle), Err)
    }

    /// Waits up to `millis` for `fd` to become readable.
    fn poll_readable(fd: RawFd, millis: i32) -> bool {
        // SAFETY: `pollfd` is the UAPI structure and the slice is live for the
        // call.
        let mut descriptor = PollFd {
            fd,
            events: POLLIN,
            revents: 0,
        };
        let ready = unsafe { poll(&mut descriptor, 1, millis) };
        ready > 0 && descriptor.revents & POLLIN != 0
    }

    /// `POLLIN`.
    const POLLIN: i16 = 1;

    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: i16,
        revents: i16,
    }

    unsafe extern "C" {
        fn poll(fds: *mut PollFd, count: std::ffi::c_ulong, timeout: c_int) -> c_int;
    }

    fn sync_file_info(fd: RawFd) -> (SyncFileInfo, Vec<SyncFenceInfo>) {
        // The count query first: `num_fences == 0` reports the count and the
        // whole file's status without touching the record buffer.
        let mut info = SyncFileInfo {
            name: [0xff; 32],
            status: -1,
            flags: 0,
            num_fences: 0,
            pad: 0,
            fence_info: ptr::null_mut(),
        };
        must(fd, SYNC_IOC_FILE_INFO, &mut info);
        let count = info.num_fences as usize;
        let mut records = vec![SyncFenceInfo::default(); count.max(1)];
        let mut info = SyncFileInfo {
            name: [0xff; 32],
            status: -1,
            flags: 0,
            num_fences: count as u32,
            pad: 0,
            fence_info: records.as_mut_ptr(),
        };
        must(fd, SYNC_IOC_FILE_INFO, &mut info);
        assert_eq!(info.num_fences as usize, count, "the count is reported");
        records.truncate(count);
        (info, records)
    }

    fn open_card() -> (File, String) {
        let path = std::env::var("DRM_DEVICE").unwrap_or_else(|_| "/dev/dri/card0".into());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("cannot open {path}: {error}"));
        (file, path)
    }

    /// `DRM_IO(0x1e)`, `DRM_IOCTL_SET_MASTER`.
    const SET_MASTER: c_ulong = 0x641e;

    /// Fails when another client owns the card.
    ///
    /// `SET_MASTER` reports `EBUSY` for a card another file already owns, which
    /// is the condition that makes a flip test meaningless.
    fn is_idle(fd: RawFd, path: &str) {
        // SAFETY: `SET_MASTER` takes no argument.
        if ret_errno(unsafe { ioctl(fd, SET_MASTER) }) == Some(EBUSY) {
            panic!("{path} is owned by another DRM client; use an idle card");
        }
    }

    /// `_IOWR('d', 0xB8, struct drm_mode_fb_cmd2)`, 104 bytes.
    const ADDFB2: c_ulong = 0xc068_64b8;

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

    #[test]
    fn drain_sync_defines_the_uapi_words() {
        // Sizes as a C compiler computes them from `include/uapi/`. Every one
        // is encoded into its ioctl number, and an unknown number reaches the
        // kernel as a different command or as nothing at all.
        assert_eq!(size_of::<SyncobjCreate>(), 8);
        assert_eq!(size_of::<SyncobjEventfd>(), 24);
        assert_eq!(size_of::<SyncobjDestroy>(), 8);
        assert_eq!(size_of::<SyncobjArray>(), 16);
        assert_eq!(size_of::<SyncobjWait>(), 40);
        assert_eq!(size_of::<TimelineWait>(), 48);
        assert_eq!(size_of::<SyncobjTimelineArray>(), 24);
        assert_eq!(size_of::<SyncobjHandle>(), 16);
        assert_eq!(size_of::<SyncobjTransfer>(), 32);
        assert_eq!(size_of::<SyncobjQuery>(), 24);
        assert_eq!(size_of::<SyncFileInfo>(), 56);
        assert_eq!(size_of::<SyncFenceInfo>(), 80);
        assert_eq!(size_of::<DmaBufSync>(), 8);
        assert_eq!(size_of::<SetClientCap>(), 16);
        assert_eq!(size_of::<FbCmd2>(), 104);
        assert_eq!(size_of::<ModeAtomic>(), 56);
    }

    #[test]
    fn capabilities_advertise_what_is_implemented() {
        let (card, path) = open_card();
        is_idle(card.as_raw_fd(), &path);
        let fd = card.as_raw_fd();
        assert_eq!(get_cap(fd, DRM_CAP_SYNCOBJ), 1);
        assert_eq!(get_cap(fd, DRM_CAP_SYNCOBJ_TIMELINE), 1);
        assert_eq!(get_cap(fd, DRM_CAP_PRIME), 3);
        // An unknown capability is `EINVAL`, as `drm_getcap` answers it; a
        // known-but-unsupported one is 0 with a successful result.
        let mut unknown = GetCap {
            capability: 0xffff,
            value: 0,
        };
        assert_eq!(call(fd, GET_CAP, &mut unknown), Some(EINVAL));
    }

    #[test]
    fn binary_syncobj_signal_and_wait() {
        let (card, path) = open_card();
        is_idle(card.as_raw_fd(), &path);
        let fd = card.as_raw_fd();
        let first = create_syncobj(fd, 0);
        let second = create_syncobj(fd, DRM_SYNCOBJ_CREATE_SIGNALED);

        // An empty object that will never be signaled: a deadline reports the
        // timeout as a successful wait with no first_signaled index.
        let started = Instant::now();
        let timed_out =
            wait(fd, &[first], 0, deadline_in(20_000_000)).expect("a timed wait succeeds");
        assert_eq!(timed_out, u32::MAX);
        assert!(started.elapsed() >= Duration::from_millis(15));

        // A signaled object satisfies the wait at once and names itself.
        let ready = wait(fd, &[first, second], 0, -1).expect("signaled wait");
        assert_eq!(ready, 1);

        signal(fd, &[first]);
        let ready = wait(fd, &[first], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, 0).expect("wait all");
        assert_eq!(ready, u32::MAX);

        // The binary value survives into a sync_file.
        let sync = handle_to_fd(fd, first, 1);
        let (info, records) = sync_file_info(sync);
        // `dma_fence_get_status`: a successful completion is 1, not 0.
        assert_eq!(info.status, 1, "a signaled object exports a signaled fence");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, 1);
        // STATUS and FLAGS follow sync_file.c: odd flags or pad are EINVAL,
        // a short record buffer is EINVAL, and SET_DEADLINE guards its pad.
        let mut bad = SyncFileInfo {
            name: [0; 32],
            status: 0,
            flags: 1,
            num_fences: 0,
            pad: 0,
            fence_info: ptr::null_mut(),
        };
        assert_eq!(call(sync, SYNC_IOC_FILE_INFO, &mut bad), Some(EINVAL));
        let mut short = SyncFileInfo {
            name: [0; 32],
            status: -1,
            flags: 0,
            num_fences: 1,
            pad: 0,
            fence_info: ptr::null_mut(),
        };
        // A null record buffer with a non-zero count is a copy fault, not a
        // validation error: `copy_to_user` answers `EFAULT`.
        assert_eq!(call(sync, SYNC_IOC_FILE_INFO, &mut short), Some(EFAULT));
        assert_eq!(sync_set_deadline(sync, 10), Ok(()));
        // A merge of this file with itself is a new readable file.
        let merged = sync_merge(sync, sync, b"merged").expect("merge");
        let (merged_info, merged_records) = sync_file_info(merged);
        assert_eq!(merged_records.len(), 1, "a duplicate fence is merged once");
        assert_eq!(merged_info.status, 1);
        close_fd(merged);
        assert!(records[0].timestamp_ns != 0, "a signal records its instant");
        close_fd(sync);

        let mut destroy = SyncobjDestroy {
            handle: first,
            pad: 0,
        };
        // SAFETY: the structure is the wire form of `SYNCOBJ_DESTROY`.
        assert_eq!(unsafe { ioctl(fd, SYNCOBJ_DESTROY, &mut destroy) }, 0);
        assert_eq!(
            wait(fd, &[first], 0, 0),
            Err(ENOENT),
            "a destroyed handle is gone"
        );
        // A second destroy is ENOENT; the pad field is checked first.
        assert_eq!(
            ret_errno(unsafe { ioctl(fd, SYNCOBJ_DESTROY, &destroy) }),
            Some(ENOENT)
        );
        destroy.pad = 1;
        assert_eq!(
            ret_errno(unsafe { ioctl(fd, SYNCOBJ_DESTROY, &destroy) }),
            Some(EINVAL)
        );
    }

    #[test]
    fn timeline_points_are_ordered_and_queryable() {
        let (card, path) = open_card();
        is_idle(card.as_raw_fd(), &path);
        let fd = card.as_raw_fd();
        let object = create_syncobj(fd, 0);

        timeline_signal(fd, &[object], &[7]);
        assert_eq!(query(fd, &[object]), vec![7]);
        // The point makes every earlier point available as well.
        timeline_wait(fd, &[object], &[3], -1).expect("point 3 is covered by 7");

        let sync = handle_to_fd(fd, object, 1);
        let (info, records) = sync_file_info(sync);
        // The exported fence is the signaled one the timeline holds.
        assert_eq!(info.status, 1);
        assert_eq!(records[0].status, 1);
        close_fd(sync);

        // RESET clears the timeline, so a query reports nothing available.
        let array = SyncobjArray {
            handles: &object,
            count_handles: 1,
            pad: 0,
        };
        // SAFETY: the array names a live handle for the call.
        assert_eq!(unsafe { ioctl(fd, SYNCOBJ_RESET, &array) }, 0);
        assert_eq!(query(fd, &[object]), vec![0]);
    }

    #[test]
    fn transfer_and_fd_round_trip_carry_a_fence() {
        let (card, path) = open_card();
        is_idle(card.as_raw_fd(), &path);
        let fd = card.as_raw_fd();
        let source = create_syncobj(fd, DRM_SYNCOBJ_CREATE_SIGNALED);
        let destination = create_syncobj(fd, 0);

        let transfer = SyncobjTransfer {
            src_handle: source,
            dst_handle: destination,
            src_point: 0,
            dst_point: 3,
            flags: 0,
            pad: 0,
        };
        // SAFETY: the structure is the wire form of `SYNCOBJ_TRANSFER`.
        assert_eq!(unsafe { ioctl(fd, SYNCOBJ_TRANSFER, &transfer) }, 0);
        timeline_wait(fd, &[destination], &[3], -1).expect("the transferred fence is signaled");

        // `EXPORT_SYNC_FILE` hands out a real sync_file, and `IMPORT_SYNC_FILE`
        // installs its fence on another object; a card descriptor is neither.
        let sync = handle_to_fd(fd, destination, 1);
        let reimported = create_syncobj(fd, 0);
        fd_to_handle(fd, reimported, 1, sync).expect("import the exported sync_file");
        wait(fd, &[reimported], 0, -1).expect("the imported fence is signaled");
        assert_eq!(
            fd_to_handle(fd, reimported, 1, card.as_raw_fd()),
            Err(EINVAL),
            "a card descriptor is not a sync_file"
        );
        // Plain export hands out the syncobj object; importing it names the
        // same object, so the handle it returns signals together with the
        // original.
        let object_fd = handle_to_fd(fd, destination, 0);
        let named = create_syncobj(fd, 0);
        let imported = fd_to_handle(fd, named, 0, object_fd).expect("import the syncobj object");
        wait(fd, &[imported], 0, -1).expect("the imported object names the same value");
        close_fd(object_fd);
        close_fd(sync);
    }

    #[test]
    fn wait_rejects_unknown_objects_and_bad_flags() {
        let (card, path) = open_card();
        is_idle(card.as_raw_fd(), &path);
        let fd = card.as_raw_fd();
        assert_eq!(wait(fd, &[u32::MAX], 0, 0), Err(ENOENT));
        let object = create_syncobj(fd, 0);
        assert_eq!(
            wait(fd, &[object], 1 << 7, 0),
            Err(EINVAL),
            "an undefined flag is rejected"
        );
        // An empty wait succeeds at once: `count_handles == 0` returns 0
        // before anything else is looked at.
        assert_eq!(
            wait(fd, &[], 0, 0),
            Ok(u32::MAX),
            "a wait without objects succeeds"
        );
        assert_eq!(
            wait(fd, &[object], 0, -1),
            Err(EINVAL),
            "a negative timeout is invalid"
        );
    }

    /// `_IOWR('d', 0xA0, struct drm_mode_card_res)`.
    const GET_RESOURCES: c_ulong = 0xc040_64a0;

    #[repr(C)]
    #[derive(Default)]
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

    /// The first connector id the card reports.
    fn connector(fd: RawFd) -> u32 {
        let mut ids = [0u32; 4];
        let mut resources = CardRes {
            connector_id_ptr: ids.as_mut_ptr(),
            count_connectors: ids.len() as u32,
            ..Default::default()
        };
        must(fd, GET_RESOURCES, &mut resources);
        assert!(resources.count_connectors >= 1, "the card has a connector");
        ids[0]
    }

    /// `_IOWR('d', 0xA2, struct drm_mode_crtc)`.
    const SETCRTC: c_ulong = 0xc068_64a2;

    #[repr(C)]
    #[derive(Default)]
    struct ModeCrtc {
        set_connectors_ptr: *mut u32,
        count_connectors: u32,
        crtc_id: u32,
        fb_id: u32,
        x: u32,
        y: u32,
        gamma_size: u32,
        mode_valid: u32,
        mode: [u32; 17],
    }

    /// Allocates a dumb buffer and registers a framebuffer over it.
    fn add_framebuffer(fd: RawFd) -> (u32, u32, u32) {
        let mut dumb = CreateDumb {
            width: 64,
            height: 48,
            bpp: 32,
            ..Default::default()
        };
        must(fd, CREATE_DUMB, &mut dumb);
        let mut fb = FbCmd {
            width: dumb.width,
            height: dumb.height,
            pitch: dumb.pitch,
            bpp: 32,
            depth: 24,
            handle: dumb.handle,
            ..Default::default()
        };
        must(fd, ADDFB, &mut fb);
        assert_ne!(fb.fb_id, 0);
        (dumb.handle, dumb.pitch, fb.fb_id)
    }

    /// Sets the card's single CRTC to `fb`.
    fn set_mode(fd: RawFd, fb: u32) {
        let mut connectors = [connector(fd)];
        let mut crtc = ModeCrtc {
            set_connectors_ptr: connectors.as_mut_ptr(),
            count_connectors: 1,
            crtc_id: CRTC_ID,
            fb_id: fb,
            mode_valid: 0,
            ..Default::default()
        };
        must(fd, SETCRTC, &mut crtc);
    }

    #[test]
    fn an_imported_dma_buf_scans_out_as_a_framebuffer() {
        let (card, _path) = open_card();
        let fd = card.as_raw_fd();
        let (handle, pitch, _fb) = add_framebuffer(fd);

        let mut prime = PrimeHandle {
            handle,
            flags: 0,
            fd: -1,
        };
        must(fd, PRIME_HANDLE_TO_FD, &mut prime);
        let export = prime.fd;

        // Re-open the dma-buf from a second card descriptor: the import must
        // not depend on which file exported it.
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(std::env::var("DRM_DEVICE").unwrap_or_else(|_| "/dev/dri/card0".into()))
            .expect("second open");
        let mut imported = PrimeHandle {
            flags: 0,
            fd: export,
            handle: 0,
        };
        must(second.as_raw_fd(), PRIME_FD_TO_HANDLE, &mut imported);
        assert_ne!(imported.handle, handle, "the import is its own handle");

        // The imported object backs a framebuffer of the exported geometry,
        // with the same layout; ADDFB2 accepts it because the object is large
        // enough, not because it was created here.
        let mut fb = FbCmd2 {
            width: 64,
            height: 48,
            pixel_format: 0x3432_5258, // DRM_FORMAT_XRGB8888
            handles: [imported.handle, 0, 0, 0],
            pitches: [pitch, 0, 0, 0],
            ..Default::default()
        };
        must(second.as_raw_fd(), ADDFB2, &mut fb);
        assert_ne!(fb.fb_id, 0);

        // A start of CPU access on the export is the sync a writer performs
        // before it fills the buffer; the END form publishes the completion.
        let mut begin = DmaBufSync { flags: 2 };
        must(export, DMA_BUF_IOCTL_SYNC, &mut begin);
        let mut end = DmaBufSync { flags: 2 | 4 };
        must(export, DMA_BUF_IOCTL_SYNC, &mut end);

        // The dma-buf maps its own backing.
        // SAFETY: the mapping is only established, not dereferenced here; the
        // kernel maps the export's pages for the descriptor's lifetime.
        let mapped = unsafe {
            mmap(
                std::ptr::null_mut(),
                64 * 48 * 4,
                PROT_READ,
                MAP_SHARED,
                export,
                0,
            )
        };
        assert_ne!(mapped, MAP_FAILED, "a dma-buf maps its backing");
        // SAFETY: `mapped` is the mapping just established above.
        assert_eq!(unsafe { munmap(mapped, 64 * 48 * 4) }, 0);

        let mut close = GemClose {
            handle: imported.handle,
            pad: 0,
        };
        must(second.as_raw_fd(), GEM_CLOSE, &mut close);
        close_fd(export);
    }

    #[test]
    fn dma_buf_exports_and_imports_one_backing() {
        let (card, _path) = open_card();
        let fd = card.as_raw_fd();
        let (handle, _pitch, _fb) = add_framebuffer(fd);

        let mut prime = PrimeHandle {
            handle,
            flags: 0,
            fd: -1,
        };
        must(fd, PRIME_HANDLE_TO_FD, &mut prime);
        let export = prime.fd;
        assert!(export >= 3, "the export is a real descriptor");

        // Linux has no size ioctl; the descriptor's `st_size` is the buffer
        // size, and it must cover the pixels.
        let size = descriptor_size(export);
        assert!(size >= 64 * 48 * 4, "the exported size covers the pixels");

        // A start of read access on a buffer nobody writes completes at once.
        let mut sync = DmaBufSync { flags: 1 };
        must(export, DMA_BUF_IOCTL_SYNC, &mut sync);
        // `DMA_BUF_SYNC_NOWAIT` is an Android-era bit outside Linux's
        // validation mask, so Linux answers `EINVAL` for it.
        let mut nowait = DmaBufSync {
            flags: 1 | (1 << 4),
        };
        assert_eq!(
            call(export, DMA_BUF_IOCTL_SYNC, &mut nowait),
            Some(EINVAL),
            "the legacy NOWAIT bit is rejected"
        );
        let mut no_direction = DmaBufSync { flags: 0 };
        assert_eq!(
            call(export, DMA_BUF_IOCTL_SYNC, &mut no_direction),
            Some(EINVAL),
            "a sync names a direction"
        );

        // Exporting the buffer's fence yields a sync_file with one record; the
        // request names the direction, and a missing one is `EINVAL` as in
        // `dma_buf_export_sync_file`.
        let mut no_direction_export = DmaBufExportSyncFile { flags: 0, fd: -1 };
        assert_eq!(
            call(export, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut no_direction_export),
            Some(EINVAL)
        );
        let mut exported = DmaBufExportSyncFile {
            flags: 1,
            fd: -1,
        };
        must(export, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut exported);
        let (_info, records) = sync_file_info(exported.fd);
        assert_eq!(records.len(), 1);

        // Importing the same descriptor back names the same backing.
        let mut imported = PrimeHandle {
            flags: 0,
            fd: export,
            handle: 0,
        };
        must(fd, PRIME_FD_TO_HANDLE, &mut imported);
        // A second import of the same descriptor names the same handle:
        // `drm_prime_lookup_buf_handle` resolves a buffer this file already
        // imported to the handle it got the first time.
        let mut again = PrimeHandle {
            flags: 0,
            fd: export,
            handle: 0,
        };
        must(fd, PRIME_FD_TO_HANDLE, &mut again);
        assert_eq!(
            again.handle, imported.handle,
            "a repeated import resolves to the existing handle"
        );
        // A second export of the same object is the same buffer.
        let mut second_named = PrimeHandle {
            handle,
            flags: 0,
            fd: -1,
        };
        must(fd, PRIME_HANDLE_TO_FD, &mut second_named);
        let mut repeated = PrimeHandle {
            flags: 0,
            fd: second_named.fd,
            handle: 0,
        };
        must(fd, PRIME_FD_TO_HANDLE, &mut repeated);
        assert_eq!(
            repeated.handle, imported.handle,
            "a re-exported buffer resolves to the existing import"
        );
        close_fd(second_named.fd);
        let imported_export = {
            let mut second = PrimeHandle {
                handle: imported.handle,
                flags: 0,
                fd: -1,
            };
            must(fd, PRIME_HANDLE_TO_FD, &mut second);
            second.fd
        };
        let imported_size = descriptor_size(imported_export);
        assert_eq!(imported_size, size, "both names describe one allocation");

        // A start fence can be imported as a reader of the buffer.
        let mut back = DmaBufImportSyncFile {
            flags: 1,
            fd: exported.fd,
        };
        must(export, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &mut back);
        let mut no_direction = DmaBufImportSyncFile { flags: 0, fd: 0 };
        assert_eq!(
            call(export, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &mut no_direction),
            Some(EINVAL)
        );

        // A descriptor that is not a dma-buf is `ENOENT`, as a failed
        // `dma_buf_get` is in Linux; "unsupported" is reserved for a real
        // dma-buf this card cannot import.
        let mut foreign = PrimeHandle {
            flags: 0,
            fd: card.as_raw_fd(),
            handle: 0,
        };
        assert_eq!(call(fd, PRIME_FD_TO_HANDLE, &mut foreign), Some(ENOENT));

        // Closing the handle and the card descriptor leaves the exported
        // buffer valid: the dma-buf owns its backing.
        let mut close = GemClose { handle, pad: 0 };
        must(fd, GEM_CLOSE, &mut close);
        close_fd(exported.fd);
        close_fd(imported_export);
        close_fd(export);
    }

    #[test]
    fn atomic_commit_hands_out_an_out_fence() {
        let (card, path) = open_card();
        is_idle(card.as_raw_fd(), &path);
        let fd = card.as_raw_fd();
        let mut cap = SetClientCap {
            capability: DRM_CLIENT_CAP_ATOMIC,
            value: 1,
        };
        must(fd, SET_CLIENT_CAP, &mut cap);

        // The plane exposes IN_FENCE_FD and reads it as -1 when no fence is set.
        let mut props = vec![0u32; 32];
        let mut values = vec![0u64; 32];
        let mut query = ObjGetProperties {
            props_ptr: props.as_mut_ptr(),
            prop_values_ptr: values.as_mut_ptr(),
            count_props: props.len() as u32,
            obj_id: PLANE_ID,
            ..Default::default()
        };
        must(fd, OBJ_GETPROPERTIES, &mut query);
        props.truncate(query.count_props as usize);
        let position = props
            .iter()
            .position(|&prop| prop == PROP_PLANE_IN_FENCE_FD)
            .expect("the plane exposes IN_FENCE_FD");
        assert_eq!(values[position], u64::MAX, "IN_FENCE_FD reads -1");

        // A bad IN_FENCE_FD is EINVAL, as `sync_file_get_fence` reports.
        let mut out_fence: i32 = 0;
        let objs = [PLANE_ID, CRTC_ID];
        let counts = [1u32, 1u32];
        let props = [PROP_PLANE_IN_FENCE_FD, PROP_CRTC_OUT_FENCE_PTR];
        let values = [card.as_raw_fd() as u64, &mut out_fence as *mut i32 as u64];
        let mut atomic = ModeAtomic {
            count_objs: objs.len() as u32,
            objs_ptr: objs.as_ptr(),
            count_props_ptr: counts.as_ptr(),
            props_ptr: props.as_ptr(),
            prop_values_ptr: values.as_ptr(),
            ..Default::default()
        };
        assert_eq!(
            call(fd, MODE_ATOMIC, &mut atomic),
            Some(EINVAL),
            "a card descriptor is not an in-fence"
        );

        // A TEST_ONLY commit reports the out-fence word as -1.
        let mut out_fence: i32 = 0x7fff;
        let values = [&mut out_fence as *mut i32 as u64];
        let objs = [CRTC_ID];
        let counts = [1u32];
        let props = [PROP_CRTC_OUT_FENCE_PTR];
        let mut test_only = ModeAtomic {
            flags: 0x0100,
            count_objs: 1,
            objs_ptr: objs.as_ptr(),
            count_props_ptr: counts.as_ptr(),
            props_ptr: props.as_ptr(),
            prop_values_ptr: values.as_ptr(),
            ..Default::default()
        };
        must(fd, MODE_ATOMIC, &mut test_only);
        assert_eq!(out_fence, -1, "TEST_ONLY never hands out a fence");

        // An unmapped out-fence address is EFAULT.
        let mut atomic = ModeAtomic {
            count_objs: 1,
            objs_ptr: objs.as_ptr(),
            count_props_ptr: counts.as_ptr(),
            props_ptr: props.as_ptr(),
            prop_values_ptr: [1u64].as_ptr(),
            ..Default::default()
        };
        assert_eq!(call(fd, MODE_ATOMIC, &mut atomic), Some(EFAULT));

        // The CRTC exposes OUT_FENCE_PTR as an atomic range property.
        let mut property = GetProperty {
            prop_id: PROP_CRTC_OUT_FENCE_PTR,
            ..Default::default()
        };
        must(fd, GET_PROPERTY, &mut property);
        assert_eq!(
            &property.name[..12],
            b"OUT_FENCE_PT",
            "the property is named as in Linux"
        );

        // A real flip: the out-fence descriptor is readable once the flip
        // reached the display, and its record carries a timestamp.
        let (_handle, _pitch, fb) = add_framebuffer(fd);
        set_mode(fd, fb);
        let mut out_fence: i32 = -1;
        let objs = [PLANE_ID, CRTC_ID];
        let counts = [2u32, 1u32];
        let props = [
            PROP_PLANE_FB_ID,
            PROP_PLANE_IN_FENCE_FD,
            PROP_CRTC_OUT_FENCE_PTR,
        ];
        let values = [u64::from(fb), u64::MAX, &mut out_fence as *mut i32 as u64];
        let mut atomic = ModeAtomic {
            count_objs: objs.len() as u32,
            objs_ptr: objs.as_ptr(),
            count_props_ptr: counts.as_ptr(),
            props_ptr: props.as_ptr(),
            prop_values_ptr: values.as_ptr(),
            ..Default::default()
        };
        must(fd, MODE_ATOMIC, &mut atomic);
        assert!(out_fence >= 0, "the commit handed out an out-fence");
        // The flip completes on a blank, so the fence is pollable rather than
        // already signaled; waiting on it is what a compositor does next.
        assert!(poll_readable(out_fence, 2_000), "the fence becomes readable");
        let (_info, records) = sync_file_info(out_fence);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, 1, "the flip completed");
        assert!(records[0].timestamp_ns != 0, "completion is timestamped");
        close_fd(out_fence);
    }

    #[test]
    fn eventfd_waits_require_a_real_eventfd() {
        let (card, path) = open_card();
        is_idle(card.as_raw_fd(), &path);
        let fd = card.as_raw_fd();
        let object = create_syncobj(fd, 0);
        let mut eventfd = SyncobjEventfd {
            handle: object,
            flags: 0,
            point: 1,
            fd: card.as_raw_fd(),
            pad: 0,
        };
        // The descriptor must be an eventfd; a DRM card fd is not one, and
        // `eventfd_ctx_fdget` answers `EINVAL`, which comes before the
        // command's own support.
        assert_eq!(call(fd, SYNCOBJ_EVENTFD, &mut eventfd), Some(EINVAL));
    }

    #[test]
    fn every_timeout_form_follows_the_absolute_instant_contract() {
        let (card, path) = open_card();
        is_idle(card.as_raw_fd(), &path);
        let fd = card.as_raw_fd();
        let object = create_syncobj(fd, 0);

        // `0` is a poll: the value is unavailable, reported as a successful
        // wait without an index, and it returns without sleeping.
        let started = Instant::now();
        assert_eq!(wait(fd, &[object], 0, 0), Ok(u32::MAX));
        assert!(started.elapsed() < Duration::from_millis(200));

        // A past absolute instant is also a poll (`drm_timeout_abs_to_jiffies`
        // turns it into a zero timeout).
        assert_eq!(wait(fd, &[object], 0, 1), Ok(u32::MAX));

        // A future instant parks until it passes, then reports the timeout as
        // a successful wait.
        let started = Instant::now();
        assert_eq!(
            wait(fd, &[object], 0, deadline_in(30_000_000)),
            Ok(u32::MAX)
        );
        assert!(started.elapsed() >= Duration::from_millis(10));

        // The infinite wait stays until the value is available; a wait that
        // succeeds names the object that satisfied it.
        signal(fd, &[object]);
        assert_eq!(wait(fd, &[object], 0, -1), Ok(0));
    }
}
