// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Cancellation syscalls implementation for TEE using session-level state

use core::ffi::c_uint;

use osvm::MemError;
use posix_types::UserPtr;
use tee_raw_sys::TeeTime;

use crate::tee::{
    TeeResult,
    tee_session::{TeeSessionCtx, with_tee_session_ctx, with_tee_session_ctx_mut},
};

fn map_user_mem_error(err: MemError) -> u32 {
    match err {
        MemError::InvalidAddr | MemError::NoAccess => tee_raw_sys::TEE_ERROR_BAD_PARAMETERS,
        _ => tee_raw_sys::TEE_ERROR_GENERIC,
    }
}

/// TEE_GetCancellationFlag
/// Returns 1 if the session cancel flag is set and not masked, otherwise 0.
/// Get the cancellation flag for the current session
/// Returns 1 if cancelled and unmasked, otherwise 0
pub fn sys_tee_scn_get_cancellation_flag(cancel: *mut c_uint) -> TeeResult {
    let is_cancelled = with_tee_session_ctx(|ctx| Ok(tee_ta_session_is_cancelled(ctx, None)))?;
    let flag: u32 = if is_cancelled { 1 } else { 0 };
    UserPtr::<c_uint>::from(cancel)
        .write_vm(flag)
        .map_err(map_user_mem_error)?;
    Ok(())
}

/// TEE_UnmaskCancellation
/// Unmasks cancellation at session level; returns previous masked state (1 if masked before).
/// If unmasking reveals a pending cancellation, interrupt the current task so cancellable
/// functions can detect the flag.
/// Unmask cancellation for the current session
/// Returns previous masked state
pub fn sys_tee_scn_unmask_cancellation(old_mask: *mut c_uint) -> TeeResult {
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
pub fn sys_tee_scn_mask_cancellation(old_mask: *mut c_uint) -> TeeResult {
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

fn tee_ta_session_is_cancelled(ctx: &TeeSessionCtx, curr_time: Option<&TeeTime>) -> bool {
    if ctx.cancel_mask {
        return false;
    }

    if ctx.cancel {
        return true;
    }

    if ctx.cancel_time.seconds == u32::MAX {
        return false;
    }

    let current_time = match curr_time {
        Some(time) => *time,
        None => tee_time_get_sys_time(),
    };

    if current_time.seconds > ctx.cancel_time.seconds
        || (current_time.seconds == ctx.cancel_time.seconds
            && current_time.millis >= ctx.cancel_time.millis)
    {
        return true;
    }
    false
}

fn tee_time_get_sys_time() -> TeeTime {
    let systiem = ktime::realtime();
    TeeTime {
        seconds: systiem.unix_seconds() as u32,
        millis: systiem.subsec_nanos() / 1_000_000,
    }
}
