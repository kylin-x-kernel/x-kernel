// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory file descriptor syscalls.

use alloc::format;
use core::ffi::c_char;

use kerrno::{KError, KResult};
use kfs::{FS_CONTEXT, OpenOptions};
use kservices::file::{File, FileLike};
use linux_raw_sys::general::{MFD_CLOEXEC, O_RDWR};
use posix_types::UserConstPtr;

// TODO: correct memfd implementation

/// Creates an anonymous in-memory file descriptor.
pub fn sys_memfd_create(_name: UserConstPtr<c_char>, flags: u32) -> KResult<isize> {
    // This is cursed
    for id in 0..0xffff {
        let name = format!("/tmp/memfd-{id:04x}");
        let fs = FS_CONTEXT.lock().clone();
        if fs.resolve(&name).is_err() {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&fs, &name)?
                .into_file()?;
            let cloexec = flags & MFD_CLOEXEC != 0;
            return File::new(file, O_RDWR)
                .add_to_fd_table(cloexec)
                .map(|fd| fd as _);
        }
    }
    Err(KError::TooManyOpenFiles)
}
