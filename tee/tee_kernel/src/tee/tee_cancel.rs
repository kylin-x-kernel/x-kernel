// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Cancellation syscalls implementation for TEE using session-level state

use core::ffi::c_uint;

use ktime_types::{MonotonicInstant, TimeSpan};
use osvm::MemError;
use posix_types::UserPtr;
use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_CANCEL};

use crate::tee::{
    TeeResult,
    tee_session::{TeeSessionCtx, with_tee_session_ctx, with_tee_session_ctx_mut},
};

fn map_user_mem_error(err: MemError) -> u32 {
    match err {
        MemError::OutOfMemory => tee_raw_sys::TEE_ERROR_OUT_OF_MEMORY,
        MemError::InvalidAddr | MemError::NoAccess => tee_raw_sys::TEE_ERROR_BAD_PARAMETERS,
        _ => tee_raw_sys::TEE_ERROR_GENERIC,
    }
}

/// TEE_GetCancellationFlag
/// Returns 1 if the session cancel flag is set and not masked, otherwise 0.
/// Get the cancellation flag for the current session
/// Returns 1 if cancelled and unmasked, otherwise 0
pub(super) fn sys_tee_scn_get_cancellation_flag(cancel: *mut c_uint) -> TeeResult {
    let is_cancelled = with_tee_session_ctx(|ctx| Ok(tee_ta_session_is_cancelled(ctx, None)))?;
    let flag: u32 = if is_cancelled { 1 } else { 0 };
    UserPtr::<c_uint>::from(cancel)
        .write_vm(flag)
        .map_err(map_user_mem_error)?;
    Ok(())
}

/// TEE_UnmaskCancellation
/// Unmasks cancellation at session level; returns previous masked state (1 if masked before).
/// Unmask cancellation for the current session
/// Returns previous masked state
pub(super) fn sys_tee_scn_unmask_cancellation(old_mask: *mut c_uint) -> TeeResult {
    let prev = with_tee_session_ctx_mut(|ctx| {
        let prev = ctx.cancel_mask;
        ctx.cancel_mask = false;
        Ok(prev)
    })?;
    let prev_mask: u32 = if prev { 1 } else { 0 };
    UserPtr::<c_uint>::from(old_mask)
        .write_vm(prev_mask)
        .map_err(map_user_mem_error)?;
    Ok(())
}

/// TEE_MaskCancellation
/// Masks cancellation at session level; returns previous masked state (1 if masked before).
/// Mask cancellation for the current session
/// Returns previous masked state
pub(super) fn sys_tee_scn_mask_cancellation(old_mask: *mut c_uint) -> TeeResult {
    let prev = with_tee_session_ctx_mut(|ctx| {
        let prev = ctx.cancel_mask;
        ctx.cancel_mask = true;
        Ok(prev)
    })?;
    let prev_mask: u32 = if prev { 1 } else { 0 };
    UserPtr::<c_uint>::from(old_mask)
        .write_vm(prev_mask)
        .map_err(map_user_mem_error)?;
    Ok(())
}

/// Private TA runtime ABI (572). Only the current thread's cancellation fields
/// change: operation 0 installs an absolute guest CLOCK_MONOTONIC ns deadline,
/// 1 ends the request, and 2 starts an unlimited request. Session mask and all
/// crypto/storage objects are preserved. A TA can only weaken its own cooperative
/// cancellation; this interface is not an authorization boundary for TA actions.
pub(super) fn sys_tee_scn_invoke_scope(operation: u32, deadline_ns: u64) -> TeeResult {
    let deadline = match operation {
        0 => Some(MonotonicInstant::from_span_since_origin(
            TimeSpan::from_nanos(deadline_ns),
        )),
        1 | 2 if deadline_ns == 0 => None,
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };
    with_tee_session_ctx_mut(|ctx| {
        ctx.cancel = false;
        ctx.cancel_deadline = deadline;
        Ok(())
    })
}

fn tee_ta_session_is_cancelled(ctx: &TeeSessionCtx, current: Option<MonotonicInstant>) -> bool {
    !ctx.cancel_mask
        && (ctx.cancel
            || ctx
                .cancel_deadline
                .is_some_and(|deadline| current.unwrap_or_else(ktask::monotonic_time) >= deadline))
}

/// Check cancellation and return the next deadline without holding thread state
/// across a sleep. Masked cancellation must not shorten an ordinary Wait.
pub(super) fn cancellation_wait_deadline() -> TeeResult<Option<MonotonicInstant>> {
    with_tee_session_ctx(|ctx| {
        if tee_ta_session_is_cancelled(ctx, None) {
            return Err(TEE_ERROR_CANCEL);
        }
        Ok(if ctx.cancel_mask {
            None
        } else {
            ctx.cancel_deadline
        })
    })
}

unittest::path_mod_test!(tests, "../../ut-tests/tee/tee_cancel/tests.rs");
