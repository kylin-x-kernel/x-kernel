// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Program execution syscalls.
//!
//! This module implements program execution operations including:
//! - Execute a new program (execve, execveat, execveeat, etc.)
//! - Program loading and initialization
//! - Argument and environment passing

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ffi::{c_char, c_int};

use kerrno::{KError, KResult, LinuxError};
use kexec::{ExecRequest, load_prepared_user_app, prepare_user_app_request};
use khal::uspace::UserContext;
use ktask::current;
use kuaccess::vm_load_string;
use kvfs::{Filename, LookupFlags, LookupIntent};
use osvm::load_vec_until_null;
use posix_process::exec_de_thread;

pub(crate) fn sys_execve(
    uctx: &mut UserContext,
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> KResult<isize> {
    let path = vm_load_string(path)?;

    let args = if argv.is_null() {
        // Handle NULL argv (treat as empty array)
        Vec::new()
    } else {
        load_vec_until_null(argv)?
            .into_iter()
            .map(vm_load_string)
            .collect::<Result<Vec<_>, _>>()?
    };

    let envs = if envp.is_null() {
        // Handle NULL envp (treat as empty array)
        Vec::new()
    } else {
        load_vec_until_null(envp)?
            .into_iter()
            .map(vm_load_string)
            .collect::<Result<Vec<_>, _>>()?
    };

    exec_owned(
        uctx,
        path,
        args,
        envs,
        #[cfg(unittest)]
        None,
    )
}

pub(super) fn exec_owned(
    uctx: &mut UserContext,
    path: String,
    args: Vec<String>,
    envs: Vec<String>,
    #[cfg(unittest)] probe: Option<&mut lifecycle_tests::ExecProbe>,
) -> KResult<isize> {
    debug!("sys_execve <= path: {path:?}, args: {args:?}, envs: {envs:?}");

    let process = kprocess::current_user_thread().process().clone();
    let fs_context = process.fs_context()?;
    let fs = fs_context.lock();
    let cred = kprocess::current_cred();
    let loc = Filename::new(path.as_str()).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Exec,
        LookupFlags::follow(),
        &cred,
    )?;
    drop(fs);
    exec_resolved(
        uctx,
        loc,
        path,
        args,
        envs,
        cred,
        #[cfg(unittest)]
        probe,
    )
}

/// `execveat(2)`: execute a program selected by `dirfd`/`pathname`.
///
/// Two forms are supported, matching what runc's memfd self-reexec
/// (glibc `fexecve`) needs:
/// - `AT_EMPTY_PATH` with an empty pathname: exec the file already open on
///   `dirfd` (anonymous memfd / tmpfs file); the display path is reported as
///   `/proc/self/fd/<dirfd>`.
/// - a non-empty pathname: resolve relative to `dirfd` (`AT_FDCWD` = caller
///   cwd) and exec, following symlinks like `execve`.
///
/// The final symlink is followed unless `AT_SYMLINK_NOFOLLOW` is set, in
/// which case a symlink target is rejected like Linux. Unknown flag bits are
/// rejected with `EINVAL`.
pub(crate) fn sys_execveat(
    uctx: &mut UserContext,
    dirfd: c_int,
    pathname: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
    flags: u32,
) -> KResult<isize> {
    use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};

    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW) != 0 {
        return Err(KError::from(LinuxError::EINVAL));
    }
    let lookup_flags = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        LookupFlags::empty()
    } else {
        LookupFlags::follow()
    };

    let (args, envs) = load_strings(argv, envp)?;
    let cred = kprocess::current_cred();

    // Linux requires a non-NULL pathname; `AT_EMPTY_PATH` selects the file
    // already open on `dirfd` only when the pathname is the empty string.
    if pathname.is_null() {
        return Err(KError::from(LinuxError::EFAULT));
    }
    let path = vm_load_string(pathname)?;

    if flags & AT_EMPTY_PATH != 0 && path.is_empty() {
        let file = kprocess::current_files().get_file(dirfd)?;
        // Linux exec's a regular file: an `AT_EMPTY_PATH` request naming a
        // non-regular descriptor (directory, FIFO, device, socket) is `EACCES`,
        // not a path re-resolution.
        if !file.path().is_regular_file() {
            return Err(KError::from(LinuxError::EACCES));
        }
        let display = alloc::format!("/proc/self/fd/{dirfd}");
        debug!("sys_execveat <= fd {dirfd} (AT_EMPTY_PATH), args: {args:?}");
        // Execute the file already open on `dirfd`, not a path re-resolution:
        // this is what makes `fexecve` on an anonymous `memfd`, or on a file
        // renamed after opening, execute the opened image.
        let entry_name = file.path().name();
        let request = ExecRequest::from_file(file, display.clone(), args, envs, cred);
        return exec_request(
            uctx,
            request,
            display,
            entry_name,
            #[cfg(unittest)]
            None,
        );
    }

    debug!("sys_execveat <= dirfd {dirfd}, path: {path:?}, args: {args:?}");
    let process = kprocess::current_user_thread().process().clone();
    // Snapshot the fs context before any descriptor lookup, matching the
    // `with_fs` pattern used by every other `*at` syscall: taking the fd-table
    // lock under the fs lock would invert the established order, and a live
    // fs would let a concurrent `chdir`/`chroot` move the lookup base.
    let fs = process.fs_context()?.lock().snapshot();
    let location = if path.as_str().chars().next().is_some_and(|c| c == '/') || dirfd == AT_FDCWD {
        Filename::new(path.as_str()).lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Exec,
            lookup_flags,
            &cred,
        )?
    } else {
        let dir_file = kprocess::current_files().get_file(dirfd)?;
        if !dir_file.is_dir() {
            return Err(KError::NotADirectory);
        }
        let snapshot = fs.clone_with_pwd(dir_file.path().clone())?;
        Filename::new(path.as_str()).lookup_at(
            snapshot.root(),
            snapshot.pwd(),
            LookupIntent::Exec,
            lookup_flags,
            &cred,
        )?
    };
    exec_resolved(
        uctx,
        location,
        path,
        args,
        envs,
        cred,
        #[cfg(unittest)]
        None,
    )
}

/// Returns whether the process must terminate instead of entering a new image.
///
/// Called after the thread-group surgery and the address-space swap. A fatal
/// signal delivered during that window, or a concurrent `exit_group(2)`, must be
/// taken here so `execve` never returns a live entry point for a dying process.
/// `fatal_snapshot` is the packed fatal-signal snapshot taken before the surgery.
fn abort_if_terminating(
    process: &Arc<kprocess::Process>,
    fatal_snapshot: Option<(u32, u32)>,
) -> bool {
    if let Ok(manager) = process.signal_manager() {
        let (epoch, signo) = manager.fatal_signal_snapshot();
        if fatal_snapshot.map(|snapshot| snapshot.0) != Some(epoch) {
            posix_process::do_exit(signo as i32, true);
            return true;
        }
    }
    if process.is_group_exited() {
        posix_process::do_exit(process.exit_code(), true);
        return true;
    }
    false
}

/// Loads `argv`/`envp` (NUL-terminated pointer arrays) into owned strings.
fn load_strings(
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> KResult<(Vec<String>, Vec<String>)> {
    let load = |ptr: *const *const c_char| -> KResult<Vec<String>> {
        if ptr.is_null() {
            return Ok(Vec::new());
        }
        load_vec_until_null(ptr)?
            .into_iter()
            .map(vm_load_string)
            .collect::<Result<Vec<_>, _>>()
    };
    Ok((load(argv)?, load(envp)?))
}

/// Prepares the resolved executable and swaps the caller's address space and
/// register context for the new program. Shared by `execve`/`execveat`.
fn exec_resolved(
    uctx: &mut UserContext,
    loc: kvfs::Path,
    display_path: String,
    args: Vec<String>,
    envs: Vec<String>,
    cred: Arc<kcred::Cred>,
    #[cfg(unittest)] probe: Option<&mut lifecycle_tests::ExecProbe>,
) -> KResult<isize> {
    let absolute_path = loc
        .absolute_path()
        .map(|path| path.to_string())
        .unwrap_or_else(|_| display_path.clone());
    let entry_name = loc.name();
    let request = ExecRequest::from_resolved_with_display(loc, display_path, args, envs, cred);
    exec_request(
        uctx,
        request,
        absolute_path,
        entry_name,
        #[cfg(unittest)]
        probe,
    )
}

/// Executes a prepared request by swapping the caller's address space and
/// register context for the new program.
///
/// `absolute_path` and `entry_name` describe the image for process metadata and
/// the task name; the argument vector is read from `request`.
fn exec_request(
    uctx: &mut UserContext,
    request: ExecRequest,
    absolute_path: String,
    entry_name: String,
    #[cfg(unittest)] mut probe: Option<&mut lifecycle_tests::ExecProbe>,
) -> KResult<isize> {
    let curr = current();
    let thread = kprocess::current_user_thread();
    let process = thread.process().clone();

    let args = request.args().to_vec();
    let prepared = prepare_user_app_request(request)?;
    // Everything that can still fail with a plain errno must run before the
    // first irreversible step. `de_thread()` is that step: once the siblings
    // are gone the old program cannot be resumed, so allocate the new address
    // space up front.
    let new_mm = memspace::MmSpace::new_user_empty()?;
    // Validate the whole image before joining the thread group, mirroring
    // Linux, which only reaches `de_thread()` inside `begin_new_exec` after the
    // binary format is confirmed. Go runtimes (runc init) keep helper threads
    // alive at exec time, so this is the normal path.
    // `de_thread()` is also required when the group leader already exited and
    // this is the sole surviving non-leader thread: the group pid must move onto
    // it before the image is installed, or number-indexed lookups (pidfd,
    // procfs, `tgkill`) would keep missing the survivor.
    // Snapshot the fatal-signal counter before the thread-group surgery so a
    // fatal signal (or group exit) arriving during it is detected before the
    // new image is entered.
    let fatal_snapshot = process
        .signal_manager()
        .ok()
        .map(|manager| manager.fatal_signal_snapshot());
    let needs_takeover = kprocess::scheduler::process_task_count(process.as_ref()) > 1
        || thread.tid() != process.pid();
    if needs_takeover && exec_de_thread() {
        // The takeover failed and the current thread was made to exit. Do not
        // swap the address space or install the image: continuing would return
        // to the fatal path and run the thread teardown a second time.
        return Ok(0);
    }
    // Pin the backing executable for `/proc/<pid>/exe` before `prepared` is
    // consumed by installation.
    let exe_file = prepared.executable_file();
    let exec_update = kprocess::ProcessExecUpdate::new(absolute_path.clone(), Arc::new(args))
        .with_exe_file(exe_file);
    #[cfg(unittest)]
    let (prepared, exec_update) = if let Some(probe) = probe.as_deref_mut() {
        probe.attach_failure_probes(prepared, exec_update)
    } else {
        (prepared, exec_update)
    };

    #[cfg(feature = "tee")]
    let exec_update = {
        #[cfg(feature = "tee_ta_sign")]
        let ta_head_bytes = tee_task_iface::tasign::get_ta_head_cached(absolute_path.as_str())
            .unwrap_or_default()
            .unwrap_or_default();
        #[cfg(not(feature = "tee_ta_sign"))]
        let ta_head_bytes =
            tee_task_iface::ta_ctx::read_ta_head_if_applicable(absolute_path.as_str())
                .unwrap_or_default()
                .unwrap_or_default();
        exec_update.with_ta_head_bytes(ta_head_bytes)
    };

    // From `de_thread()` onward the old thread group is already gone: a failure
    // here is fatal rather than an errno the caller could resume from with a
    // vanished thread group. A single-threaded exec has no such step, so it
    // keeps returning the error with its state intact.
    // SAFETY: new_user_empty preserves kernel mappings. All request/metadata
    // inputs are owned, with no old-MM guard or user-copy window retained.
    let replaced = unsafe {
        kprocess::replace_current_address_space(
            new_mm,
            #[cfg(unittest)]
            probe
                .as_ref()
                .is_some_and(|probe| probe.should_fail_precheck()),
        )
    };
    if let Err(error) = replaced {
        if needs_takeover {
            error!("sys_execve: address-space swap failed after de_thread; terminating: {error}");
            posix_process::do_exit(ksignal::Signo::SIGSEGV as i32, true);
            return Ok(0);
        }
        return Err(error);
    }
    thread.reset_user_state_after_exec();
    // Keep every fallible post-exchange operation inside this fatal boundary.
    // The closure releases temporary MM owners and locks before do_exit.
    let committed: KResult<_> = (|| {
        #[cfg(unittest)]
        if let Some(probe) = probe.as_deref_mut() {
            probe.on_activated()?;
        }
        let image = {
            let aspace = process.address_space()?;
            load_prepared_user_app(&mut aspace.lock(), prepared)?
        };
        curr.set_name(entry_name.as_str());
        let updated = thread.apply_exec_update(exec_update);
        #[cfg(unittest)]
        if let Some(probe) = probe {
            probe.observe_update(&process, image.0.as_usize(), image.1.as_usize());
        }
        updated?;
        Ok(image)
    })();
    let (entry_point, user_stack_base) = match committed {
        Ok(image) => image,
        Err(error) => {
            error!("sys_execve: image commit failed; terminating current process: {error}");
            posix_process::do_exit(ksignal::Signo::SIGSEGV as i32, true);
            return Ok(0);
        }
    };
    let mut exec_cred = thread.prepare_creds();
    exec_cred.apply_exec(thread.no_new_privileges());
    thread.commit_creds(exec_cred);
    #[cfg(feature = "tee")]
    thread.reset_tee_session_after_exec();

    // This process no longer uses the old mappings, even if VM-sharing peers
    // retain them. Drop its stale attachment bookkeeping before new user entry.
    posix_ipc::SHM_MANAGER.lock().clear_proc_shm(process.pid());

    // A fatal signal or group exit may have arrived after `exec_de_thread`'s own
    // check. Take it here instead of entering a program that is already doomed;
    // the trap-return drain would catch it, but this keeps the exec boundary
    // explicit and never returns a live entry point for a dying process.
    if abort_if_terminating(&process, fatal_snapshot) {
        return Ok(0);
    }

    // execve replaces the whole user register context.  Do not inherit any
    // general-purpose register of the old program: the x86_64 `execve`
    // argument `envp` sits in `rdx`, which static glibc entry reads as the
    // `rtld_fini` callback and later calls during exit handling, crashing on
    // the stale address.  Re-establish only the fields required by the target
    // architecture's ELF entry ABI.
    uctx.reset_for_exec();
    uctx.set_ip(entry_point.as_usize());
    uctx.set_sp(user_stack_base.as_usize());
    uctx.set_tls(0);
    Ok(0)
}

#[unittest::mod_test]
mod tests {
    use alloc::format;

    use ksignal::SignalStack;
    use unittest::def_test;

    use super::*;

    #[def_test(user, serial)]
    fn empty_exec_path_preserves_current_image() {
        let thread = kprocess::current_user_thread();
        let process = thread.process();
        let signal = thread.signal_manager();
        let original_tid = thread.clear_child_tid();
        let original_robust = thread.robust_list_head();
        let original_stack = signal.stack();
        let mm_id = process.mm_id().unwrap();
        let heap = process.heap_top().unwrap();
        let snapshot = || {
            process
                .address_space()
                .unwrap()
                .lock()
                .vmas()
                .map(|vma| (vma.start(), vma.end(), vma.flags()))
                .collect::<Vec<_>>()
        };
        let mappings = snapshot();
        let mut uctx = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
        uctx.set_tls(0x9000);
        // Debug includes private registers without reading struct padding.
        let context = format!("{uctx:?}");
        // These registrations are observed only; this path must never dereference them.
        thread.set_clear_child_tid(0x4000);
        thread.set_robust_list_head(0x5000);
        signal.set_stack(SignalStack {
            sp: 0x6000,
            flags: 0,
            size: 0x2000,
        });

        assert_eq!(
            exec_owned(&mut uctx, String::new(), Vec::new(), Vec::new(), None),
            Err(KError::NotFound)
        );
        assert_eq!(process.mm_id().unwrap(), mm_id);
        assert_eq!(snapshot(), mappings);
        assert_eq!(process.heap_top().unwrap(), heap);
        assert_eq!(thread.clear_child_tid(), 0x4000);
        assert_eq!(thread.robust_list_head(), 0x5000);
        let stack = signal.stack();
        assert_eq!((stack.sp, stack.flags, stack.size), (0x6000, 0, 0x2000));
        assert_eq!(format!("{uctx:?}"), context);
        assert!(!thread.is_exiting());
        thread.set_clear_child_tid(original_tid);
        thread.set_robust_list_head(original_robust);
        signal.set_stack(original_stack);
    }
}

#[cfg(unittest)]
#[path = "tests/exec_lifecycle.rs"]
pub(super) mod lifecycle_tests;
