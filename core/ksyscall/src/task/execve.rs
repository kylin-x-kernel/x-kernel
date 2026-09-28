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
use core::ffi::c_char;

use kerrno::{KError, KResult};
use kexec::{ExecRequest, load_prepared_user_app, prepare_user_app_request};
use khal::uspace::UserContext;
use ktask::current;
use kuaccess::vm_load_string;
use kvfs::{Filename, LookupFlags, LookupIntent};
use osvm::load_vec_until_null;

pub fn sys_execve(
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
    #[cfg(unittest)] mut probe: Option<&mut lifecycle_tests::ExecProbe>,
) -> KResult<isize> {
    debug!("sys_execve <= path: {path:?}, args: {args:?}, envs: {envs:?}");

    let curr = current();
    let thread = kprocess::current_user_thread();
    let process = thread.process().clone();

    if kprocess::scheduler::process_task_count(process.as_ref()) > 1 {
        // TODO: dispatch_irq multi-thread case
        error!("sys_execve: multi-thread not supported");
        return Err(KError::WouldBlock);
    }

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
    let absolute_path = loc
        .absolute_path()
        .map(|path| path.to_string())
        .unwrap_or_else(|_| path.clone());
    let entry_name = loc.name();

    let prepared = prepare_user_app_request(ExecRequest::from_resolved_with_display(
        loc,
        path,
        args.clone(),
        envs,
        cred,
    ))?;
    let exec_update = kprocess::ProcessExecUpdate::new(absolute_path.clone(), Arc::new(args));
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

    let new_mm = memspace::MmSpace::new_user_empty()?;
    // SAFETY: new_user_empty preserves kernel mappings. All request/metadata
    // inputs are owned, with no old-MM guard or user-copy window retained.
    // After exchange, only successful installation/context reset or fatal exit
    // can finish this syscall; the previous user context is never resumed.
    unsafe {
        kprocess::replace_current_address_space(
            new_mm,
            #[cfg(unittest)]
            probe
                .as_ref()
                .is_some_and(|probe| probe.should_fail_precheck()),
        )?;
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
        let updated = process.apply_exec_update(exec_update);
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
    exec_cred.apply_exec();
    thread.commit_creds(exec_cred);
    #[cfg(feature = "tee")]
    thread.reset_tee_session_after_exec();

    // This process no longer uses the old mappings, even if VM-sharing peers
    // retain them. Drop its stale attachment bookkeeping before new user entry.
    posix_ipc::SHM_MANAGER.lock().clear_proc_shm(process.pid());

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

#[cfg(unittest)]
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
