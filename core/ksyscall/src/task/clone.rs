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
use kerrno::{KError, KResult, LinuxError};
use khal::uspace::UserContext;
use kns::NamespaceFlags;
use kprocess::{
    AsThread, ForkAddressSpace, ForkFdTable, ForkFs, ForkParent, ForkSignalActions, Pid, PidFd,
    ProcessForkConfig, current_user_thread, publish_user_task_with,
};
use ksignal::Signo;
use ktask::current;
use linux_raw_sys::general::*;
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
        /// Reset every signal handler not set to ignore to its default
        /// disposition in the child. Only valid without [`Self::SIGHAND`].
        const CLEAR_SIGHAND = CLONE_CLEAR_SIGHAND;
        /// Place the child in the cgroup named by the clone3 `cgroup`
        /// descriptor instead of the caller's cgroup.
        const INTO_CGROUP = CLONE_INTO_CGROUP;
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
    reject_exit_signal_with_shared_parent: bool,
    /// Resolved `clone3(CLONE_INTO_CGROUP)` target; `None` inherits the
    /// caller's cgroup. Only the clone3 adapter sets this.
    cgroup_target: Option<Arc<kcgroup::Cgroup>>,
}

impl CloneRequest {
    /// Creates a new request with all fields zeroed (defaults).
    pub(crate) fn new() -> Self {
        Self {
            flags: CloneFlags::empty(),
            exit_signal: 0,
            stack: 0,
            parent_tid: 0,
            child_tid: 0,
            tls: 0,
            pidfd: 0,
            reject_exit_signal_with_shared_parent: false,
            cgroup_target: None,
        }
    }

    pub(crate) fn flags(&self) -> CloneFlags {
        self.flags
    }

    pub(crate) fn set_flags(&mut self, flags: CloneFlags) -> &mut Self {
        self.flags = flags;
        self
    }

    pub(crate) fn set_exit_signal(&mut self, sig: u64) -> &mut Self {
        self.exit_signal = sig;
        self
    }

    pub(crate) fn set_stack(&mut self, sp: usize) -> &mut Self {
        self.stack = sp;
        self
    }

    /// Set the stack from a base address and size (clone3 convention).
    /// SP is computed as `base + size` for downward-growing architectures.
    pub(crate) fn set_stack_with_size(&mut self, base: u64, size: u64) -> &mut Self {
        self.stack = if base == 0 { 0 } else { (base + size) as usize };
        self
    }

    pub(crate) fn set_parent_tid(&mut self, ptr: usize) -> &mut Self {
        self.parent_tid = ptr;
        self
    }

    pub(crate) fn set_child_tid(&mut self, ptr: usize) -> &mut Self {
        self.child_tid = ptr;
        self
    }

    pub(crate) fn set_tls(&mut self, tls: usize) -> &mut Self {
        self.tls = tls;
        self
    }

    pub(crate) fn set_pidfd(&mut self, ptr: usize) -> &mut Self {
        self.pidfd = ptr;
        self
    }

    /// Sets the resolved `CLONE_INTO_CGROUP` target cgroup.
    pub(crate) fn set_cgroup_target(&mut self, target: Arc<kcgroup::Cgroup>) -> &mut Self {
        self.cgroup_target = Some(target);
        self
    }

    pub(crate) fn reject_exit_signal_with_shared_parent(&mut self) -> &mut Self {
        self.reject_exit_signal_with_shared_parent = true;
        self
    }

    /// Consume this request and execute the clone operation.
    /// Requires a current user thread in sleepable task context, without locks
    /// needed by reclaim, I/O or filesystem callbacks. Entry allocation happens
    /// before preparing child process/thread state and may return NoMemory.
    pub(crate) fn do_clone(self, uctx: &UserContext) -> KResult<isize> {
        self.do_clone_with_entry(uctx, |context, child_tid| {
            posix_process::prepare_user_entry(context, child_tid, crate::handle_syscall)
        })
    }

    fn do_clone_with_entry(
        self,
        uctx: &UserContext,
        prepare_entry: impl FnOnce(UserContext, usize) -> KResult<ktask::PreparedTaskEntry>,
    ) -> KResult<isize> {
        self.do_clone_with_preparation(uctx, prepare_entry, knet::netns::PreparedStack::new)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: TODO(cg): legacy clone; split flag validation, \
                  resource sharing and task start"
    )]
    fn do_clone_with_preparation(
        mut self,
        uctx: &UserContext,
        prepare_entry: impl FnOnce(UserContext, usize) -> KResult<ktask::PreparedTaskEntry>,
        prepare_network: impl FnOnce() -> KResult<knet::netns::PreparedStack>,
    ) -> KResult<isize> {
        if self.flags.contains(CloneFlags::VFORK) {
            // TODO: Implement Linux vfork semantics: keep the mm shared, block
            // the parent until the child successfully execs or exits, and wake
            // it after the child has detached from the shared mm. execve
            // currently modifies its process mm in place, so sharing it here
            // would let the child replace the parent's address space.
            debug!("do_clone: CLONE_VFORK uses private-mm fallback");
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

        self.validate_pre_cgroup_flags()?;
        // Threads share the process's cgroup membership. This kernel has no
        // threaded cgroup domains, so a thread cannot be placed into another
        // cgroup. Checked after the THREAD-needs-SIGHAND EINVAL above and after
        // the clone3 entry resolved the cgroup fd, matching Linux
        // cgroup_attach_permissions (EOPNOTSUPP after the fd fetch).
        if self
            .flags
            .contains(CloneFlags::INTO_CGROUP | CloneFlags::THREAD)
        {
            return Err(KError::from(LinuxError::EOPNOTSUPP));
        }
        // CLONE_INTO_CGROUP needs a resolved target; only the clone3 adapter
        // supplies one, so a bare flag without a target is a bad request.
        if self.flags.contains(CloneFlags::INTO_CGROUP) && self.cgroup_target.is_none() {
            return Err(KError::from(LinuxError::EINVAL));
        }

        // Namespace flag validation
        self.validate_namespace_flags()?;

        let namespace_flags = self.extract_namespace_flags();

        // Linux `copy_process()` rejects CLONE_THREAD combined with the
        // namespace flags that would change the thread group's view with
        // `EINVAL` (fork.c spells out NEWUSER/NEWPID; the same rule guards
        // every namespace switch here). This kernel attaches the namespace
        // bundle to the process, so a thread clone cannot switch namespaces
        // at all — the flags are rejected instead of being silently ignored
        // (and silently bypassing the privilege gate below).
        if self.flags.contains(CloneFlags::THREAD) && !namespace_flags.is_empty() {
            return Err(KError::from(LinuxError::EINVAL));
        }
        // The same Linux check has a third arm that needs no namespace flag:
        // `task_active_pid_ns(current) != nsp->pid_ns_for_children` is also
        // `EINVAL`, i.e. a thread cannot join a group whose PID namespace its
        // own view has already left (`unshare(CLONE_NEWPID)`/`setns`). Without
        // this arm such a clone succeeds here and fails on Linux.
        if self.flags.contains(CloneFlags::THREAD) {
            let process = kprocess::current_user_process();
            if !Arc::ptr_eq(
                &process.active_pid_namespace(),
                &process.pid_ns_for_children()?,
            ) {
                return Err(KError::from(LinuxError::EINVAL));
            }
        }

        // Namespace authorization happens in `kns::NamespacePlan::build`,
        // after the user namespace (if any) has been created. That ordering
        // matches Linux: `CLONE_NEWUSER` is capability-free and every other
        // flag is checked against the new user namespace.

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
        // Linux `sched_fork`: sample the creator's class before the child is
        // built. A Deadline parent without SCHED_RESET_ON_FORK is EAGAIN and
        // must not leave a half-built child behind. The attribute is written
        // before publication, while the child is still private.
        let child_attr = ktask::sched_getattr(&curr)
            .for_fork()
            .map_err(super::sched::sched_errno)?;
        let current_thread = current_user_thread();
        let exit_signal = if self.flags.contains(CloneFlags::THREAD) {
            None
        } else {
            Self::parse_exit_signal(self.exit_signal)?
        };
        let entry = prepare_entry(new_uctx, set_child_tid)?;
        let network_stack = if namespace_flags.contains(NamespaceFlags::NEWNET) {
            Some(prepare_network()?)
        } else {
            None
        };
        let prepared = if self.flags.contains(CloneFlags::THREAD) {
            current_thread
                .prepare_thread_clone_with_files(self.flags.contains(CloneFlags::FILES))?
        } else {
            let config = ProcessForkConfig {
                parent: if self.flags.contains(CloneFlags::PARENT) {
                    ForkParent::CallerParent
                } else {
                    ForkParent::Caller
                },
                address_space: if self.flags.contains(CloneFlags::VM) {
                    ForkAddressSpace::Shared
                } else {
                    ForkAddressSpace::Private
                },
                fs: if self.flags.contains(CloneFlags::FS) {
                    ForkFs::Shared
                } else {
                    ForkFs::Private
                },
                signal_actions: if self.flags.contains(CloneFlags::SIGHAND) {
                    ForkSignalActions::Shared
                } else if self.flags.contains(CloneFlags::CLEAR_SIGHAND) {
                    ForkSignalActions::Cleared
                } else {
                    ForkSignalActions::Private
                },
                fd_table: if self.flags.contains(CloneFlags::FILES) {
                    ForkFdTable::Shared
                } else {
                    ForkFdTable::Private
                },
                namespace_flags,
                exit_signal,
                cgroup_target: self.cgroup_target.take(),
            };
            current_thread.prepare_process_fork(config)?
        };
        let tid = prepared.tid();
        let child_process = prepared.process().clone();
        if let Some(network_stack) = network_stack {
            // Materialize the private protocol stack of the child's fresh
            // network namespace (loopback with 127.0.0.1/8) before the child
            // can run.
            network_stack.install(child_process.net_ns()?.as_ref());
        }
        let page_table_root = prepared.page_table_root();
        let (thr, task_number) = prepared.into_parts();
        // Linux reports the new task's number in the forking process's active
        // PID namespace (pid_nr_ns): the leader number for a process fork,
        // the thread's own TID for a thread clone. The projection uses the
        // created task's OWN identity — the process's leader handle would
        // report the thread-group number and break every pthread tid — and
        // an identity without a projection in the observer's namespace keeps
        // the root number, as does a root-namespace observer.
        let observer = current_user_thread().process().active_pid_namespace();
        let tid = task_number.number_in(&observer).unwrap_or(tid);

        if self.flags.contains(CloneFlags::CHILD_CLEARTID) {
            thr.set_clear_child_tid(self.child_tid);
        }

        // A thread pidfd (CLONE_THREAD | CLONE_PIDFD) reports the thread's own
        // tid, resolved dynamically in fdinfo; capture the id and stable
        // identity before task_number/thr move into the new task. A process
        // pidfd reports the stable leader pid via PidFd::new_file instead.
        let pidfd_task_identity = if self.flags.contains(CloneFlags::THREAD) {
            Some((task_number.clone(), thr.identity_token()))
        } else {
            None
        };
        let child_cgroup = thr.cgroup();
        let mut new_task = new_user_task(&curr.name(), entry, task_number, thr);
        // Linux copies the creator's placement into the child, then applies
        // the child's own cgroup cpuset bound. Keeping the two inputs separate
        // prevents a stale parent bound from pinning the child.
        new_task.inherit_placement(&curr);
        if let Some(cgroup) = child_cgroup
            && let Ok(cpus) = cgroup.cpuset_effective_cpus()
        {
            let mut bound = ktask::KCpuMask::new();
            for cpu in cpus.iter() {
                bound.set(cpu, true);
            }
            let _ = new_task.set_cgroup_cpus(bound);
        }
        new_task.ctx_mut().set_page_table_root(page_table_root);

        // Create the pidfd before publishing, but install it into the caller's
        // (possibly shared) fd table only after the child is published, so a
        // concurrent /proc/self/fdinfo reader never sees a pidfd whose task is
        // not yet lookup-visible (Linux attaches the pid before the pidfd).
        let pidfd_file = if self.flags.contains(CloneFlags::PIDFD) {
            Some(match pidfd_task_identity {
                Some((task_number, token)) => PidFd::new_file_for_task(
                    &child_process,
                    task_number,
                    token,
                    O_RDWR,
                    kprocess::current_cred(),
                )?,
                None => PidFd::new_file(&child_process, O_RDWR, kprocess::current_cred())?,
            })
        } else {
            None
        };

        let published = publish_user_task_with(new_task, |task| {
            ktask::init_task_sched_attr(task, child_attr).map_err(super::sched::sched_errno)?;
            if let Some(thread) = task.try_as_thread() {
                thread.set_nice(kprocess::NiceValue::new_clamped(child_attr.nice.as_i32()));
            }
            Ok(())
        })?;

        let pidfd_install = if let Some(file) = pidfd_file {
            let resources = kprocess::current_files();
            match resources.add_file(file, true) {
                Ok(fd) => Some((resources, fd)),
                Err(err) => {
                    published.abort();
                    return Err(err);
                }
            }
        } else {
            None
        };

        published.commit(|_task| {
            if self.flags.contains(CloneFlags::PARENT_SETTID)
                && let Err(err) = posix_types::UserPtr::<Pid>::from(self.parent_tid).write_vm(tid)
            {
                if let Some((resources, fd)) = pidfd_install.as_ref() {
                    // Rollback is best effort; preserve the original user-memory error.
                    let _ = resources.close_file(*fd);
                }
                return Err(err.into());
            }
            if let Some((resources, fd)) = pidfd_install.as_ref()
                && let Err(err) = posix_types::UserPtr::<i32>::from(self.pidfd).write_vm(*fd)
            {
                // Rollback is best effort; preserve the original user-memory error.
                let _ = resources.close_file(*fd);
                return Err(err.into());
            }
            Ok(())
        })?;

        Ok(tid as _)
    }

    fn parse_exit_signal(exit_signal: u64) -> KResult<Option<Signo>> {
        if exit_signal == 0 {
            return Ok(None);
        }
        Signo::from_raw(exit_signal)
            .ok_or(KError::InvalidInput)
            .map(Some)
    }

    fn validate_exit_signal_policy(&self) -> KResult<()> {
        if self.reject_exit_signal_with_shared_parent
            && self.exit_signal != 0
            && self
                .flags
                .intersects(CloneFlags::THREAD | CloneFlags::PARENT)
        {
            return Err(KError::InvalidInput);
        }
        Ok(())
    }

    /// Validates the flag combinations Linux checks before the
    /// `CLONE_INTO_CGROUP` descriptor is resolved: `clone3_args_valid`
    /// (exit-signal policy, `CLONE_CLEAR_SIGHAND | CLONE_SIGHAND`) and the
    /// `copy_process` thread/signal-sharing check that precedes
    /// `cgroup_can_fork`. Running them before the fd lookup keeps their
    /// `EINVAL` precedence over a bad descriptor's `EBADF`.
    ///
    /// The `CLONE_INTO_CGROUP | CLONE_THREAD` rejection is deliberately not
    /// here: Linux raises it in `cgroup_attach_permissions`, after the fd
    /// fetch, so it stays in [`Self::do_clone_with_preparation`].
    pub(crate) fn validate_pre_cgroup_flags(&self) -> KResult<()> {
        self.validate_exit_signal_policy()?;
        // Linux copy_process: CLONE_THREAD requires CLONE_SIGHAND (and VM).
        if self.flags.contains(CloneFlags::THREAD)
            && !self.flags.contains(CloneFlags::VM | CloneFlags::SIGHAND)
        {
            return Err(KError::InvalidInput);
        }
        self.validate_clear_sighand_flag()
    }

    /// Rejects `CLONE_CLEAR_SIGHAND` combined with `CLONE_SIGHAND`: a shared
    /// action table cannot be cleared without affecting the parent.
    fn validate_clear_sighand_flag(&self) -> KResult<()> {
        if self
            .flags
            .contains(CloneFlags::CLEAR_SIGHAND | CloneFlags::SIGHAND)
        {
            return Err(KError::from(LinuxError::EINVAL));
        }
        Ok(())
    }

    /// Validates namespace-related flag combinations.
    fn validate_namespace_flags(&self) -> KResult<()> {
        let ns = self.flags;

        // CLONE_NEWNS | CLONE_FS is invalid
        if ns.contains(CloneFlags::NEWNS | CloneFlags::FS) {
            return Err(KError::from(LinuxError::EINVAL));
        }
        // A new user namespace must own a private fs_struct; sharing the
        // parent's would leave the mount view owned by the old namespace.
        if ns.contains(CloneFlags::NEWUSER | CloneFlags::FS) {
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

        // Linux rejects `CLONE_INTO_CGROUP` for threads: the target cgroup
        // applies to a new process, and a thread inherits its group's
        // membership.
        if ns.contains(CloneFlags::INTO_CGROUP | CloneFlags::THREAD) {
            return Err(KError::from(LinuxError::EINVAL));
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

pub(crate) fn sys_clone(
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
pub(crate) fn sys_fork(uctx: &UserContext) -> KResult<isize> {
    sys_clone(uctx, SIGCHLD, 0, 0, 0, 0)
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn sys_vfork(uctx: &UserContext) -> KResult<isize> {
    let mut request = CloneRequest::new();
    request
        .set_flags(CloneFlags::VM | CloneFlags::VFORK)
        .set_exit_signal(SIGCHLD as u64);
    request.do_clone(uctx)
}

unittest::path_mod_test!(tests_clone, "../../ut-tests/task/clone/tests_clone.rs");
