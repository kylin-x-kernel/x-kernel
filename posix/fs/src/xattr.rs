// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux extended-attribute syscalls.

use alloc::vec::Vec;
use core::ffi::c_char;

use kcred::Cred;
use kerrno::{KError, KResult};
use kvfs::{
    Path, VfsResult, XATTR_NAME_MAX, XattrName, XattrNameRef, XattrNameSink, XattrSetFlags,
};
use linux_raw_sys::general::{
    AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, XATTR_LIST_MAX, XATTR_SIZE_MAX,
};
use posix_types::{UserConstPtr, UserPtr};

use crate::path::resolve_at_with_cred;

fn load_xattr_name(name: UserConstPtr<c_char>) -> KResult<XattrName> {
    XattrName::new(name.load_bytes_with_max_len(XATTR_NAME_MAX)?)
}

fn resolve_path(path: UserConstPtr<c_char>, lookup_flags: u32, cred: &Cred) -> KResult<Path> {
    let path = path.load_string()?;
    resolve_at_with_cred(AT_FDCWD, Some(&path), lookup_flags, cred)?.into_path()
}

fn resolve_fd(fd: i32, cred: &Cred) -> KResult<Path> {
    resolve_at_with_cred(fd, None, AT_EMPTY_PATH, cred)?.into_path()
}

fn load_xattr_value(value: UserConstPtr<u8>, size: usize) -> KResult<Vec<u8>> {
    if size > XATTR_SIZE_MAX as usize {
        return Err(KError::ArgumentListTooLong);
    }
    if size == 0 {
        Ok(Vec::new())
    } else {
        Ok(value.load_vm_vec(size)?)
    }
}

fn copy_xattr_value(value: &[u8], output: UserPtr<u8>, size: usize) -> KResult<isize> {
    if value.len() > XATTR_SIZE_MAX as usize {
        return Err(KError::ArgumentListTooLong);
    }
    if size == 0 {
        return Ok(value.len() as isize);
    }
    let size = size.min(XATTR_SIZE_MAX as usize);
    if value.len() > size {
        return Err(KError::OutOfRange);
    }
    if !value.is_empty() {
        output.write_vm_slice(value)?;
    }
    Ok(value.len() as isize)
}

struct XattrListWriter {
    bytes: Vec<u8>,
    output_capacity: Option<usize>,
    required_len: usize,
    is_too_large: bool,
    is_too_small: bool,
}

impl XattrListWriter {
    fn new(size: usize) -> Self {
        Self {
            bytes: Vec::new(),
            output_capacity: (size != 0).then_some(size.min(XATTR_LIST_MAX as usize)),
            required_len: 0,
            is_too_large: false,
            is_too_small: false,
        }
    }

    fn copy_to_user(self, output: UserPtr<u8>) -> KResult<isize> {
        if self.is_too_large {
            return Err(KError::ArgumentListTooLong);
        }
        if self.output_capacity.is_none() {
            return Ok(self.required_len as isize);
        }
        if self.is_too_small {
            return Err(KError::OutOfRange);
        }
        if !self.bytes.is_empty() {
            output.write_vm_slice(&self.bytes)?;
        }
        Ok(self.required_len as isize)
    }
}

impl XattrNameSink for XattrListWriter {
    fn emit(&mut self, name: XattrNameRef<'_>) -> VfsResult<()> {
        if self.is_too_large {
            return Ok(());
        }
        let entry_len = name
            .encoded_len()
            .checked_add(1)
            .ok_or(KError::ArgumentListTooLong)?;
        let required_len = self
            .required_len
            .checked_add(entry_len)
            .ok_or(KError::ArgumentListTooLong)?;
        if required_len > XATTR_LIST_MAX as usize {
            self.is_too_large = true;
            return Ok(());
        }
        self.required_len = required_len;

        let Some(capacity) = self.output_capacity else {
            return Ok(());
        };
        if self.is_too_small || required_len > capacity {
            self.is_too_small = true;
            return Ok(());
        }
        name.append_to(&mut self.bytes);
        self.bytes.push(0);
        Ok(())
    }
}

fn set_xattr(
    path: Path,
    name: UserConstPtr<c_char>,
    value: UserConstPtr<u8>,
    size: usize,
    flags: u32,
    cred: &Cred,
) -> KResult<isize> {
    let flags = XattrSetFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    let name = load_xattr_name(name)?;
    let value = load_xattr_value(value, size)?;
    path.set_xattr(&name, &value, flags, cred)?;
    Ok(0)
}

fn get_xattr(
    path: Path,
    name: UserConstPtr<c_char>,
    output: UserPtr<u8>,
    size: usize,
    cred: &Cred,
) -> KResult<isize> {
    let name = load_xattr_name(name)?;
    let value = path.get_xattr(&name, cred)?;
    copy_xattr_value(&value, output, size)
}

fn list_xattrs(path: Path, list: UserPtr<u8>, size: usize, cred: &Cred) -> KResult<isize> {
    let mut writer = XattrListWriter::new(size);
    path.list_xattrs(cred, &mut writer)?;
    writer.copy_to_user(list)
}

fn remove_xattr(path: Path, name: UserConstPtr<c_char>, cred: &Cred) -> KResult<isize> {
    let name = load_xattr_name(name)?;
    path.remove_xattr(&name, cred)?;
    Ok(0)
}

/// Sets an xattr on the target of a pathname.
pub fn sys_setxattr(
    path: UserConstPtr<c_char>,
    name: UserConstPtr<c_char>,
    value: UserConstPtr<u8>,
    size: usize,
    flags: u32,
) -> KResult<isize> {
    let cred = kprocess::current_cred();
    set_xattr(
        resolve_path(path, 0, &cred)?,
        name,
        value,
        size,
        flags,
        &cred,
    )
}

/// Sets an xattr on a pathname without following the final symbolic link.
pub fn sys_lsetxattr(
    path: UserConstPtr<c_char>,
    name: UserConstPtr<c_char>,
    value: UserConstPtr<u8>,
    size: usize,
    flags: u32,
) -> KResult<isize> {
    let cred = kprocess::current_cred();
    set_xattr(
        resolve_path(path, AT_SYMLINK_NOFOLLOW, &cred)?,
        name,
        value,
        size,
        flags,
        &cred,
    )
}

/// Sets an xattr on an open file description.
pub fn sys_fsetxattr(
    fd: i32,
    name: UserConstPtr<c_char>,
    value: UserConstPtr<u8>,
    size: usize,
    flags: u32,
) -> KResult<isize> {
    let cred = kprocess::current_cred();
    set_xattr(resolve_fd(fd, &cred)?, name, value, size, flags, &cred)
}

/// Reads an xattr from the target of a pathname.
pub fn sys_getxattr(
    path: UserConstPtr<c_char>,
    name: UserConstPtr<c_char>,
    value: UserPtr<u8>,
    size: usize,
) -> KResult<isize> {
    let cred = kprocess::current_cred();
    get_xattr(resolve_path(path, 0, &cred)?, name, value, size, &cred)
}

/// Reads an xattr from a pathname without following the final symbolic link.
pub fn sys_lgetxattr(
    path: UserConstPtr<c_char>,
    name: UserConstPtr<c_char>,
    value: UserPtr<u8>,
    size: usize,
) -> KResult<isize> {
    let cred = kprocess::current_cred();
    get_xattr(
        resolve_path(path, AT_SYMLINK_NOFOLLOW, &cred)?,
        name,
        value,
        size,
        &cred,
    )
}

/// Reads an xattr from an open file description.
pub fn sys_fgetxattr(
    fd: i32,
    name: UserConstPtr<c_char>,
    value: UserPtr<u8>,
    size: usize,
) -> KResult<isize> {
    let cred = kprocess::current_cred();
    get_xattr(resolve_fd(fd, &cred)?, name, value, size, &cred)
}

/// Lists xattrs on the target of a pathname.
pub fn sys_listxattr(path: UserConstPtr<c_char>, list: UserPtr<u8>, size: usize) -> KResult<isize> {
    let cred = kprocess::current_cred();
    list_xattrs(resolve_path(path, 0, &cred)?, list, size, &cred)
}

/// Lists xattrs without following the final symbolic link.
pub fn sys_llistxattr(
    path: UserConstPtr<c_char>,
    list: UserPtr<u8>,
    size: usize,
) -> KResult<isize> {
    let cred = kprocess::current_cred();
    list_xattrs(
        resolve_path(path, AT_SYMLINK_NOFOLLOW, &cred)?,
        list,
        size,
        &cred,
    )
}

/// Lists xattrs on an open file description.
pub fn sys_flistxattr(fd: i32, list: UserPtr<u8>, size: usize) -> KResult<isize> {
    let cred = kprocess::current_cred();
    list_xattrs(resolve_fd(fd, &cred)?, list, size, &cred)
}

/// Removes an xattr from the target of a pathname.
pub fn sys_removexattr(path: UserConstPtr<c_char>, name: UserConstPtr<c_char>) -> KResult<isize> {
    let cred = kprocess::current_cred();
    remove_xattr(resolve_path(path, 0, &cred)?, name, &cred)
}

/// Removes an xattr without following the final symbolic link.
pub fn sys_lremovexattr(path: UserConstPtr<c_char>, name: UserConstPtr<c_char>) -> KResult<isize> {
    let cred = kprocess::current_cred();
    remove_xattr(resolve_path(path, AT_SYMLINK_NOFOLLOW, &cred)?, name, &cred)
}

/// Removes an xattr from an open file description.
pub fn sys_fremovexattr(fd: i32, name: UserConstPtr<c_char>) -> KResult<isize> {
    let cred = kprocess::current_cred();
    remove_xattr(resolve_fd(fd, &cred)?, name, &cred)
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn xattr_list_is_nul_separated_and_bounded() {
        let mut writer = XattrListWriter::new(64);
        writer
            .emit(XattrNameRef::new(b"user.alpha").unwrap())
            .unwrap();
        writer
            .emit(XattrNameRef::new(b"security.beta").unwrap())
            .unwrap();
        assert_eq!(writer.bytes, b"user.alpha\0security.beta\0");
    }

    #[def_test]
    fn xattr_list_size_query_does_not_materialize_names() {
        let mut writer = XattrListWriter::new(0);
        writer
            .emit(XattrNameRef::new(b"user.alpha").unwrap())
            .unwrap();

        assert_eq!(writer.required_len, b"user.alpha\0".len());
        assert!(writer.bytes.is_empty());
    }
}
