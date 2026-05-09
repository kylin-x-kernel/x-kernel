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
use kcore::mm::load_user_app;
use kerrno::{KError, KResult};
use khal::uspace::UserContext;
use kservices::mm::vm_load_string;
use ktask::current;
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
    let current_thread = kthread::current_thread();
    let proc_state = current_thread.process_state();

    if proc_state.proc.threads().len() > 1 {
        // TODO: dispatch_irq multi-thread case
        error!("sys_execve: multi-thread not supported");
        return Err(KError::WouldBlock);
    }

    let mut aspace = proc_state.address_space().lock();
    let (entry_point, user_stack_base) =
        load_user_app(&mut aspace, Some(path.as_str()), &args, &envs)?;
    drop(aspace);

    let loc = proc_state.fs_context().lock().resolve(&path)?;
    let absolute_path = loc.absolute_path()?.to_string();
    curr.set_name(loc.name());

    *proc_state.exe_path().write() = absolute_path.clone();
    *proc_state.cmdline().write() = Arc::new(args);

    #[cfg(feature = "tee")]
    {
        proc_state.tee_ta_ctx.write().init_ta_ctx(
            absolute_path.as_str(),
            tee_task_iface::tasign::get_ta_head_cached(absolute_path.as_str())?
                .unwrap_or_default()
                .as_slice(),
        );
    }

    proc_state.set_heap_top(USER_HEAP_BASE);
    proc_state.credentials.write().apply_exec();

    *proc_state.signal.actions.lock() = Default::default();

    // Clear set_child_tid after exec since the original address is no longer valid
    kthread::current_thread().set_clear_child_tid(0);

    // Close CLOEXEC file descriptors
    proc_state.resources.close_cloexec_files();

    uctx.set_ip(entry_point.as_usize());
    uctx.set_sp(user_stack_base.as_usize());
    Ok(0)
}
