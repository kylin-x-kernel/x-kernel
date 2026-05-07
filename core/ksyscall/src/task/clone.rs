// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process and thread cloning syscalls.
//!
//! This module implements process and thread creation operations including:
//! - Clone system call with various flags
//! - Thread creation and configuration
//! - Process/thread sharing options (VM, FS, files, signals, etc.)
//!
//! The core logic is shared between `sys_clone` and `sys_clone3` via [`CloneRequest::do_clone`].

use alloc::sync::Arc;

use bitflags::bitflags;
use kcore::{
    mm::copy_from_kernel,
    task::{AsThread, ProcessData, Thread, add_task_to_table},
};
use kerrno::{KError, KResult};
use kfs::FS_CONTEXT;
use khal::uspace::UserContext;
use kprocess::Pid;
use kservices::task::new_user_task;
use ksignal::Signo;
use kspin::SpinNoIrq;
use ktask::{KTaskExt, current, spawn_task};
use linux_raw_sys::general::*;
use osvm::VirtMutPtr;

use crate::file::{FD_TABLE, FileLike, PidFd};

bitflags! {
    /// Options for use with [`sys_clone`] and [`sys_clone3`].
    ///
    /// Uses `u64` as the underlying type for compatibility with the clone3 64-bit flags field.
    #[derive(Debug, Clone, Copy, Default)]
    pub(crate) struct CloneFlags: u64 {
        /// The calling process and the child process run in the same
        /// memory space.
        const VM = CLONE_VM as u64;
        /// The caller and the child process share the same  filesystem
        /// information.
        const FS = CLONE_FS as u64;
        /// The calling process and the child process share the same file
        /// descriptor table.
        const FILES = CLONE_FILES as u64;
        /// The calling process and the child process share the same table
        /// of signal handlers.
        const SIGHAND = CLONE_SIGHAND as u64;
        /// Sets pidfd to the child process's PID file descriptor.
        const PIDFD = CLONE_PIDFD as u64;
        /// If the calling process is being traced, then trace the child
        /// also.
        const PTRACE = CLONE_PTRACE as u64;
        /// The execution of the calling process is suspended until the
        /// child releases its virtual memory resources via a call to
        /// execve(2) or _exit(2) (as with vfork(2)).
        const VFORK = CLONE_VFORK as u64;
        /// The parent of the new child  (as returned by getppid(2))
        /// will be the same as that of the calling process.
        const PARENT = CLONE_PARENT as u64;
        /// The child is placed in the same thread group as the calling
        /// process.
        const THREAD = CLONE_THREAD as u64;
        /// The cloned child is started in a new mount namespace.
        const NEWNS = CLONE_NEWNS as u64;
        /// The child and the calling process share a single list of System
        /// V semaphore adjustment values
        const SYSVSEM = CLONE_SYSVSEM as u64;
        /// The TLS (Thread Local Storage) descriptor is set to tls.
        const SETTLS = CLONE_SETTLS as u64;
        /// Store the child thread ID in the parent's memory.
        const PARENT_SETTID = CLONE_PARENT_SETTID as u64;
        /// Clear (zero) the child thread ID in child memory when the child
        /// exits, and do a wakeup on the futex at that address.
        const CHILD_CLEARTID = CLONE_CHILD_CLEARTID as u64;
        /// A tracing process cannot force `CLONE_PTRACE` on this child
        /// process.
        const UNTRACED = CLONE_UNTRACED as u64;
        /// Store the child thread ID in the child's memory.
        const CHILD_SETTID = CLONE_CHILD_SETTID as u64;
        /// Create the process in a new cgroup namespace.
        const NEWCGROUP = CLONE_NEWCGROUP as u64;
        /// Create the process in a new UTS namespace.
        const NEWUTS = CLONE_NEWUTS as u64;
        /// Create the process in a new IPC namespace.
        const NEWIPC = CLONE_NEWIPC as u64;
        /// Create the process in a new user namespace.
        const NEWUSER = CLONE_NEWUSER as u64;
        /// Create the process in a new PID namespace.
        const NEWPID = CLONE_NEWPID as u64;
        /// Create the process in a new network namespace.
        const NEWNET = CLONE_NEWNET as u64;
        /// The new process shares an I/O context with the calling process.
        const IO = CLONE_IO as u64;
    }
}

/// A one-shot request for creating a new process or thread.
///
/// Both `sys_clone` and `sys_clone3` incrementally build this request and then
/// consume it via [`Self::do_clone`].
pub(crate) struct CloneRequest {
    flags: CloneFlags,
    exit_signal: u64,
    stack: usize,
    parent_tid: usize,
    child_tid: usize,
    tls: usize,
    pidfd: usize,
}

impl CloneRequest {
    /// Creates a new request with all fields zeroed (defaults).
    pub fn new() -> Self {
        Self {
            flags: CloneFlags::empty(),
            exit_signal: 0,
            stack: 0,
            parent_tid: 0,
            child_tid: 0,
            tls: 0,
            pidfd: 0,
        }
    }

    pub fn flags(&self) -> CloneFlags {
        self.flags
    }

    pub fn set_flags(&mut self, flags: CloneFlags) -> &mut Self {
        self.flags = flags;
        self
    }

    pub fn set_exit_signal(&mut self, sig: u64) -> &mut Self {
        self.exit_signal = sig;
        self
    }

    pub fn set_stack(&mut self, sp: usize) -> &mut Self {
        self.stack = sp;
        self
    }

    /// Set the stack from a base address and size (clone3 convention).
    /// SP is computed as `base + size` for downward-growing architectures.
    pub fn set_stack_with_size(&mut self, base: u64, size: u64) -> &mut Self {
        self.stack = if base == 0 { 0 } else { (base + size) as usize };
        self
    }

    pub fn set_parent_tid(&mut self, ptr: usize) -> &mut Self {
        self.parent_tid = ptr;
        self
    }

    pub fn set_child_tid(&mut self, ptr: usize) -> &mut Self {
        self.child_tid = ptr;
        self
    }

    pub fn set_tls(&mut self, tls: usize) -> &mut Self {
        self.tls = tls;
        self
    }

    pub fn set_pidfd(&mut self, ptr: usize) -> &mut Self {
        self.pidfd = ptr;
        self
    }

    /// Consume this request and execute the clone operation.
    pub fn do_clone(mut self, uctx: &UserContext) -> KResult<isize> {
        if self.flags.contains(CloneFlags::VFORK) {
            debug!("do_clone: CLONE_VFORK slow path");
            self.flags.remove(CloneFlags::VM);
        }

        debug!(
            "do_clone <= flags: {:?}, exit_signal: {}, stack: {:#x}, ptid: {:#x}, ctid: {:#x}, \
             tls: {:#x}, pidfd: {:#x}",
            self.flags,
            self.exit_signal,
            self.stack,
            self.parent_tid,
            self.child_tid,
            self.tls,
            self.pidfd,
        );

        if self.exit_signal != 0 && self.flags.contains(CloneFlags::THREAD | CloneFlags::PARENT) {
            return Err(KError::InvalidInput);
        }
        if self.flags.contains(CloneFlags::THREAD)
            && !self.flags.contains(CloneFlags::VM | CloneFlags::SIGHAND)
        {
            return Err(KError::InvalidInput);
        }

        let exit_signal = Signo::from_repr(self.exit_signal as u8);

        let mut new_uctx = *uctx;
        if self.stack != 0 {
            new_uctx.set_sp(self.stack);
        }
        if self.flags.contains(CloneFlags::SETTLS) {
            new_uctx.set_tls(self.tls);
        }
        new_uctx.set_retval(0);

        let set_child_tid = if self.flags.contains(CloneFlags::CHILD_SETTID) {
            self.child_tid
        } else {
            0
        };

        let curr = current();
        let old_proc_data = &curr.as_thread().proc_data;

        let mut new_task = new_user_task(
            &curr.name(),
            new_uctx,
            set_child_tid,
            crate::dispatch_irq_syscall,
        );

        let tid = new_task.id().as_u64() as Pid;
        if self.flags.contains(CloneFlags::PARENT_SETTID) {
            (self.parent_tid as *mut Pid).write_vm(tid).ok();
        }

        let new_proc_data = if self.flags.contains(CloneFlags::THREAD) {
            new_task
                .ctx_mut()
                .set_page_table_root(old_proc_data.aspace.lock().page_table_root().into());
            old_proc_data.clone()
        } else {
            let proc = if self.flags.contains(CloneFlags::PARENT) {
                old_proc_data.proc.parent().ok_or(KError::InvalidInput)?
            } else {
                old_proc_data.proc.clone()
            }
            .fork(tid);

            let aspace = if self.flags.contains(CloneFlags::VM) {
                old_proc_data.aspace.clone()
            } else {
                let mut aspace = old_proc_data.aspace.lock();
                let aspace = aspace.try_clone()?;
                copy_from_kernel(&mut aspace.lock())?;
                aspace
            };
            new_task
                .ctx_mut()
                .set_page_table_root(aspace.lock().page_table_root().into());

            let signal_actions = if self.flags.contains(CloneFlags::SIGHAND) {
                old_proc_data.signal.actions.clone()
            } else {
                Arc::new(SpinNoIrq::new(old_proc_data.signal.actions.lock().clone()))
            };
            let proc_data = ProcessData::new(
                proc,
                old_proc_data.exe_path.read().clone(),
                old_proc_data.cmdline.read().clone(),
                aspace,
                signal_actions,
                exit_signal,
                old_proc_data.credentials.read().clone(),
            );
            proc_data.set_umask(old_proc_data.umask());
            // Inherit heap pointers from parent to ensure child's heap state is consistent after fork
            proc_data.set_heap_top(old_proc_data.get_heap_top());

            {
                let mut scope = proc_data.scope.write();
                if self.flags.contains(CloneFlags::FILES) {
                    FD_TABLE.scope_mut(&mut scope).clone_from(&FD_TABLE);
                } else {
                    FD_TABLE
                        .scope_mut(&mut scope)
                        .write()
                        .clone_from(&FD_TABLE.read());
                }

                if self.flags.contains(CloneFlags::FS) {
                    FS_CONTEXT.scope_mut(&mut scope).clone_from(&FS_CONTEXT);
                } else {
                    FS_CONTEXT
                        .scope_mut(&mut scope)
                        .lock()
                        .clone_from(&FS_CONTEXT.lock());
                }
            }

            proc_data
        };

        new_proc_data.proc.add_thread(tid);

        if self.flags.contains(CloneFlags::PIDFD) {
            let pidfd = PidFd::new(&new_proc_data);
            (self.pidfd as *mut i32).write_vm(pidfd.add_to_fd_table(true)?)?;
        }

        let thr = Thread::new(tid, new_proc_data);
        if self.flags.contains(CloneFlags::CHILD_CLEARTID) {
            thr.set_clear_child_tid(self.child_tid);
        }
        *new_task.task_ext_mut() = Some(unsafe { KTaskExt::from_impl(thr) });

        let task = spawn_task(new_task);
        add_task_to_table(&task);

        Ok(tid as _)
    }
}

pub fn sys_clone(
    uctx: &UserContext,
    flags: u32,
    stack: usize,
    parent_tid: usize,
    #[cfg(any(target_arch = "x86_64", target_arch = "loongarch64"))] child_tid: usize,
    tls: usize,
    #[cfg(not(any(target_arch = "x86_64", target_arch = "loongarch64")))] child_tid: usize,
) -> KResult<isize> {
    const FLAG_MASK: u32 = 0xff;

    let mut req = CloneRequest::new();

    let clone_flags = CloneFlags::from_bits_truncate((flags & !FLAG_MASK) as u64);
    req.set_flags(clone_flags);
    req.set_exit_signal((flags & FLAG_MASK) as u64);

    // In legacy clone, PIDFD and PARENT_SETTID share the same pointer argument so they conflict.
    if req
        .flags()
        .contains(CloneFlags::PIDFD | CloneFlags::PARENT_SETTID)
    {
        return Err(KError::InvalidInput);
    }

    req.set_stack(stack);
    req.set_tls(tls);
    req.set_parent_tid(parent_tid);
    req.set_child_tid(child_tid);

    if req.flags().contains(CloneFlags::PIDFD) {
        req.set_pidfd(parent_tid);
    }

    req.do_clone(uctx)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_fork(uctx: &UserContext) -> KResult<isize> {
    sys_clone(uctx, SIGCHLD, 0, 0, 0, 0)
}
