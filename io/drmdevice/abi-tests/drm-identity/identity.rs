// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Userspace ABI regressions for X-Kernel's DRM identity ioctls.

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("DRM identity regressions require a 64-bit Linux userspace target");

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

    const VERSION: c_ulong = 0xc040_6400;
    const GET_UNIQUE: c_ulong = 0xc010_6401;
    const SET_VERSION: c_ulong = 0xc010_6407;
    const SET_MASTER: c_ulong = 0x641e;
    const DROP_MASTER: c_ulong = 0x641f;
    const EACCES: i32 = 13;
    const EBUSY: i32 = 16;
    const SENTINEL: u8 = 0xa5;

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    #[repr(C)]
    struct Version {
        major: c_int,
        minor: c_int,
        patchlevel: c_int,
        name_len: usize,
        name: *mut u8,
        date_len: usize,
        date: *mut u8,
        desc_len: usize,
        desc: *mut u8,
    }

    impl Version {
        fn query() -> Self {
            Self {
                major: 0,
                minor: 0,
                patchlevel: 0,
                name_len: 0,
                name: ptr::null_mut(),
                date_len: 0,
                date: ptr::null_mut(),
                desc_len: 0,
                desc: ptr::null_mut(),
            }
        }

        fn lengths(&self) -> [usize; 3] {
            [self.name_len, self.date_len, self.desc_len]
        }

        fn with_buffers(buffers: &mut [Vec<u8>; 3], lengths: [usize; 3]) -> Self {
            Self {
                name_len: lengths[0],
                name: buffers[0].as_mut_ptr().wrapping_add(1),
                date_len: lengths[1],
                date: buffers[1].as_mut_ptr().wrapping_add(1),
                desc_len: lengths[2],
                desc: buffers[2].as_mut_ptr().wrapping_add(1),
                ..Self::query()
            }
        }
    }

    #[repr(C)]
    struct Unique {
        len: usize,
        buffer: *mut u8,
    }

    #[repr(C)]
    struct SetVersion {
        interface_major: c_int,
        interface_minor: c_int,
        driver_major: c_int,
        driver_minor: c_int,
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
        // SAFETY: callers use the repr(C) ABI struct for the matching command;
        // its storage and any referenced buffers remain live through the call.
        let result = unsafe { ioctl(file.as_raw_fd(), command, ptr::from_mut(value)) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn master_ioctl(file: &File, command: c_ulong) -> io::Result<()> {
        // SAFETY: SET_MASTER and DROP_MASTER take no argument buffer, and the
        // File owns a live descriptor for the duration of the call.
        let result = unsafe { ioctl(file.as_raw_fd(), command, ptr::null_mut::<c_void>()) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn set_version(file: &File, major: c_int, minor: c_int) -> io::Result<()> {
        let mut request = SetVersion {
            interface_major: major,
            interface_minor: minor,
            driver_major: -1,
            driver_minor: -1,
        };
        ioctl_arg(file, SET_VERSION, &mut request)
    }

    fn unique_length(file: &File) -> usize {
        let mut unique = Unique {
            len: 0,
            buffer: ptr::null_mut(),
        };
        ioctl_arg(file, GET_UNIQUE, &mut unique).expect("GET_UNIQUE length query");
        unique.len
    }

    fn read_identity(file: &File) -> Vec<u8> {
        let len = unique_length(file);
        let mut bytes = vec![SENTINEL; len + 2];
        let mut unique = Unique {
            len,
            buffer: bytes.as_mut_ptr().wrapping_add(1),
        };
        ioctl_arg(file, GET_UNIQUE, &mut unique).expect("GET_UNIQUE complete copy");
        assert_eq!(unique.len, len);
        assert_eq!(bytes[0], SENTINEL, "GET_UNIQUE underwrote its destination");
        assert_eq!(bytes[len + 1], SENTINEL, "GET_UNIQUE wrote beyond capacity");
        bytes[1..len + 1].to_vec()
    }

    fn version_buffer_contract(file: &File) {
        let mut query = Version::query();
        ioctl_arg(file, VERSION, &mut query).expect("VERSION length query");
        let lengths = query.lengths();
        assert!(lengths.iter().all(|&len| len > 1));
        let mut full = lengths.map(|len| vec![SENTINEL; len + 2]);
        let mut request = Version::with_buffers(&mut full, lengths);
        ioctl_arg(file, VERSION, &mut request).expect("VERSION complete copy");
        assert_eq!(request.lengths(), lengths);
        for (bytes, len) in full.iter().zip(lengths) {
            assert_eq!(bytes[0], SENTINEL);
            assert_eq!(bytes[len + 1], SENTINEL, "VERSION appended beyond capacity");
        }

        for capacities in [[0; 3], [1; 3]] {
            let mut bytes = lengths.map(|len| vec![SENTINEL; len + 2]);
            let mut request = Version::with_buffers(&mut bytes, capacities);
            ioctl_arg(file, VERSION, &mut request).expect("VERSION bounded copy");
            assert_eq!(
                request.lengths(),
                lengths,
                "VERSION must report full lengths"
            );
            for index in 0..3 {
                let copied = capacities[index];
                assert_eq!(&bytes[index][1..copied + 1], &full[index][1..copied + 1]);
                assert_eq!(bytes[index][0], SENTINEL);
                assert!(
                    bytes[index][copied + 1..]
                        .iter()
                        .all(|&byte| byte == SENTINEL)
                );
            }
        }

        let mut null_query = Version {
            name_len: lengths[0],
            date_len: lengths[1],
            desc_len: lengths[2],
            ..Version::query()
        };
        ioctl_arg(file, VERSION, &mut null_query).expect("VERSION null destinations");
        assert_eq!(null_query.lengths(), lengths);
    }

    // One sequential case owns the device throughout, so independent opens
    // cannot interfere with one another through a parallel test runner.
    #[test]
    fn identity_buffers_and_master_lifecycle() {
        assert_eq!(size_of::<Version>(), 64);
        assert_eq!(size_of::<Unique>(), 16);
        assert_eq!(size_of::<SetVersion>(), 16);
        let owner = open_card();
        master_ioctl(&owner, SET_MASTER).expect("device must have no external owner");
        let client = open_card();
        assert_eq!(
            master_ioctl(&client, SET_MASTER)
                .unwrap_err()
                .raw_os_error(),
            Some(EBUSY)
        );
        assert_eq!(
            unique_length(&owner),
            0,
            "fresh master must have no identity"
        );
        version_buffer_contract(&owner);
        assert_eq!(
            unique_length(&owner),
            0,
            "VERSION must not establish identity"
        );

        for (major, minor) in [(1, 4), (-1, -1)] {
            assert_eq!(
                set_version(&client, major, minor)
                    .unwrap_err()
                    .raw_os_error(),
                Some(EACCES)
            );
            assert_eq!(
                unique_length(&owner),
                0,
                "non-owner changed shared identity"
            );
        }
        let mut unset = Unique {
            len: 8,
            buffer: ptr::null_mut(),
        };
        ioctl_arg(&owner, GET_UNIQUE, &mut unset)
            .expect("unset identity must not touch destination");
        assert_eq!(unset.len, 0);
        set_version(&owner, -1, 5).expect("query ignores interface minor");
        set_version(&owner, 1, 0).expect("interface 1.0 negotiation");
        assert_eq!(
            unique_length(&owner),
            0,
            "query and 1.0 must not establish identity"
        );
        let mut lower_version = SetVersion {
            interface_major: 1,
            interface_minor: 2,
            driver_major: -1,
            driver_minor: -1,
        };
        ioctl_arg(&owner, SET_VERSION, &mut lower_version).expect("interface 1.2 negotiation");
        assert_eq!(
            (lower_version.interface_major, lower_version.interface_minor),
            (1, 4),
            "SET_VERSION must report the highest supported interface version"
        );
        set_version(&owner, 1, 4).expect("active owner negotiates identity");
        let identity = read_identity(&owner);
        assert!(!identity.is_empty());
        assert_eq!(read_identity(&client), identity);

        let mut short = vec![SENTINEL; identity.len() + 2];
        let mut unique = Unique {
            len: identity.len() - 1,
            buffer: short.as_mut_ptr().wrapping_add(1),
        };
        ioctl_arg(&owner, GET_UNIQUE, &mut unique).expect("GET_UNIQUE insufficient capacity");
        assert_eq!(unique.len, identity.len());
        assert!(
            short.iter().all(|&byte| byte == SENTINEL),
            "GET_UNIQUE wrote a partial identity"
        );

        master_ioctl(&owner, DROP_MASTER).expect("owner releases active ownership");
        assert_eq!(read_identity(&owner), identity);
        assert_eq!(
            set_version(&owner, 1, 4).unwrap_err().raw_os_error(),
            Some(EACCES)
        );
        master_ioctl(&owner, SET_MASTER).expect("former owner reacquires its master");
        assert_eq!(
            read_identity(&owner),
            identity,
            "reacquisition discarded identity"
        );
        let duplicate = owner.try_clone().expect("duplicate owner descriptor");
        drop(owner);
        assert_eq!(
            master_ioctl(&client, SET_MASTER)
                .unwrap_err()
                .raw_os_error(),
            Some(EBUSY)
        );
        set_version(&duplicate, 1, 4).expect("duplicate retains active ownership");
        drop(duplicate);
        assert_eq!(
            read_identity(&client),
            identity,
            "last owner close cleared retained identity"
        );
        master_ioctl(&client, SET_MASTER).expect("last owner close releases active ownership");
        assert_eq!(
            unique_length(&client),
            0,
            "client becoming owner inherited old identity"
        );
        set_version(&client, 1, 4).expect("new owner negotiates its own identity");
        drop(client);
        let fresh = open_card();
        assert_eq!(
            unique_length(&fresh),
            0,
            "fresh owner inherited closed identity"
        );
        set_version(&fresh, 1, 4).expect("final owner close allows a fresh owner");
    }
}
