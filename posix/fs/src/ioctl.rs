// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `ioctl(2)` syscall implementation.

use core::{ffi::c_int, mem::size_of};

use kerrno::{KError, KResult};
use kvfs::{
    FiemapExtentFlags, FiemapExtentInfo, FiemapExtentWriter, FiemapFlags, OpenFlags, VfsError,
    VfsFile, VfsResult,
};
use linux_raw_sys::{
    general as fs_flags,
    ioctl::{
        FIONBIO, FS_IOC_FIEMAP, FS_IOC_FSGETXATTR, FS_IOC_FSSETXATTR, FS_IOC_GETFLAGS,
        FS_IOC_SETFLAGS, TCGETS, TIOCGWINSZ,
    },
};
use posix_types::{UserConstPtr, UserPtr, UserRead, UserWrite};

#[repr(C)]
#[derive(Clone, Copy, UserRead, UserWrite)]
struct FiemapHeader {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, UserWrite)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, UserRead, UserWrite)]
struct Fsxattr {
    xflags: u32,
    extsize: u32,
    nextents: u32,
    project_id: u32,
    cowextsize: u32,
    pad: [u8; 8],
}

const FILEATTR_FLAGS: [(u32, u32); 8] = [
    (fs_flags::FS_SYNC_FL, fs_flags::FS_XFLAG_SYNC),
    (fs_flags::FS_IMMUTABLE_FL, fs_flags::FS_XFLAG_IMMUTABLE),
    (fs_flags::FS_APPEND_FL, fs_flags::FS_XFLAG_APPEND),
    (fs_flags::FS_NODUMP_FL, fs_flags::FS_XFLAG_NODUMP),
    (fs_flags::FS_NOATIME_FL, fs_flags::FS_XFLAG_NOATIME),
    (fs_flags::FS_DAX_FL, fs_flags::FS_XFLAG_DAX),
    (fs_flags::FS_PROJINHERIT_FL, fs_flags::FS_XFLAG_PROJINHERIT),
    (fs_flags::FS_VERITY_FL, 0x0002_0000),
];

fn ioctl_fileattr(file: &VfsFile, cmd: u32, arg: usize) -> KResult<isize> {
    let path = file.path();
    match cmd {
        FS_IOC_GETFLAGS => {
            UserPtr::<u32>::from(arg).write_vm(path.fileattr_get()?.flags)?;
        }
        FS_IOC_SETFLAGS => {
            let flags = UserConstPtr::<u32>::from(arg).read_vm()?;
            path.fileattr_set(
                kvfs::FileAttr {
                    flags,
                    project_id: 0,
                    flags_valid: true,
                },
                &kprocess::current_cred(),
            )?;
        }
        FS_IOC_FSGETXATTR => {
            let attributes = path.fileattr_get()?;
            let xflags = FILEATTR_FLAGS.iter().fold(0, |flags, &(common, xflag)| {
                flags
                    | if attributes.flags & common != 0 {
                        xflag
                    } else {
                        0
                    }
            });
            UserPtr::<Fsxattr>::from(arg).write_vm(Fsxattr {
                xflags,
                extsize: 0,
                nextents: 0,
                project_id: attributes.project_id,
                cowextsize: 0,
                pad: [0; 8],
            })?;
        }
        FS_IOC_FSSETXATTR => {
            let attributes = UserConstPtr::<Fsxattr>::from(arg).read_vm()?;
            const KNOWN_XFLAGS: u32 = fs_flags::FS_XFLAG_REALTIME
                | fs_flags::FS_XFLAG_PREALLOC
                | fs_flags::FS_XFLAG_IMMUTABLE
                | fs_flags::FS_XFLAG_APPEND
                | fs_flags::FS_XFLAG_SYNC
                | fs_flags::FS_XFLAG_NOATIME
                | fs_flags::FS_XFLAG_NODUMP
                | fs_flags::FS_XFLAG_RTINHERIT
                | fs_flags::FS_XFLAG_PROJINHERIT
                | fs_flags::FS_XFLAG_NOSYMLINKS
                | fs_flags::FS_XFLAG_EXTSIZE
                | fs_flags::FS_XFLAG_EXTSZINHERIT
                | fs_flags::FS_XFLAG_NODEFRAG
                | fs_flags::FS_XFLAG_FILESTREAM
                | fs_flags::FS_XFLAG_DAX
                | fs_flags::FS_XFLAG_COWEXTSIZE
                | fs_flags::FS_XFLAG_HASATTR
                | 0x000e_0000;
            if attributes.xflags & !KNOWN_XFLAGS != 0 {
                return Err(KError::OperationNotSupported);
            }
            if attributes.xflags & fs_flags::FS_XFLAG_EXTSIZE != 0 && !file.is_regular_file()
                || attributes.xflags & fs_flags::FS_XFLAG_EXTSZINHERIT != 0 && !file.is_dir()
                || attributes.xflags & fs_flags::FS_XFLAG_COWEXTSIZE != 0
                    && !file.is_dir()
                    && !file.is_regular_file()
                || attributes.xflags & fs_flags::FS_XFLAG_DAX != 0
                    && !file.is_dir()
                    && !file.is_regular_file()
            {
                return Err(KError::InvalidInput);
            }
            let flags = FILEATTR_FLAGS.iter().fold(0, |flags, &(common, xflag)| {
                flags
                    | if attributes.xflags & xflag != 0 {
                        common
                    } else {
                        0
                    }
            });
            path.fileattr_set(
                kvfs::FileAttr {
                    flags,
                    project_id: attributes.project_id,
                    flags_valid: false,
                },
                &kprocess::current_cred(),
            )?;
        }
        _ => return Err(KError::InvalidInput),
    }
    Ok(0)
}

const FIEMAP_MAX_EXTENTS: u32 = u32::MAX / size_of::<FiemapExtent>() as u32;

struct FiemapUserWriter {
    extents_address: usize,
}

impl FiemapUserWriter {
    fn new(ioctl_arg: usize, capacity: u32) -> KResult<Self> {
        if capacity > FIEMAP_MAX_EXTENTS {
            return Err(KError::InvalidInput);
        }
        let extents_address = ioctl_arg
            .checked_add(size_of::<FiemapHeader>())
            .ok_or(KError::BadAddress)?;
        let extent_bytes = usize::try_from(capacity)
            .ok()
            .and_then(|count| count.checked_mul(size_of::<FiemapExtent>()))
            .ok_or(KError::BadAddress)?;
        extents_address
            .checked_add(extent_bytes)
            .ok_or(KError::BadAddress)?;
        Ok(Self { extents_address })
    }
}

impl FiemapExtentWriter for FiemapUserWriter {
    fn write_extent(
        &mut self,
        index: u32,
        logical: u64,
        physical: u64,
        length: u64,
        flags: FiemapExtentFlags,
    ) -> VfsResult<()> {
        let index = usize::try_from(index).map_err(|_| VfsError::BadAddress)?;
        let byte_offset = index
            .checked_mul(size_of::<FiemapExtent>())
            .ok_or(VfsError::BadAddress)?;
        let destination = UserPtr::<FiemapExtent>::from(
            self.extents_address
                .checked_add(byte_offset)
                .ok_or(VfsError::BadAddress)?,
        );
        destination.write_vm(FiemapExtent {
            logical,
            physical,
            length,
            reserved64: [0; 2],
            flags: flags.bits(),
            reserved: [0; 3],
        })?;
        Ok(())
    }
}

fn ioctl_fiemap(file: &VfsFile, ioctl_arg: usize) -> KResult<isize> {
    // Match Linux's error ordering: unsupported inodes are rejected before
    // the userspace request header is accessed.
    let capability = file
        .inode()
        .fiemap_capability()
        .ok_or(KError::OperationNotSupported)?;
    let header_ptr = UserPtr::<FiemapHeader>::from(ioctl_arg);
    let mut header = header_ptr.read_vm()?;
    let mut writer = FiemapUserWriter::new(ioctl_arg, header.extent_count)?;
    let mut info = FiemapExtentInfo::new(
        FiemapFlags::from_bits_retain(header.flags),
        header.extent_count,
        &mut writer,
    );
    let result = capability.map(&mut info, header.start, header.length);

    header.flags = info.flags().bits();
    header.mapped_extents = info.mapped_extents();
    header_ptr.write_vm(header)?;
    result?;
    Ok(0)
}

/// The `ioctl()` syscall manipulates the underlying device parameters
/// of special files.
pub fn sys_ioctl(fd: i32, cmd: u32, arg: usize) -> KResult<isize> {
    debug!("sys_ioctl <= fd: {fd}, cmd: {cmd}, arg: {arg}");
    let f = kprocess::current_files().get_file(fd)?;
    if matches!(
        cmd,
        FS_IOC_GETFLAGS | FS_IOC_SETFLAGS | FS_IOC_FSGETXATTR | FS_IOC_FSSETXATTR
    ) {
        return ioctl_fileattr(&f, cmd, arg);
    }
    if cmd == FS_IOC_FIEMAP {
        return ioctl_fiemap(&f, arg);
    }
    if cmd == FIONBIO {
        let val = UserConstPtr::<c_int>::from(arg).read_vm()?;
        if val != 0 && val != 1 {
            return Err(KError::InvalidInput);
        }
        let flags = if val != 0 {
            OpenFlags::NONBLOCK
        } else {
            OpenFlags::empty()
        };
        f.replace_flags(OpenFlags::NONBLOCK, flags);
        return Ok(0);
    }
    f.ioctl(&kprocess::current_cred(), cmd, arg)
        .map(|result| result as isize)
        .inspect_err(|err| {
            if *err == KError::NotATty {
                // TIOCGWINSZ / TCGETS on non-terminal fds are normal
                // (isatty() calls TCGETS to check if fd is a terminal)
                if cmd == TIOCGWINSZ || cmd == TCGETS {
                    return;
                }
                // Many programs probe for optional features (e.g. btrfs reflink
                // ioctls with magic 0x94) by issuing the ioctl and falling back
                // when it returns ENOTTY. That fallback path is expected, so log
                // at debug rather than warning on every probe.
                debug!("Unsupported ioctl command: {cmd} for fd: {fd}");
            }
        })
}

unittest::path_mod_test!(tests, "../ut-tests/ioctl.rs");
