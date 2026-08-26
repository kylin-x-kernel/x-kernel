// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Program execution syscalls.
//!
//! This module implements program execution operations including:
//! - Execute a new program (execve, execveat, execveeat, etc.)
//! - Program loading and initialization
//! - Argument and environment passing

use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::ffi::c_char;

use kaddr_layout::USER_HEAP_BASE;
use kerrno::{KError, KResult};
use kexec::{ExecRequest, load_user_app_request};
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

    let aspace_ref = process.address_space()?;
    let mut aspace = aspace_ref.lock();
    let load_result = load_user_app_request(
        &mut aspace,
        ExecRequest::from_resolved_with_display(
            loc.clone(),
            path.clone(),
            args.clone(),
            envs.clone(),
            cred,
        ),
    );
    let (entry_point, user_stack_base) = load_result?;
    drop(aspace);

    curr.set_name(entry_name.as_str());

    let exec_update =
        kprocess::ProcessExecUpdate::new(absolute_path.clone(), Arc::new(args), USER_HEAP_BASE);

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

    process.apply_exec_update(exec_update)?;
    let mut exec_cred = thread.prepare_creds();
    exec_cred.apply_exec();
    thread.commit_creds(exec_cred);
    thread.reset_after_exec();

    // execve replaces the entire address space.  The old mappings are
    // now destroyed (including any System-V shared memory attachments).
    // Clear the stale ShmManager entries so the process can re-attach
    // the same segments later.
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
