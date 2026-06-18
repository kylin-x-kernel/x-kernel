// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::string::ToString;
use core::ffi::{c_uint, c_ulong};

use osvm::MemError;
use posix_types::UserConstPtr;
use tee_raw_sys::{TEE_UUID, utee_params};

use crate::tee::{
    TeeResult,
    tee_ta_manager::{
        tee_ta_close_session, tee_ta_get_session, tee_ta_init_session, tee_ta_invoke_command,
    },
    uuid::Uuid,
};

fn map_user_mem_error(err: MemError) -> u32 {
    match err {
        MemError::InvalidAddr | MemError::NoAccess => tee_raw_sys::TEE_ERROR_BAD_PARAMETERS,
        _ => tee_raw_sys::TEE_ERROR_GENERIC,
    }
}

/// Open a session to another TEE application
pub fn sys_tee_scn_open_ta_session(
    dest: *const TEE_UUID,
    _cancel_req_to: c_ulong,
    _usr_param: *mut utee_params,
    _ta_sees: *mut c_uint,
    _ret_orig: *mut c_uint,
) -> TeeResult {
    let uuid = UserConstPtr::<TEE_UUID>::from(dest)
        .read_vm()
        .map_err(map_user_mem_error)?;

    tee_ta_init_session(Uuid::from(uuid).to_string())?;

    Ok(())
}

/// Close a session to another TEE application
pub fn sys_tee_scn_close_ta_session(ta_sees: c_ulong) -> TeeResult {
    let sess_id = tee_ta_get_session(ta_sees as u32)?;
    tee_ta_close_session(sess_id)?;
    Ok(())
}

/// Invoke a command in another TEE application
pub fn sys_tee_scn_invoke_ta_command(
    ta_sees: c_ulong,
    _cancel_req_to: c_ulong,
    cmd_id: c_ulong,
    usr_param: *mut utee_params,
    _ret_orig: *mut c_uint,
) -> TeeResult {
    let sess_id = tee_ta_get_session(ta_sees as u32)?;
    tee_ta_invoke_command(sess_id, cmd_id as u32, usr_param)?;
    Ok(())
}
