// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File control and manipulation syscalls.
//!
//! This module implements file control operations including:
//! - I/O control (ioctl, etc.)
//! - File control (fcntl, etc.)
//! - File attribute manipulation (chmod, chown, utime, etc.)
//! - Directory operations (mkdir, rmdir, rename, etc.)
//! - Symbolic links (symlink, readlink, etc.)
//! - File removal (unlink, unlinkat, etc.)

use alloc::{ffi::CString, vec, vec::Vec};
use core::{
    ffi::{c_char, c_int, c_long},
    mem::offset_of,
    time::Duration,
};

use fs_ng_vfs::{MetadataUpdate, NodePermission, NodeType, path::Path};
use kerrno::{KError, KResult};
use kfs::FsContext;
use khal::time::wall_time;
use kservices::file::Directory;
use linux_raw_sys::{
    general::*,
    ioctl::{FIONBIO, TIOCGWINSZ},
};
use osvm::VirtPtr;
use posix_types::{TimeValueLike, UserConstPtr, UserPtr};

use crate::path::{resolve_at, with_fs};
/// The ioctl() system call manipulates the underlying device parameters
/// of special files.
pub fn sys_ioctl(fd: i32, cmd: u32, arg: usize) -> KResult<isize> {
    debug!("sys_ioctl <= fd: {fd}, cmd: {cmd}, arg: {arg}");
    let f = kthread::current_resources().get_file_like(fd)?;
    if cmd == FIONBIO {
        let val = (arg as *const u8).read_vm()?;
        if val != 0 && val != 1 {
            return Err(KError::InvalidInput);
        }
        f.set_nonblocking(val != 0)?;
        return Ok(0);
    }
    f.ioctl(cmd, arg)
        .map(|result| result as isize)
        .inspect_err(|err| {
            if *err == KError::NotATty {
                // glibc likes to call TIOCGWINSZ on non-terminal files, just
                // ignore it
                if cmd == TIOCGWINSZ {
                    return;
                }
                warn!("Unsupported ioctl command: {cmd} for fd: {fd}");
            }
        })
}

/// Changes the current working directory.
pub fn sys_chdir(path: UserConstPtr<c_char>) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_chdir <= path: {path}");

    let proc_state = kthread::current_process_state();
    let mut fs = proc_state.fs_context().lock();
    let entry = fs.resolve(path)?;
    fs.set_current_dir(entry)?;
    Ok(0)
}

/// Changes the current working directory by file descriptor.
pub fn sys_fchdir(dirfd: i32) -> KResult<isize> {
    debug!("sys_fchdir <= dirfd: {dirfd}");

    let entry = with_fs(dirfd, |fs| Ok(fs.current_dir().clone()))?;
    kthread::current_process_state()
        .fs_context()
        .lock()
        .set_current_dir(entry)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_mkdir(path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    sys_mkdirat(AT_FDCWD, path, mode)
}

/// Changes the root directory of the calling process.
pub fn sys_chroot(path: UserConstPtr<c_char>) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_chroot <= path: {path}");

    let proc_state = kthread::current_process_state();
    let mut fs = proc_state.fs_context().lock();
    let loc = fs.resolve(path)?;
    if loc.node_type() != NodeType::Directory {
        return Err(KError::NotADirectory);
    }
    *fs = FsContext::new(loc);
    Ok(0)
}

/// Creates a directory relative to a directory file descriptor.
pub fn sys_mkdirat(dirfd: i32, path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_mkdirat <= dirfd: {dirfd}, path: {path}, mode: {mode}");

    let mode = mode & !kthread::current_thread().process_state().umask();
    let mode = NodePermission::from_bits_truncate(mode as u16);

    with_fs(dirfd, |fs| match fs.create_dir(&path, mode) {
        Ok(_) => Ok(0),
        // mkdir on an existing path should report EEXIST.
        // Use no-follow lookup so dangling symlinks are treated as existing
        // entries, and avoid converting empty-path invalid input.
        Err(KError::InvalidInput) if !path.is_empty() && fs.resolve_no_follow(&path).is_ok() => {
            Err(KError::AlreadyExists)
        }
        Err(err) => Err(err),
    })
}

// Directory buffer for getdents64 syscall
struct DirBuffer {
    buf: Vec<u8>,
    offset: usize,
}

impl DirBuffer {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0; len],
            offset: 0,
        }
    }

    fn remaining_space(&self) -> usize {
        self.buf.len().saturating_sub(self.offset)
    }

    fn write_entry(&mut self, d_ino: u64, d_off: i64, d_type: NodeType, name: &[u8]) -> bool {
        const NAME_OFFSET: usize = offset_of!(linux_dirent64, d_name);

        let len = NAME_OFFSET + name.len() + 1;
        // alignment
        let len = len.next_multiple_of(align_of::<linux_dirent64>());
        if self.remaining_space() < len {
            return false;
        }

        // FIXME: safety
        unsafe {
            let entry_ptr = self.buf.as_mut_ptr().add(self.offset);
            entry_ptr.cast::<linux_dirent64>().write(linux_dirent64 {
                d_ino,
                d_off,
                d_reclen: len as _,
                d_type: d_type as _,
                d_name: Default::default(),
            });

            let name_ptr = entry_ptr.add(NAME_OFFSET);
            name_ptr.copy_from_nonoverlapping(name.as_ptr(), name.len());
            name_ptr.add(name.len()).write(0);
        }

        self.offset += len;
        true
    }
}

/// Reads directory entries in linux_dirent64 format.
pub fn sys_getdents64(fd: i32, buf: UserPtr<u8>, len: usize) -> KResult<isize> {
    debug!(
        "sys_getdents64 <= fd: {fd}, buf: {:?}, len: {len}",
        buf.as_ptr()
    );

    let mut buffer = DirBuffer::new(len);

    let dir = kthread::current_resources().get_file_like_as::<Directory>(fd)?;
    let mut dir_offset = dir.offset.lock();

    let mut has_remaining = false;

    dir.inner()
        .read_dir(*dir_offset, &mut |name: &str, ino, node_type, offset| {
            has_remaining = true;
            if !buffer.write_entry(ino, offset as _, node_type, name.as_bytes()) {
                return false;
            }
            *dir_offset = offset;
            true
        })?;

    if has_remaining && buffer.offset == 0 {
        return Err(KError::InvalidInput);
    }

    buf.write_vm_slice(&buffer.buf)?;

    Ok(buffer.offset as _)
}

/// create a link from new_path to old_path
/// old_path: old file path
/// new_path: new file path
/// flags: link flags
/// return value: return 0 when success, else return -1.
/// Creates a hard link to an existing file.
pub fn sys_linkat(
    old_dirfd: c_int,
    old_path: UserConstPtr<c_char>,
    new_dirfd: c_int,
    new_path: UserConstPtr<c_char>,
    flags: u32,
) -> KResult<isize> {
    let old_path = old_path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let new_path = new_path.load_string()?;
    debug!(
        "sys_linkat <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path}, flags: {flags}"
    );

    if flags != 0 {
        warn!("Unsupported flags: {flags}");
    }

    let old = resolve_at(old_dirfd, old_path.as_deref(), flags)?
        .into_file()
        .ok_or(KError::BadFileDescriptor)?;
    if old.is_dir() {
        return Err(KError::OperationNotPermitted);
    }
    let (new_dir, new_name) =
        with_fs(new_dirfd, |fs| fs.resolve_nonexistent(Path::new(&new_path)))?;

    new_dir.link(new_name, &old)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_link(old_path: UserConstPtr<c_char>, new_path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_linkat(AT_FDCWD, old_path, AT_FDCWD, new_path, 0)
}

/// remove link of specific file (can be used to delete file)
/// dir_fd: the directory of link to be removed
/// path: the name of link to be removed
/// flags: can be 0 or AT_REMOVEDIR
/// return 0 when success, else return -1
/// Removes a directory entry (file or directory).
pub fn sys_unlinkat(dirfd: i32, path: UserConstPtr<c_char>, flags: usize) -> KResult<isize> {
    let path = path.load_string()?;

    debug!("sys_unlinkat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    with_fs(dirfd, |fs| {
        if flags == AT_REMOVEDIR as usize {
            fs.remove_dir(path)?;
        } else {
            fs.remove_file(path)?;
        }
        Ok(0)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_rmdir(path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_unlinkat(AT_FDCWD, path, AT_REMOVEDIR as usize)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_unlink(path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_unlinkat(AT_FDCWD, path, 0)
}

/// Gets the current working directory path.
pub fn sys_getcwd(buf: UserPtr<u8>, size: isize) -> KResult<isize> {
    let size: usize = size.try_into().map_err(|_| KError::BadAddress)?;
    if buf.is_null() {
        return Ok(0);
    }

    let cwd = kthread::current_process_state()
        .fs_context()
        .lock()
        .current_dir()
        .absolute_path()?;
    debug!("sys_getcwd => cwd: {cwd}");

    let cwd = CString::new(cwd.as_str()).map_err(|_| KError::InvalidInput)?;
    let cwd = cwd.as_bytes_with_nul();

    if cwd.len() <= size {
        buf.write_vm_slice(cwd)?;
        // FIXME: it is said that this should return 0
        Ok(buf.as_ptr() as _)
    } else {
        Err(KError::OutOfRange)
    }
}

#[cfg(target_arch = "x86_64")]
pub fn sys_symlink(target: UserConstPtr<c_char>, linkpath: UserConstPtr<c_char>) -> KResult<isize> {
    sys_symlinkat(target, AT_FDCWD, linkpath)
}

/// Creates a symbolic link relative to a directory file descriptor.
pub fn sys_symlinkat(
    target: UserConstPtr<c_char>,
    new_dirfd: i32,
    linkpath: UserConstPtr<c_char>,
) -> KResult<isize> {
    let target = target.load_string()?;
    let linkpath = linkpath.load_string()?;
    debug!("sys_symlinkat <= target: {target:?}, new_dirfd: {new_dirfd}, linkpath: {linkpath:?}");

    with_fs(new_dirfd, |fs| {
        fs.symlink(target, linkpath)?;
        Ok(0)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_readlink(path: UserConstPtr<c_char>, buf: UserPtr<u8>, size: usize) -> KResult<isize> {
    sys_readlinkat(AT_FDCWD, path, buf, size)
}

/// Reads the target of a symbolic link.
pub fn sys_readlinkat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    buf: UserPtr<u8>,
    size: usize,
) -> KResult<isize> {
    let path = path.load_string()?;

    debug!("sys_readlinkat <= dirfd: {dirfd}, path: {path:?}");

    with_fs(dirfd, |fs| {
        let entry = fs.resolve_no_follow(path)?;
        let link = entry.read_link()?;
        let read = size.min(link.len());
        buf.write_vm_slice(&link.as_bytes()[..read])?;
        Ok(read as isize)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_chown(path: UserConstPtr<c_char>, uid: i32, gid: i32) -> KResult<isize> {
    sys_fchownat(AT_FDCWD, path, uid, gid, 0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_lchown(path: UserConstPtr<c_char>, uid: i32, gid: i32) -> KResult<isize> {
    use linux_raw_sys::general::AT_SYMLINK_NOFOLLOW;
    sys_fchownat(AT_FDCWD, path, uid, gid, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_fchown(fd: i32, uid: i32, gid: i32) -> KResult<isize> {
    sys_fchownat(fd, UserConstPtr::default(), uid, gid, AT_EMPTY_PATH)
}

/// Changes file ownership relative to a directory file descriptor.
pub fn sys_fchownat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    uid: i32,
    gid: i32,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let loc = resolve_at(dirfd, path.as_deref(), flags)?
        .into_file()
        .ok_or(KError::BadFileDescriptor)?;
    let meta = loc.metadata()?;

    let mut mode = meta.mode;
    // chown always clears the setuid bits
    mode.remove(NodePermission::SET_UID);
    // chown also removes the setgid bits if group-executable
    if mode.contains(NodePermission::GROUP_EXEC) {
        mode.remove(NodePermission::SET_GID);
    }

    let uid = if uid == -1 { meta.uid } else { uid as _ };
    let gid = if gid == -1 { meta.gid } else { gid as _ };
    loc.update_metadata(MetadataUpdate {
        owner: Some((uid, gid)),
        mode: Some(mode),
        ..Default::default()
    })?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_chmod(path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    sys_fchmodat(AT_FDCWD, path, mode, 0)
}

/// Changes file permissions by file descriptor.
pub fn sys_fchmod(fd: i32, mode: u32) -> KResult<isize> {
    sys_fchmodat(fd, UserConstPtr::default(), mode, AT_EMPTY_PATH)
}

/// Changes file permissions relative to a directory file descriptor.
pub fn sys_fchmodat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    mode: u32,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    resolve_at(dirfd, path.as_deref(), flags)?
        .into_file()
        .ok_or(KError::BadFileDescriptor)?
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(mode as u16)),
            ..Default::default()
        })?;
    Ok(0)
}

fn update_times(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    atime: Option<Duration>,
    mtime: Option<Duration>,
    flags: u32,
) -> KResult<()> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    resolve_at(dirfd, path.as_deref(), flags)?
        .into_file()
        .ok_or(KError::BadFileDescriptor)?
        .update_metadata(MetadataUpdate {
            atime,
            mtime,
            ..Default::default()
        })?;
    Ok(())
}

#[cfg(target_arch = "x86_64")]
#[allow(non_camel_case_types)]
#[repr(C)]
pub struct utimbuf {
    actime: linux_raw_sys::general::__kernel_old_time_t,
    modtime: linux_raw_sys::general::__kernel_old_time_t,
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utime(path: UserConstPtr<c_char>, times: UserConstPtr<utimbuf>) -> KResult<isize> {
    let (atime, mtime) = if let Some(times) = times.check_non_null() {
        // FIXME: AnyBitPattern
        let times = unsafe { times.read_uninit()?.assume_init() };
        (
            Duration::from_secs(times.actime as _),
            Duration::from_secs(times.modtime as _),
        )
    } else {
        let time = wall_time();
        (time, time)
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), 0)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utimes(
    path: UserConstPtr<c_char>,
    times: UserConstPtr<[linux_raw_sys::general::timeval; 2]>,
) -> KResult<isize> {
    let (atime, mtime) = if let Some(times) = times.check_non_null() {
        // FIXME: AnyBitPattern
        let [atime, mtime] = unsafe { times.read_uninit()?.assume_init() };
        (atime.try_into_time_value()?, mtime.try_into_time_value()?)
    } else {
        let time = wall_time();
        (time, time)
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), 0)?;
    Ok(0)
}

pub fn sys_utimensat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    times: UserConstPtr<[timespec; 2]>,
    mut flags: u32,
) -> KResult<isize> {
    if path.is_null() {
        flags |= AT_EMPTY_PATH;
    }
    fn utime_to_duration(time: &timespec) -> Option<KResult<Duration>> {
        match time.tv_nsec {
            val if val == UTIME_OMIT as c_long => None,
            val if val == UTIME_NOW as c_long => Some(Ok(wall_time())),
            _ => Some(time.try_into_time_value()),
        }
    }

    let (atime, mtime) = if let Some(times) = times.check_non_null() {
        // FIXME: AnyBitPattern
        let [atime, mtime] = unsafe { times.read_uninit()?.assume_init() };
        (
            utime_to_duration(&atime).transpose()?,
            utime_to_duration(&mtime).transpose()?,
        )
    } else {
        let time = wall_time();
        (Some(time), Some(time))
    };
    if atime.is_none() && mtime.is_none() {
        return Ok(0);
    }

    update_times(dirfd, path, atime, mtime, flags)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_rename(
    old_path: UserConstPtr<c_char>,
    new_path: UserConstPtr<c_char>,
) -> KResult<isize> {
    sys_renameat(AT_FDCWD, old_path, AT_FDCWD, new_path)
}

pub fn sys_renameat(
    old_dirfd: i32,
    old_path: UserConstPtr<c_char>,
    new_dirfd: i32,
    new_path: UserConstPtr<c_char>,
) -> KResult<isize> {
    sys_renameat2(old_dirfd, old_path, new_dirfd, new_path, 0)
}

pub fn sys_renameat2(
    old_dirfd: i32,
    old_path: UserConstPtr<c_char>,
    new_dirfd: i32,
    new_path: UserConstPtr<c_char>,
    flags: u32,
) -> KResult<isize> {
    let old_path = old_path.load_string()?;
    let new_path = new_path.load_string()?;
    debug!(
        "sys_renameat2 <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path}, flags: {flags}"
    );

    let (old_dir, old_name) = with_fs(old_dirfd, |fs| fs.resolve_parent(Path::new(&old_path)))?;
    let (new_dir, new_name) =
        with_fs(new_dirfd, |fs| fs.resolve_nonexistent(Path::new(&new_path)))?;

    old_dir.rename(&old_name, &new_dir, new_name)?;
    Ok(0)
}

pub fn sys_sync() -> KResult<isize> {
    let root = kthread::current_process_state()
        .fs_context()
        .lock()
        .root_dir()
        .clone();
    root.filesystem().flush()?;
    Ok(0)
}

pub fn sys_syncfs(fd: i32) -> KResult<isize> {
    let file_like = kthread::current_resources().get_file_like(fd)?;

    if let Some(file) = file_like.downcast_ref::<kservices::file::File>() {
        file.inner().location().filesystem().flush()?;
        return Ok(0);
    }

    if let Some(dir) = file_like.downcast_ref::<Directory>() {
        dir.inner().filesystem().flush()?;
        return Ok(0);
    }

    Err(KError::InvalidInput)
}
