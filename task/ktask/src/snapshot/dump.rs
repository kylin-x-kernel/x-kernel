// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Low-level task backtrace dump helpers.
//!
//! All functions in this module are `pub(crate)` — external callers should use
//! the high-level interfaces from the parent `snapshot` module.
//!
//! Backtrace output is unified through [`backtrace::Backtrace`]'s `Display`
//! impl: raw frame-pointer addresses by default, optionally annotated with
//! the compact symbol table or fully symbolicated with DWARF. Host-side
//! symbolication consumes the raw address lines (`xkmake symbolize`).

use core::fmt;

use kcpu_id_map::LogicalCpuId;
use khal::context::TrapFrame;

#[inline(always)]
pub(crate) fn dump_println(force: bool, args: fmt::Arguments<'_>) {
    if force {
        khal::kprint_atomic!("{}", args);
    } else {
        log::error!("{}", args);
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub(crate) fn current_task_for_cpu(cpu_id: LogicalCpuId) -> Option<crate::KtaskRef> {
    let mut running = None;
    crate::task_registry::for_each_tracked_task(cpu_id, |weaktask| {
        if running.is_some() {
            return;
        }
        let Some(task) = weaktask.upgrade() else {
            return;
        };
        if task.inner().is_running() && task.inner().cpu_id() == cpu_id {
            running = Some(task);
        }
    });
    running
}

/// Dump backtraces for all non-running tasks on the given CPU.
#[cfg(target_arch = "aarch64")]
pub(crate) fn dump_cpu_task_backtrace(cpu_id: LogicalCpuId, force: bool) {
    crate::task_registry::for_each_tracked_task(cpu_id, |weaktask| {
        if let Some(task) = weaktask.upgrade()
            && !task.inner().is_running()
        {
            let ctx = task.inner().ctx();
            let bt = backtrace::Backtrace::capture_trap(
                ctx.r29 as usize, // fp
                ctx.lr as usize,  // ip
                ctx.lr as usize,  // ra
            );
            dump_println(
                force,
                format_args!("cpu_id: {}, {:?}\n{bt}", cpu_id.as_usize(), task.inner()),
            );
        }
    });
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn dump_cpu_task_backtrace(cpu_id: LogicalCpuId, force: bool) {
    crate::task_registry::for_each_tracked_task(cpu_id, |weaktask| {
        if let Some(task) = weaktask.upgrade()
            && has_stable_saved_task_context(&task)
            && let Some((rbp, rip)) = task.inner().ctx().backtrace_frame()
        {
            let bt = backtrace::Backtrace::capture_trap(rbp, rip, 0);
            dump_println(
                force,
                format_args!("cpu_id: {}, {:?}\n{bt}", cpu_id.as_usize(), task.inner()),
            );
        }
    });
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn has_stable_saved_task_context(task: &crate::KtaskRef) -> bool {
    let inner = task.inner();
    if inner.is_running() {
        return false;
    }

    #[cfg(feature = "smp")]
    if inner.on_cpu() {
        return false;
    }

    #[cfg(not(feature = "smp"))]
    if crate::current_may_uninit().is_some_and(|curr| curr.ptr_eq(task)) {
        return false;
    }

    true
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub(crate) fn dump_cpu_task_backtrace(_cpu_id: LogicalCpuId, _force: bool) {
    // Architecture not yet supported for task backtrace dumping.
}

/// Dump backtrace for the currently-running task on the given CPU using its trap frame.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn dump_cur_task_backtrace(cpu_id: LogicalCpuId, tf: &TrapFrame, force: bool) {
    let bt =
        backtrace::Backtrace::capture_trap(tf.x[29] as usize, tf.x[30] as usize, tf.x[30] as usize);
    dump_running_task_backtrace(cpu_id, force, &bt);
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) fn dump_cur_task_backtrace(cpu_id: LogicalCpuId, tf: &TrapFrame, force: bool) {
    let bt = backtrace::Backtrace::capture_trap(tf.rbp as usize, tf.rip as usize, 0);
    dump_running_task_backtrace(cpu_id, force, &bt);
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline(always)]
pub(crate) fn dump_cur_task_backtrace(_cpu_id: LogicalCpuId, _tf: &TrapFrame, _force: bool) {
    // Architecture not yet supported for task backtrace dumping.
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn dump_running_task_backtrace(cpu_id: LogicalCpuId, force: bool, bt: &backtrace::Backtrace) {
    // Zero-allocation on purpose: this runs on panic/NMI snapshot paths.
    match current_task_for_cpu(cpu_id).as_ref() {
        Some(task) => dump_println(
            force,
            format_args!("cpu_id: {}, {:?}\n{bt}", cpu_id.as_usize(), task.inner()),
        ),
        None => dump_println(
            force,
            format_args!(
                "cpu_id: {}, <running task unavailable>\n{bt}",
                cpu_id.as_usize()
            ),
        ),
    }
}
