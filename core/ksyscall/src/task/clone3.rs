// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `clone3` system call.
//!
//! `clone3` is the modern interface for creating new processes/threads, superseding the
//! legacy `clone` syscall. It takes a pointer to a `clone_args` struct instead of
//! encoding everything in register arguments, which allows for extensibility and
//! cleaner 64-bit flag handling.

use core::mem::{self, MaybeUninit};

use kerrno::{KError, KResult};
use khal::uspace::UserContext;
use linux_raw_sys::general::clone_args;

use super::clone::{CloneFlags, CloneRequest};

/// Minimum size of clone_args (the original v0 layout: flags through tls, 8 fields × 8 bytes).
const CLONE_ARGS_SIZE_VER0: usize = 64;

const fn zeroed_clone_args() -> clone_args {
    clone_args {
        flags: 0,
        pidfd: 0,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: 0,
        stack: 0,
        stack_size: 0,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: 0,
    }
}

pub fn sys_clone3(uctx: &UserContext, cl_args: usize, size: usize) -> KResult<isize> {
    if size < CLONE_ARGS_SIZE_VER0 {
        return Err(KError::InvalidInput);
    }

    // Zero-init then copy min(size, sizeof) bytes from user space so that
    // fields beyond the caller's struct version default to zero.
    let mut kargs = zeroed_clone_args();
    let read_size = core::cmp::min(size, mem::size_of::<clone_args>());
    // SAFETY: `kargs` is a fully initialized plain `#[repr(C)]` struct of
    // integer fields. Reinterpreting its storage as writable `MaybeUninit<u8>`
    // bytes is sound for an in-place copy from user memory.
    let dst = unsafe {
        core::slice::from_raw_parts_mut(
            (&mut kargs as *mut clone_args).cast::<MaybeUninit<u8>>(),
            read_size,
        )
    };
    osvm::read_vm_mem(cl_args as *const u8, dst)?;

    debug!(
        "sys_clone3 <= flags: {:#x}, exit_signal: {}, stack: {:#x}, stack_size: {:#x}, ptid: \
         {:#x}, ctid: {:#x}, tls: {:#x}, pidfd: {:#x}",
        kargs.flags,
        kargs.exit_signal,
        kargs.stack,
        kargs.stack_size,
        kargs.parent_tid,
        kargs.child_tid,
        kargs.tls,
        kargs.pidfd,
    );

    let mut req = CloneRequest::new();

    // clone3 rejects unknown flags with EINVAL rather than silently truncating
    // them (unlike legacy clone, which masks with the low-byte exit signal).
    let flags = CloneFlags::from_bits(kargs.flags)
        .ok_or_else(|| KError::from(kerrno::LinuxError::EINVAL))?;
    req.set_flags(flags)
        .set_exit_signal(kargs.exit_signal)
        .set_stack_with_size(kargs.stack, kargs.stack_size)
        .set_parent_tid(kargs.parent_tid as usize)
        .set_child_tid(kargs.child_tid as usize)
        .set_tls(kargs.tls as usize)
        .set_pidfd(kargs.pidfd as usize);

    req.do_clone(uctx)
}
