// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File descriptor operations.
//!
//! This module implements file descriptor manipulation syscalls including:
//! - Closing files (close, close_range, etc.)
//! - File descriptor duplication (dup, dup2, dup3, etc.)
//! - File descriptor flags and control (fcntl, etc.)

use core::ffi::c_int;

use bitflags::bitflags;
use kerrno::{KError, KResult, LinuxError};
use kvfs::{FMode, FileLock, FileLockOwner, FileLockType, OpenFlags, VfsFile, pipe::PipeObject};
use linux_raw_sys::general::*;
use memfs::shmem;
use posix_types::{UserPtr, fs::k_flock64};

const SETFL_MASK: OpenFlags = OpenFlags::APPEND
    .union(OpenFlags::NONBLOCK)
    .union(OpenFlags::DIRECT)
    .union(OpenFlags::NO_ATIME);

/// Closes the specified file descriptor.
pub fn sys_close(fd: c_int) -> KResult<isize> {
    debug!("sys_close <= {fd}");
    kprocess::current_resources().close_file(fd)?;
    Ok(0)
}

bitflags! {
    #[derive(Debug, Clone, Copy)]
    struct CloseRangeFlags: u32 {
        const UNSHARE = 1 << 1;
        const CLOEXEC = 1 << 2;
    }
}

/// Closes a range of file descriptors.
pub fn sys_close_range(first: i32, last: i32, flags: u32) -> KResult<isize> {
    if first < 0 || last < first {
        return Err(KError::InvalidInput);
    }
    let flags = CloseRangeFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    debug!("sys_close_range <= fds: [{first}, {last}], flags: {flags:?}");

    let resources = kprocess::current_user_process().resources()?;
    if flags.contains(CloseRangeFlags::UNSHARE) {
        resources.unshare_fd_table()?;
    }

    if flags.contains(CloseRangeFlags::CLOEXEC) {
        resources.set_cloexec_range(first, last)?;
    } else {
        resources.close_range(first, last)?;
    }

    Ok(0)
}

/// Duplicates a file descriptor and optionally sets `CLOEXEC`.
fn dup_fd(old_fd: c_int, cloexec: bool) -> KResult<isize> {
    let resources = kprocess::current_user_process().resources()?;
    let new_fd = resources.duplicate_file(old_fd, cloexec)?;
    Ok(new_fd as _)
}

fn seal_bits_from_fcntl_arg(arg: usize) -> KResult<u32> {
    u32::try_from(arg).map_err(|_| KError::InvalidInput)
}

/// Duplicates a file descriptor.
pub fn sys_dup(old_fd: c_int) -> KResult<isize> {
    debug!("sys_dup <= {old_fd}");
    dup_fd(old_fd, false)
}

/// Converts the fcntl unsigned-long argument to Linux's signed int minimum.
fn dupfd_minimum_from_arg(arg: usize) -> c_int {
    // Linux do_fcntl truncates to int before passing the minimum to f_dupfd.
    (arg as u32) as c_int
}

fn dup_fd_from_minimum(old_fd: c_int, arg: usize, cloexec: bool) -> KResult<isize> {
    let resources = kprocess::current_user_process().resources()?;
    resources
        .duplicate_file_from(old_fd, dupfd_minimum_from_arg(arg), cloexec)
        .map(|fd| fd as _)
}

#[cfg(target_arch = "x86_64")]
/// Duplicates a file descriptor to a specific target fd.
pub fn sys_dup2(old_fd: c_int, new_fd: c_int) -> KResult<isize> {
    if old_fd == new_fd {
        kprocess::current_resources().get_file(new_fd)?;
        return Ok(new_fd as _);
    }
    sys_dup3(old_fd, new_fd, 0)
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Dup3Flags: c_int {
        const O_CLOEXEC = O_CLOEXEC as _; // Close on exec
    }
}

/// Duplicates a file descriptor with additional flags.
pub fn sys_dup3(old_fd: c_int, new_fd: c_int, flags: c_int) -> KResult<isize> {
    let flags = Dup3Flags::from_bits(flags).ok_or(KError::InvalidInput)?;
    debug!("sys_dup3 <= old_fd: {old_fd}, new_fd: {new_fd}, flags: {flags:?}");

    if old_fd == new_fd {
        return Err(KError::InvalidInput);
    }

    kprocess::current_user_process()
        .resources()?
        .duplicate_file_to(old_fd, new_fd, flags.contains(Dup3Flags::O_CLOEXEC))
        .map(|fd| fd as _)
}

/// Performs file descriptor control operations.
pub fn sys_fcntl(fd: c_int, cmd: c_int, arg: usize) -> KResult<isize> {
    debug!("sys_fcntl <= fd: {fd} cmd: {cmd} arg: {arg}");

    match cmd as u32 {
        F_DUPFD => dup_fd_from_minimum(fd, arg, false),
        F_DUPFD_CLOEXEC => dup_fd_from_minimum(fd, arg, true),
        F_SETLK | F_SETLKW | F_OFD_SETLK | F_OFD_SETLKW | F_GETLK | F_OFD_GETLK => {
            fcntl_lock(fd, cmd as u32, arg)
        }
        F_SETFL => {
            let mut flags = u32::try_from(arg).map_err(|_| KError::InvalidInput)?;
            if O_NONBLOCK != O_NDELAY && flags & O_NDELAY != 0 {
                flags |= O_NONBLOCK;
            }
            kprocess::current_resources()
                .get_file(fd)?
                .replace_flags(SETFL_MASK, OpenFlags::from_bits_retain(flags));
            Ok(0)
        }
        F_GETFL => Ok(kprocess::current_resources().get_file(fd)?.flags().bits() as _),
        F_GETFD => {
            let cloexec = kprocess::current_user_process().resources()?.cloexec(fd)?;
            Ok(if cloexec { FD_CLOEXEC as _ } else { 0 })
        }
        F_SETFD => {
            let cloexec = arg & FD_CLOEXEC as usize != 0;
            kprocess::current_user_process()
                .resources()?
                .set_cloexec(fd, cloexec)?;
            Ok(0)
        }
        F_GETPIPE_SZ => {
            let file = kprocess::current_resources().get_file(fd)?;
            let pipe = PipeObject::from_file(&file)?;
            Ok(pipe.capacity() as _)
        }
        F_SETPIPE_SZ => {
            let file = kprocess::current_resources().get_file(fd)?;
            let pipe = PipeObject::from_file(&file)?;
            pipe.resize(arg)?;
            Ok(pipe.capacity() as _)
        }
        F_GET_SEALS => {
            let file = kprocess::current_resources().get_file(fd)?;
            Ok(shmem::seal_bits_for_location(file.path())? as _)
        }
        F_ADD_SEALS => {
            let file = kprocess::current_resources().get_file(fd)?;
            file.verify_mode(FMode::WRITE)?;
            shmem::add_seals_for_location(file.path(), seal_bits_from_fcntl_arg(arg)?)?;
            Ok(0)
        }
        _ => {
            warn!("unsupported fcntl parameters: cmd: {cmd}");
            Ok(0)
        }
    }
}

/// Applies or removes an advisory lock on a file descriptor.
pub fn sys_flock(fd: c_int, operation: c_int) -> KResult<isize> {
    debug!("flock <= fd: {fd}, operation: {operation}");
    // Linux ignores the obsolete mandatory-flock extension before fd lookup.
    if operation as u32 & LOCK_MAND != 0 {
        return Ok(0);
    }
    let kind = match operation as u32 & !LOCK_NB {
        LOCK_SH => FileLockType::Read,
        LOCK_EX => FileLockType::Write,
        LOCK_UN => FileLockType::Unlock,
        _ => return Err(KError::InvalidInput),
    };
    let file = kprocess::current_resources().get_file(fd)?;
    if file.is_path()
        || (kind != FileLockType::Unlock && !file.mode().intersects(FMode::READ | FMode::WRITE))
    {
        return Err(KError::BadFileDescriptor);
    }
    let request = FileLock::new(FileLockOwner::flock(&file), current_lock_pid(), kind, 0, 0)?;
    file.set_lock(&request, operation as u32 & LOCK_NB == 0)?;
    Ok(0)
}

fn current_lock_pid() -> i32 {
    kprocess::current_user_process().pid() as i32
}

fn lock_request(file: &VfsFile, owner: FileLockOwner, lock: &k_flock64) -> KResult<FileLock> {
    let kind = match lock.l_type as u32 {
        F_RDLCK => FileLockType::Read,
        F_WRLCK => FileLockType::Write,
        F_UNLCK => FileLockType::Unlock,
        _ => return Err(KError::InvalidInput),
    };
    let base = match lock.l_whence as u32 {
        SEEK_SET => 0,
        SEEK_CUR => file.position(),
        SEEK_END => file.inode().size(),
        _ => return Err(KError::InvalidInput),
    };
    let base = i64::try_from(base).map_err(|_| KError::from(LinuxError::EOVERFLOW))?;
    let start = base
        .checked_add(lock.l_start)
        .ok_or(KError::from(LinuxError::EOVERFLOW))?;
    FileLock::new(owner, current_lock_pid(), kind, start, lock.l_len)
}

fn fcntl_lock(fd: c_int, cmd: u32, arg: usize) -> KResult<isize> {
    let resources = kprocess::current_resources();
    let mut table = resources.fd_table()?;
    let (file, files_owner) = {
        let table = table.read();
        (table.get_file(fd)?, table.lock_owner())
    };
    if file.is_path() {
        return Err(KError::BadFileDescriptor);
    }
    let pointer = UserPtr::<k_flock64>::from(arg);
    let mut lock = pointer.read_vm()?;
    // Traditional GETLK rejects unlock before range conversion; OFD GETLK
    // permits it to query this open description's own locks.
    if cmd == F_GETLK && !matches!(lock.l_type as u32, F_RDLCK | F_WRLCK) {
        return Err(KError::InvalidInput);
    }
    let is_ofd = matches!(cmd, F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW);
    let owner = if is_ofd {
        FileLockOwner::ofd(&file)
    } else {
        files_owner
    };
    let mut request = lock_request(&file, owner, &lock)?;
    if matches!(cmd, F_GETLK | F_OFD_GETLK) {
        if is_ofd && lock.l_pid != 0 {
            return Err(KError::InvalidInput);
        }
        if let Some(conflict) = file.test_lock(&request)? {
            lock.l_type = match conflict.kind() {
                FileLockType::Read => F_RDLCK as _,
                FileLockType::Write => F_WRLCK as _,
                FileLockType::Unlock => F_UNLCK as _,
            };
            lock.l_whence = SEEK_SET as _;
            lock.l_start = conflict.start();
            lock.l_len = conflict.length();
            lock.l_pid = conflict.pid();
        } else {
            lock.l_type = F_UNLCK as _;
        }
        pointer.write_vm(lock)?;
    } else {
        match request.kind() {
            FileLockType::Read => file.verify_mode(FMode::READ)?,
            FileLockType::Write => file.verify_mode(FMode::WRITE)?,
            FileLockType::Unlock => {}
        }
        if is_ofd && lock.l_pid != 0 {
            return Err(KError::InvalidInput);
        }
        loop {
            file.set_lock(&request, matches!(cmd, F_SETLKW | F_OFD_SETLKW))?;
            if is_ofd {
                break;
            }
            // Main still has a process-wide files binding. Validate both the
            // binding and descriptor after waiting; a replaced table must not
            // turn an installation on an obsolete owner into success.
            let installed = resources.with_fd_table(|current| {
                let descriptors = current.read();
                let is_current = core::ptr::eq(current, table.as_ref());
                let is_open = descriptors
                    .get_file(fd)
                    .is_ok_and(|entry| alloc::sync::Arc::ptr_eq(&entry, &file));
                if !is_open {
                    return Err(KError::BadFileDescriptor);
                }
                if !is_current && request.kind() != FileLockType::Unlock {
                    file.remove_posix_locks(request.owner());
                }
                Ok(is_current)
            });
            match installed {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => {
                    if request.kind() != FileLockType::Unlock {
                        file.remove_posix_locks(request.owner());
                    }
                    return Err(error);
                }
            }
            // Preserve the original absolute range across retries (SEEK_CUR
            // and SEEK_END must not be re-evaluated after a blocking wait).
            table = resources.fd_table()?;
            let owner = table.read().lock_owner();
            request = FileLock::new(
                owner,
                request.pid(),
                request.kind(),
                request.start(),
                request.length(),
            )?;
        }
    }
    Ok(0)
}

#[unittest::mod_test]
mod tests {
    use unittest::def_test;

    use super::dupfd_minimum_from_arg;

    #[def_test]
    fn test_dupfd_minimum_uses_signed_low_32_bits() {
        assert_eq!(dupfd_minimum_from_arg(64), 64);
        assert_eq!(dupfd_minimum_from_arg(u32::MAX as usize), -1);
        assert_eq!(dupfd_minimum_from_arg(0x8000_0000), i32::MIN);
    }

    #[def_test]
    fn test_dupfd_minimum_ignores_high_argument_bits() {
        if usize::BITS > 32 {
            let high = 1_u64 << 32;
            assert_eq!(dupfd_minimum_from_arg((high | 64) as usize), 64);
            assert_eq!(
                dupfd_minimum_from_arg((high | u32::MAX as u64) as usize),
                -1
            );
        }
    }
}
