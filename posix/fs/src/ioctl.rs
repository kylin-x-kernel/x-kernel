// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `ioctl(2)` syscall implementation.

use core::ffi::c_int;

use kerrno::{KError, KResult};
use linux_raw_sys::{
    general::O_NONBLOCK,
    ioctl::{FIONBIO, TCGETS, TIOCGWINSZ},
};
use posix_types::UserConstPtr;

/// The `ioctl()` syscall manipulates the underlying device parameters
/// of special files.
pub fn sys_ioctl(fd: i32, cmd: u32, arg: usize) -> KResult<isize> {
    debug!("sys_ioctl <= fd: {fd}, cmd: {cmd}, arg: {arg}");
    let f = kprocess::current_resources().get_file(fd)?;
    if cmd == FIONBIO {
        let val = UserConstPtr::<c_int>::from(arg).read_vm()?;
        if val != 0 && val != 1 {
            return Err(KError::InvalidInput);
        }
        let flags = if val != 0 { O_NONBLOCK } else { 0 };
        f.replace_flags(O_NONBLOCK, flags);
        return Ok(0);
    }
    f.ioctl(cmd, arg)
        .map(|result| result as isize)
        .inspect_err(|err| {
            if *err == KError::NotATty {
                // TIOCGWINSZ / TCGETS on non-terminal fds are normal
                // (isatty() calls TCGETS to check if fd is a terminal)
                if cmd == TIOCGWINSZ || cmd == TCGETS {
                    return;
                }
                warn!("Unsupported ioctl command: {cmd} for fd: {fd}");
            }
        })
}
