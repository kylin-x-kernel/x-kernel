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

use bitflags::bitflags;
use kerrno::{KError, KResult, LinuxError};
use khal::uspace::UserContext;
use kns::NamespaceFlags;
use kprocess::{Pid, PidFd, ProcessForkConfig, current_user_thread, publish_user_task};
use ksignal::Signo;
use ktask::current;
use linux_raw_sys::general::*;
use osvm::VirtMutPtr;
use posix_process::new_user_task;

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
        /// Create the process in a new time namespace. Not yet implemented;
        /// rejected with `ENOSYS` in [`CloneRequest::validate_namespace_flags`].
        const NEWTIME = CLONE_NEWTIME as u64;
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

        // Namespace flag validation
        self.validate_namespace_flags()?;

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
        let current_thread = current_user_thread();
        let prepared = if self.flags.contains(CloneFlags::THREAD) {
            current_thread.prepare_thread_clone()?
        } else {
            current_thread.prepare_process_fork(ProcessForkConfig {
                share_parent: self.flags.contains(CloneFlags::PARENT),
                share_vm: self.flags.contains(CloneFlags::VM),
                share_fs: self.flags.contains(CloneFlags::FS),
                share_sighand: self.flags.contains(CloneFlags::SIGHAND),
                share_files: self.flags.contains(CloneFlags::FILES),
                namespace_flags: self.extract_namespace_flags(),
                exit_signal,
            })?
        };
        let tid = prepared.tid();
        let child_process = prepared.process().clone();
        let page_table_root = prepared.page_table_root();
        let (thr, task_number) = prepared.into_parts();

        if self.flags.contains(CloneFlags::CHILD_CLEARTID) {
            thr.set_clear_child_tid(self.child_tid);
        }

        let mut new_task = new_user_task(
            &curr.name(),
            new_uctx,
            set_child_tid,
            task_number,
            thr,
            crate::dispatch_irq_syscall,
        );
        new_task.ctx_mut().set_page_table_root(page_table_root);

        let pidfd_install = if self.flags.contains(CloneFlags::PIDFD) {
            let pidfd_file = PidFd::new_file(&child_process, O_RDWR)?;
            let resources = kprocess::current_resources();
            let fd = resources.add_file(pidfd_file, true)?;
            Some((resources, fd))
        } else {
            None
        };

        publish_user_task(new_task).commit(|_| {
            if self.flags.contains(CloneFlags::PARENT_SETTID)
                && let Err(err) = (self.parent_tid as *mut Pid).write_vm(tid)
            {
                if let Some((resources, fd)) = pidfd_install.as_ref() {
                    resources.close_file(*fd).ok();
                }
                return Err(err.into());
            }
            if let Some((resources, fd)) = pidfd_install.as_ref()
                && let Err(err) = (self.pidfd as *mut i32).write_vm(*fd)
            {
                resources.close_file(*fd).ok();
                return Err(err.into());
            }
            Ok(())
        })?;

        Ok(tid as _)
    }

    /// Validates namespace-related flag combinations.
    fn validate_namespace_flags(&self) -> KResult<()> {
        let ns = self.flags;

        // CLONE_NEWNS | CLONE_FS is invalid
        if ns.contains(CloneFlags::NEWNS | CloneFlags::FS) {
            return Err(KError::from(LinuxError::EINVAL));
        }

        // CLONE_NEWIPC | CLONE_SYSVSEM is invalid
        if ns.contains(CloneFlags::NEWIPC | CloneFlags::SYSVSEM) {
            return Err(KError::from(LinuxError::EINVAL));
        }

        // CLONE_NEWPID | CLONE_THREAD is invalid
        if ns.contains(CloneFlags::NEWPID | CloneFlags::THREAD) {
            return Err(KError::from(LinuxError::EINVAL));
        }

        // CLONE_NEWPID | CLONE_PARENT is invalid. Linux's copy_process()
        // rejects this combination with EINVAL because re-parenting semantics
        // conflict with creating a new PID namespace.
        if ns.contains(CloneFlags::NEWPID | CloneFlags::PARENT) {
            return Err(KError::from(LinuxError::EINVAL));
        }

        // Unimplemented namespace flags: return ENOSYS. NEWPID is included
        // here because, although the flag is parsed, full PID-namespace
        // support is not wired up yet.
        let unimplemented =
            CloneFlags::NEWNET | CloneFlags::NEWUSER | CloneFlags::NEWCGROUP | CloneFlags::NEWTIME;
        if ns.intersects(unimplemented) {
            return Err(KError::from(LinuxError::ENOSYS));
        }

        // CLONE_NEWPID is not yet fully supported
        if ns.contains(CloneFlags::NEWPID) {
            return Err(KError::from(LinuxError::ENOSYS));
        }

        Ok(())
    }

    /// Extracts namespace flags into `NamespaceFlags` used by `kns`.
    fn extract_namespace_flags(&self) -> NamespaceFlags {
        let mut ns = NamespaceFlags::empty();
        if self.flags.contains(CloneFlags::NEWNS) {
            ns |= NamespaceFlags::NEWNS;
        }
        if self.flags.contains(CloneFlags::NEWUTS) {
            ns |= NamespaceFlags::NEWUTS;
        }
        if self.flags.contains(CloneFlags::NEWIPC) {
            ns |= NamespaceFlags::NEWIPC;
        }
        if self.flags.contains(CloneFlags::NEWUSER) {
            ns |= NamespaceFlags::NEWUSER;
        }
        if self.flags.contains(CloneFlags::NEWPID) {
            ns |= NamespaceFlags::NEWPID;
        }
        if self.flags.contains(CloneFlags::NEWNET) {
            ns |= NamespaceFlags::NEWNET;
        }
        if self.flags.contains(CloneFlags::NEWCGROUP) {
            ns |= NamespaceFlags::NEWCGROUP;
        }
        if self.flags.contains(CloneFlags::NEWTIME) {
            ns |= NamespaceFlags::NEWTIME;
        }
        ns
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

#[cfg(unittest)]
mod tests_clone {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_validate_rejects_newns_and_fs() {
        let mut req = CloneRequest::new();
        req.set_flags(CloneFlags::NEWNS | CloneFlags::FS);
        assert!(req.validate_namespace_flags().is_err());
    }

    #[def_test]
    fn test_validate_rejects_newipc_and_sysvsem() {
        let mut req = CloneRequest::new();
        req.set_flags(CloneFlags::NEWIPC | CloneFlags::SYSVSEM);
        assert!(req.validate_namespace_flags().is_err());
    }

    #[def_test]
    fn test_validate_rejects_newpid_and_thread() {
        let mut req = CloneRequest::new();
        req.set_flags(CloneFlags::NEWPID | CloneFlags::THREAD);
        assert!(req.validate_namespace_flags().is_err());
    }

    #[def_test]
    fn test_validate_rejects_newpid_and_parent() {
        let mut req = CloneRequest::new();
        req.set_flags(CloneFlags::NEWPID | CloneFlags::PARENT);
        assert!(req.validate_namespace_flags().is_err());
    }

    #[def_test]
    fn test_validate_rejects_unimplemented_namespaces_with_enosys() {
        for flags in [
            CloneFlags::NEWNET,
            CloneFlags::NEWUSER,
            CloneFlags::NEWCGROUP,
        ] {
            let mut req = CloneRequest::new();
            req.set_flags(flags);
            let err = req.validate_namespace_flags().unwrap_err();
            // ENOSYS is the expected errno for unimplemented namespaces.
            assert_eq!(
                LinuxError::from(err),
                LinuxError::ENOSYS,
                "expected ENOSYS for {flags:?}"
            );
        }
    }

    #[def_test]
    fn test_validate_rejects_newtime_with_enosys() {
        let mut req = CloneRequest::new();
        req.set_flags(CloneFlags::NEWTIME);
        let err = req.validate_namespace_flags().unwrap_err();
        assert_eq!(
            LinuxError::from(err),
            LinuxError::ENOSYS,
            "expected ENOSYS for NEWTIME"
        );
    }

    #[def_test]
    fn test_extract_namespace_flags_includes_newtime() {
        let mut req = CloneRequest::new();
        req.set_flags(CloneFlags::NEWNS | CloneFlags::NEWUTS | CloneFlags::NEWTIME);
        let ns = req.extract_namespace_flags();
        assert!(ns.contains(NamespaceFlags::NEWNS));
        assert!(ns.contains(NamespaceFlags::NEWUTS));
        assert!(ns.contains(NamespaceFlags::NEWTIME));
    }
}
