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
use kexec::{
    ExecContext, ExecRequest, PreparedExecImage, PrivateFileViewSpec, load_prepared_user_app,
    prepare_user_app_request,
};
use khal::uspace::UserContext;
use ktask::current;
use kuaccess::vm_load_string;
use kvfs::{Filename, LookupFlags, LookupIntent};
use memaddr::VirtAddr;
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
        let request = ExecRequest::from_file(file, display.clone(), args, envs, cred)
            .with_ids_allowed(!kprocess::current_user_thread().no_new_privileges());
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
    // The loader samples the resolved image itself, after script resolution:
    // a script contributes its interpreter's bits, never its own.
    let request = ExecRequest::from_resolved_with_display(loc, display_path, args, envs, cred)
        .with_ids_allowed(!kprocess::current_user_thread().no_new_privileges());
    exec_request(
        uctx,
        request,
        absolute_path,
        entry_name,
        #[cfg(unittest)]
        probe,
    )
}

/// Samples the exec-time facts under the admission guard and rebuilds the
/// initial stack and vectors from them.
///
/// The returned credential carries the same sample the vectors were built
/// from, so `AT_SECURE`/`AT_EUID`/`AT_EGID` cannot disagree with the
/// credentials committed after the image installs.
fn finalize_exec_stack(
    prepared: &mut PreparedExecImage,
    thread: &kprocess::CurrentThread,
    exec_fs: &kprocess::ExecFsGuard,
) -> KResult<kcred::Cred> {
    let context = ExecContext {
        ids_allowed: !thread.no_new_privileges(),
        fs_shared: exec_fs.is_shared_with_other_process(),
    };
    let security = prepared.finalize_stack(context)?;
    // Apply the transition to the real credential before the address space is
    // replaced: an effective file capability set that cannot be delivered must
    // fail the exec with `EPERM` while an errno is still returnable.
    let mut exec_cred = thread.prepare_creds();
    exec_cred.apply_exec_with_security(!context.ids_allowed, context.fs_shared, security)?;
    Ok(exec_cred)
}

/// Loads the prepared image into the process address space, registering its
/// private file views for cache invalidation.
///
/// - `process`: target process whose address space receives the image.
/// - `prepared`: the prepared executable and interpreter image.
///
/// Returns: the image entry point and initial user stack pointer.
///
/// Errors: `InvalidInput` when user VMAs already exist; other errors propagate
/// unchanged.
///
/// Locking: holds the address-space mutex while mapping and registering; must
/// not be called while already holding it.
fn load_exec_image(
    process: &kprocess::Process,
    prepared: PreparedExecImage,
) -> KResult<(VirtAddr, VirtAddr)> {
    let aspace = process.address_space()?;
    aspace.with_mapping_owner(move |mut mapping| {
        let observer = mapping.observer();
        let view = PrivateFileViewSpec {
            mm_id: mapping.aspace().mm_id(),
            observer: &observer,
            invalidate: mapping.invalidate_handle(),
        };
        load_prepared_user_app(mapping.aspace_mut(), prepared, Some(view))
    })
}

/// Loads and validates the new image before the thread group is torn down.
///
/// The initial stack and auxiliary vector are rebuilt later, under the exec
/// admission guard, so they agree with the committed credentials.
fn prepare_exec_image(
    request: ExecRequest,
    #[cfg(unittest)] probe: Option<&mut lifecycle_tests::ExecProbe>,
) -> KResult<PreparedExecImage> {
    let prepared = prepare_user_app_request(request)?;
    #[cfg(unittest)]
    let prepared = match probe {
        Some(probe) => probe.prepare_image_probe(prepared)?,
        None => prepared,
    };
    Ok(prepared)
}

/// Executes a prepared request by swapping the caller's address space and
/// register context for the new program.
///
/// `absolute_path` and `entry_name` describe the image for process metadata and
/// the task name; the argument vector is read from `request`.
#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: the exec transaction keeps exchange, credential commit, \
              image installation and aux publication in one ordered sequence"
)]
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
    let mut prepared = prepare_exec_image(
        request,
        #[cfg(unittest)]
        probe.as_deref_mut(),
    )?;
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
    // Linux's `check_unsafe_exec()` samples the filesystem sharing state and
    // sets `in_exec` in one step after `de_thread()`. Keeping the guard in
    // scope until credentials are committed blocks a concurrent `CLONE_FS`
    // child from attaching after the unsafe-share sample. The guard also
    // clears `in_exec` on every error path below.
    let exec_fs = process.begin_exec()?;
    // One guard-time sample drives both the auxiliary vector and the commit.
    let exec_cred = finalize_exec_stack(&mut prepared, &thread, &exec_fs)?;
    // Pin the backing executable for `/proc/<pid>/exe` before `prepared` is
    // consumed by installation.
    let exe_file = prepared.executable_file();
    let exec_update = kprocess::ProcessExecUpdate::new(absolute_path.clone(), Arc::new(args))
        .with_exe_file(exe_file)
        .with_auxv(prepared.auxv_bytes().to_vec());
    #[cfg(unittest)]
    let exec_update = if let Some(probe) = probe.as_deref() {
        probe.attach_update_probe(exec_update)
    } else {
        exec_update
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
    if !needs_takeover {
        // A single-threaded exec keeps the old image on a precheck failure, but
        // once the exchange succeeds the process is committed to the new one.
        // Linux `de_thread()` has already established the new parent contract
        // by this point; mirror that before any later operation can fail.
        process.commit_exec_parent_contract();
    }
    thread.reset_user_state_after_exec();
    // Capture the exec-time auxv before the prepared image is consumed; it is
    // committed with the new image so `prctl(PR_GET_AUXV)` reports the vector
    // the program entry actually received.
    let exec_saved_aux = prepared.saved_aux_words();
    // Keep every fallible post-exchange operation inside this fatal boundary.
    // The closure releases temporary MM owners and locks before do_exit.
    let committed: KResult<_> = (|| {
        #[cfg(unittest)]
        if let Some(probe) = probe.as_deref_mut() {
            probe.on_activated()?;
        }
        let image = load_exec_image(&process, prepared)?;
        process.set_saved_aux(exec_saved_aux);
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

unittest::path_mod_test!(tests, "../../ut-tests/task/execve/tests.rs");

#[cfg(unittest)]
unittest::path_mod_test!(pub(super) mod lifecycle_tests, "../../ut-tests/task/tests/exec_lifecycle.rs");
