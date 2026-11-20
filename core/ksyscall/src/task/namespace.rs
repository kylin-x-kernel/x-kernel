// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace syscalls (`unshare`, `setns`).
//!
//! `unshare(2)` runs a two-phase [`NsProxy::prepare_unshare`] /
//! `Process::apply_prepared_unshare` commit so that every Linux acl and
//! combination check happens before any field of the running process changes;
//! the descriptor-table detach then commits on the calling thread. `setns(2)`
//! resolves the fd through one of two channels: an [`NsFd`] that pins the
//! namespace directly, or a pidfd whose target process's namespace is entered
//! (Linux 5.8+ semantics). Both channels sit behind one authorization gate:
//! joining a namespace requires effective `CAP_SYS_ADMIN` in the initial user
//! namespace, standing in for Linux's per-type checks. The only per-type
//! difference would be the time namespace's `CAP_SYS_TIME`, and this kernel
//! rejects time-namespace creation and entry as unsupported.

use alloc::{sync::Arc, vec::Vec};

use kcred::{CAP_SYS_ADMIN, CAP_SYS_CHROOT, Cred, UserNamespace};
use kerrno::{KError, KResult};
use kns::{NamespaceFlags, NsFd, PidNamespace, UnshareContext, UnshareError};
use kprocess::{AsThread, PidFd, current_files, current_user_process};
use linux_raw_sys::general::{
    CLONE_FILES, CLONE_FS, CLONE_SIGHAND, CLONE_SYSVSEM, CLONE_THREAD, CLONE_VM,
};

/// Parse `unshare(2)` flags the way Linux's flag gate does.
///
/// Unknown bits are rejected with `EINVAL`. The thread-shared-state bits
/// `CLONE_THREAD` / `CLONE_SIGHAND` / `CLONE_VM` are legal for a
/// single-threaded caller (they have nothing to detach from) and rejected only
/// for a multithreaded one, mirroring `check_unshare_flags`; Linux's
/// `CLONE_VM => CLONE_SIGHAND => CLONE_THREAD` implication chain therefore
/// never changes the outcome, because all three bits share that same rule.
/// `CLONE_NEWNS` implies `CLONE_FS`, as in `ksys_unshare`: the mount-namespace
/// switch needs a private fs struct and the kernel adds the bit itself instead
/// of requiring callers to spell it out. `CLONE_SYSVSEM` is accepted as a
/// no-op — the kernel implements no SysV-semaphore undo lists to detach, and
/// Linux's gate admits the bit unconditionally.
///
/// Every namespace bit supported by [`NamespaceFlags`] passes this gate.
/// Linux's `CLONE_NEWUSER => CLONE_THREAD | CLONE_FS` implication is applied
/// by [`kns::NamespacePlan::build`], which creates the user namespace first and
/// derives the owners of every sibling namespace from it.
fn unshare_flag_set(flags: u64, multithreaded: bool) -> KResult<(NamespaceFlags, bool, bool)> {
    let namespace_bits = NamespaceFlags::all().bits();
    let allowed = namespace_bits
        | CLONE_FS as u64
        | CLONE_FILES as u64
        | CLONE_VM as u64
        | CLONE_SIGHAND as u64
        | CLONE_THREAD as u64
        | CLONE_SYSVSEM as u64;
    if flags & !allowed != 0 {
        return Err(KError::InvalidInput);
    }
    let shares_thread_state =
        flags & (CLONE_VM as u64 | CLONE_SIGHAND as u64 | CLONE_THREAD as u64) != 0;
    if shares_thread_state && multithreaded {
        return Err(KError::InvalidInput);
    }
    let namespaces = NamespaceFlags::from_bits_truncate(flags);
    if namespaces.contains(NamespaceFlags::NEWUSER) {
        // Linux implies CLONE_THREAD|CLONE_FS for NEWUSER and rejects the
        // request from a multithreaded caller.
        if multithreaded {
            return Err(KError::InvalidInput);
        }
        let detach_fs = true;
        let detach_files = flags & CLONE_FILES as u64 != 0;
        return Ok((namespaces, detach_fs, detach_files));
    }
    let detach_fs = flags & CLONE_FS as u64 != 0 || namespaces.contains(NamespaceFlags::NEWNS);
    let detach_files = flags & CLONE_FILES as u64 != 0;
    Ok((namespaces, detach_fs, detach_files))
}

fn map_unshare_error(err: UnshareError) -> KError {
    match err {
        UnshareError::NotPermitted => KError::OperationNotPermitted,
        UnshareError::InvalidArgument => KError::InvalidInput,
        UnshareError::Mount(err) => err,
        UnshareError::NoSpace => KError::from(kerrno::LinuxError::ENOSPC),
    }
}

/// Returns `true` when `target` is `active` itself or one of its
/// descendants — Linux `pidns_install`'s reachability rule. Fork must return
/// a usable number in the caller's namespace and processes must not escape
/// it; the same constraint keeps plain-fork allocation — keyed on
/// `pid_ns_for_children` — resolvable in the caller's namespace after a
/// `setns(CLONE_NEWPID)`.
fn pid_ns_reachable(active: &Arc<PidNamespace>, target: &Arc<PidNamespace>) -> bool {
    if target.level() < active.level() {
        return false;
    }
    let mut ancestor = Some(target);
    while let Some(ns) = ancestor {
        if ns.level() == active.level() {
            return Arc::ptr_eq(ns, active);
        }
        ancestor = ns.parent();
    }
    false
}

fn ensure_pid_ns_reachable(process: &kprocess::Process, target: &Arc<PidNamespace>) -> KResult<()> {
    if pid_ns_reachable(&process.active_pid_namespace(), target) {
        Ok(())
    } else {
        Err(KError::InvalidInput)
    }
}

/// Applies Linux's per-type `setns(2)` install checks for one namespace.
///
/// Linux evaluates these checks against two credentials: the install functions
/// test the target namespace's owner with the caller's *current* credential,
/// while the caller-side capability tests use `nsset->cred`. The latter differs
/// from the current credential when the same request first enters a user
/// namespace, because that pending credential carries full capabilities in the
/// new user namespace. There is deliberately no process-wide privileged proxy.
/// The PID reachability rule and multithreaded time-namespace rejection live
/// here because they are part of the same install predicate. Switches that are
/// a successful no-op in Linux (re-entering the current cgroup namespace) pass
/// this layer and are collapsed by the commit phase.
fn authorize_setns(
    process: &kprocess::Process,
    target_cred: &Cred,
    caller_cred: &Cred,
    nsfd: &NsFd,
    copy_fs: bool,
) -> KResult<()> {
    let require_target = |user_ns: &UserNamespace, capability: u32| {
        if target_cred.has_cap_in(user_ns, capability) {
            Ok(())
        } else {
            Err(KError::OperationNotPermitted)
        }
    };
    let require_caller = |capability: u32| {
        if caller_cred.has_cap_in(caller_cred.user_ns(), capability) {
            Ok(())
        } else {
            Err(KError::OperationNotPermitted)
        }
    };
    let require_both_admin = |target: &Arc<UserNamespace>| {
        require_target(target, CAP_SYS_ADMIN)?;
        require_caller(CAP_SYS_ADMIN)
    };

    match nsfd {
        NsFd::User(target) => {
            // Gaining capabilities by re-entering the namespace we already
            // own is exactly what Linux forbids; shared credentials or an fs
            // struct used by another task would make the switch non-atomic.
            if Arc::ptr_eq(target, target_cred.user_ns_arc()) {
                return Err(KError::InvalidInput);
            }
            if process.threads().len() > 1 || process.fs_is_shared() {
                return Err(KError::InvalidInput);
            }
            require_target(target, CAP_SYS_ADMIN)
        }
        NsFd::Mnt(ns) => {
            require_target(ns.user_ns(), CAP_SYS_ADMIN)?;
            require_caller(CAP_SYS_CHROOT)?;
            require_caller(CAP_SYS_ADMIN)?;
            // Linux `prepare_nsset()` copies the fs_struct when NEWNS is
            // combined with another namespace flag; only a lone CLONE_NEWNS
            // request evaluates `fs->users != 1` on the caller's fs.
            if process.fs_is_shared() && !copy_fs {
                return Err(KError::InvalidInput);
            }
            Ok(())
        }
        NsFd::Pid(target) => {
            require_both_admin(nsfd.owner())?;
            ensure_pid_ns_reachable(process, target)
        }
        NsFd::Time(_) => {
            if process.threads().len() > 1 {
                return Err(KError::TooManyUsers);
            }
            require_both_admin(nsfd.owner())
        }
        NsFd::Cgroup(target) => {
            // `cgroupns_install()` checks the target owner first, then accepts
            // re-entering the active namespace as a successful no-op, and only
            // then requires the pending credential to be administrative. The
            // no-op must not demand a capability the caller does not need.
            require_target(target.owner(), CAP_SYS_ADMIN)?;
            require_caller(CAP_SYS_ADMIN)?;
            if Arc::ptr_eq(target, &process.cgroup_ns()?) {
                return Ok(());
            }
            Ok(())
        }
        NsFd::Uts(_) | NsFd::Ipc(_) | NsFd::Net(_) => require_both_admin(nsfd.owner()),
    }
}

/// Disconnect the calling process from parts of its namespace/share state
/// (`unshare(2)`).
///
/// All validation happens in the prepare phase (privilege, `NEWNS` with a
/// shared `fs_struct`, unsupported namespace kinds). The commit phase installs
/// the prepared bundle on the process, then unshares the calling thread's
/// descriptor table when `CLONE_FILES` was requested — the fd table is
/// thread-owned, so the detach must run on this thread's own binding.
pub(crate) fn sys_unshare(flags: usize) -> KResult<isize> {
    sys_unshare_with_stack(flags, knet::netns::ensure_stack)
}

fn sys_unshare_with_stack(
    flags: usize,
    prepare_stack: impl FnOnce(&kns::NetNamespace) -> KResult<()>,
) -> KResult<isize> {
    let process = current_user_process();
    let multithreaded = process.threads().len() > 1;
    let (namespace_flags, detach_fs, detach_files) = unshare_flag_set(flags as u64, multithreaded)?;

    let nsproxy = process.nsproxy()?;
    let fs_context = process.fs_context()?;
    let prepared = nsproxy
        .prepare_unshare(
            namespace_flags,
            UnshareContext {
                cred: kprocess::current_cred(),
                fs_shared_with_others: process.fs_is_shared(),
                unshare_fs: detach_fs,
                unshare_files: detach_files,
                current_cgroup: if namespace_flags.contains(NamespaceFlags::NEWCGROUP) {
                    kprocess::current_cgroup()
                } else {
                    None
                },
            },
            &fs_context.lock(),
        )
        .map_err(map_unshare_error)?;
    let unshare_files = prepared.unshare_files();
    if namespace_flags.contains(NamespaceFlags::NEWNET) {
        // Materialize the private protocol stack of the new namespace before
        // the caller can enter it or bind its `/proc/<pid>/ns/net` file.
        prepare_stack(prepared.nsproxy().net_ns())?;
    }
    process.apply_prepared_unshare(prepared)?;
    if unshare_files {
        current_files().unshare_fd_table()?;
    }
    Ok(0)
}

/// Join a namespace referenced by a namespace fd or a pidfd (`setns(2)`).
///
/// The caller must hold effective `CAP_SYS_ADMIN` (standing in for Linux's
/// per-type install checks); joining any namespace without it is `EPERM`. The
/// bad-fd error keeps priority by resolving the fd first.
///
/// The `NsFd` channel requires `nstype` to be zero or to match the fd's
/// namespace type (`EINVAL` otherwise). The `PidFd` channel implements the
/// Linux 5.8 semantics: `nstype` is an OR-combination mask naming namespaces
/// of the target process, every requested slot is validated against one
/// snapshot of the target, and the whole set is committed together.
pub(crate) fn sys_setns(fd: usize, nstype: usize) -> KResult<isize> {
    let process = current_user_process();
    let file = current_files().get_file(fd as i32)?;

    if let Ok(nsfd) = NsFd::from_file(&file) {
        return sys_setns_nsfd(&process, nsfd, nstype as u64);
    }
    if let Ok(pidfd) = PidFd::from_file(&file) {
        return sys_setns_pidfd(&process, pidfd, nstype as u64);
    }
    Err(KError::InvalidInput)
}

/// `setns` through the namespace-fd channel.
fn sys_setns_nsfd(process: &kprocess::Process, nsfd: Arc<NsFd>, nstype: u64) -> KResult<isize> {
    if nstype != 0 && nstype != nsfd.namespace_flag().bits() {
        return Err(KError::InvalidInput);
    }
    let cred = kprocess::current_cred();
    authorize_setns(process, &cred, &cred, &nsfd, false)?;

    let target_mnt = match &*nsfd {
        NsFd::Mnt(ns) => Some(ns.clone()),
        NsFd::Pid(ns) => {
            ensure_pid_ns_reachable(process, ns)?;
            None
        }
        _ => None,
    };
    let target_user_ns = match &*nsfd {
        NsFd::User(ns) => Some(ns.clone()),
        _ => None,
    };
    let new_proxy = process
        .nsproxy()?
        .with_namespace(&nsfd)
        .map_err(map_unshare_error)?;
    if let NsFd::Net(ns) = &*nsfd {
        knet::netns::ensure_stack(ns)?;
    }
    process.apply_setns(new_proxy, target_user_ns, target_mnt)?;
    Ok(0)
}

/// `setns` through the pidfd channel (Linux 5.8+).
///
/// `nstype` is a mask: each `CLONE_NEW*` bit selects one namespace of the
/// target process to enter. Namespace kinds without isolated backing tables
/// answer `EOPNOTSUPP` instead of silently half-switching.
fn sys_setns_pidfd(process: &kprocess::Process, pidfd: Arc<PidFd>, nstype: u64) -> KResult<isize> {
    let requested = NamespaceFlags::from_bits(nstype).ok_or(KError::InvalidInput)?;
    if requested.is_empty() {
        return Err(KError::InvalidInput);
    }
    // Resolve every requested slot against ONE snapshot of the target's
    // namespace bundle, then commit the whole set: slots must not be pinned
    // from different generations of the target's bundle, and nothing is
    // installed before every resolution succeeded. The mount slot is read
    // once and shared between the proxy swap and the fs root/pwd retarget.
    let target = pidfd.live_process()?;
    // Linux validates the whole namespace set with
    // `ptrace_may_access(tsk, PTRACE_MODE_READ_REALCREDS)` before any install
    // check, so an unreadable target reports `EPERM` even when its namespaces
    // would otherwise be enterable.
    let target_thread = kprocess::scheduler::representative_task(target)?;
    kprocess::ptrace::check_read_real_creds_access(
        &kprocess::current_user_thread(),
        target_thread.as_thread(),
    )?;
    let target_bundle = target.nsproxy()?;
    let target_user_ns = target.credentials_snapshot()?.user_ns_arc().clone();
    let mut swaps: Vec<NsFd> = Vec::new();
    let mut target_mnt: Option<Arc<kns::MntNamespace>> = None;
    for flag in [
        NamespaceFlags::NEWUSER,
        NamespaceFlags::NEWNS,
        NamespaceFlags::NEWUTS,
        NamespaceFlags::NEWIPC,
        NamespaceFlags::NEWPID,
        NamespaceFlags::NEWCGROUP,
        NamespaceFlags::NEWNET,
        NamespaceFlags::NEWTIME,
    ] {
        if !requested.contains(flag) {
            continue;
        }
        if flag == NamespaceFlags::NEWUSER {
            swaps.push(NsFd::User(target_user_ns.clone()));
        } else if flag == NamespaceFlags::NEWNS {
            let ns = target_bundle.mnt_ns().clone();
            target_mnt = Some(ns.clone());
            swaps.push(NsFd::Mnt(ns));
        } else if flag == NamespaceFlags::NEWUTS {
            swaps.push(NsFd::Uts(target_bundle.uts_ns().clone()));
        } else if flag == NamespaceFlags::NEWIPC {
            swaps.push(NsFd::Ipc(target_bundle.ipc_ns().clone()));
        } else if flag == NamespaceFlags::NEWPID {
            swaps.push(NsFd::Pid(target_bundle.pid_ns_for_children().clone()));
        } else if flag == NamespaceFlags::NEWCGROUP {
            swaps.push(NsFd::Cgroup(target_bundle.cgroup_ns().clone()));
        } else if flag == NamespaceFlags::NEWNET {
            swaps.push(NsFd::Net(target_bundle.net_ns().clone()));
        } else {
            swaps.push(NsFd::Time(target_bundle.time_ns().clone()));
        }
    }

    // Authorize the complete set before any namespace is installed, in
    // Linux's install order. A requested user namespace contributes a pending
    // credential with full capabilities in that namespace, exactly like the
    // `nsset->cred` Linux builds before validation. This is also where a PID
    // level escape and a multithreaded time-namespace switch are rejected.
    let current_cred = kprocess::current_cred();
    let caller_cred = if requested.contains(NamespaceFlags::NEWUSER) {
        current_cred.with_user_namespace(target_user_ns.clone())
    } else {
        (*current_cred).clone()
    };
    for nsfd in &swaps {
        authorize_setns(process, &current_cred, &caller_cred, nsfd, swaps.len() > 1)?;
    }
    for nsfd in &swaps {
        if let NsFd::Net(ns) = nsfd {
            knet::netns::ensure_stack(ns)?;
        }
    }

    let mut proxy = process.nsproxy()?;
    for nsfd in &swaps {
        proxy = proxy.with_namespace(nsfd).map_err(map_unshare_error)?;
    }
    if requested.contains(NamespaceFlags::NEWNET) {
        knet::netns::ensure_stack(proxy.net_ns())?;
    }
    let pending_user_ns = requested
        .contains(NamespaceFlags::NEWUSER)
        .then(|| target_user_ns.clone());
    process.apply_setns(proxy, pending_user_ns, target_mnt)?;
    Ok(0)
}

#[cfg(unittest)]
unittest::path_mod_test!(
    tests_namespace,
    "../../ut-tests/task/namespace/tests_namespace.rs"
);
