//! Memory file descriptor syscalls.
//!
//! This module implements memory file operations including:
//! - Memory file creation (memfd_create, etc.)
//! - Memfd flags and operations

use alloc::format;
use core::ffi::c_char;

use kerrno::{KError, KResult};
use kfs::{FS_CONTEXT, OpenOptions};
use linux_raw_sys::general::MFD_CLOEXEC;

use crate::{
    file::{File, FileLike},
    mm::UserConstPtr,
};

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
            return File::new(file).add_to_fd_table(cloexec).map(|fd| fd as _);
        }
    }
    Err(KError::TooManyOpenFiles)
}
