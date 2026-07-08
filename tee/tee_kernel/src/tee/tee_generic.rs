// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ffi::c_char;

use knet::{
    SocketAddrEx, SocketOps,
    unix::{StreamTransport, UnixAddr, UnixDomainSocket},
};
use kprocess;
use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_GENERIC};

use crate::{
    mm::vm_load_string_with_len,
    tee::{
        TeeResult, protocal, protocal::TeeRequest, tee_session::with_tee_ta_ctx,
        tee_ta_manager::send_framed_message, uuid::ta_unix_socket_path,
    },
};

/// Return from a TEE syscall with a return code
pub fn sys_tee_scn_return(_return_code: u32) -> TeeResult {
    // Now we just ignore the return code and return Ok
    Ok(())
}

/// Log a message from TEE userspace
pub fn sys_tee_scn_log(buf: *const c_char, len: usize) -> TeeResult {
    // Implementation for TEE log syscall we use info to output the log now
    info!("TEE log syscall invoked with len: {}", len);
    let message = match vm_load_string_with_len(buf, len) {
        Ok(s) => s,
        Err(_) => return Err(TEE_ERROR_BAD_PARAMETERS),
    };

    info!("TEE Log: {}", message);

    Ok(())
}

/// Kernel-direct panic notification. Wire format NOTE/TODO: [`crate::tee::protocal`].
pub fn sys_tee_scn_panic(panic_code: u32) -> TeeResult {
    // Connect to current TA via Unix socket
    let socket = UnixDomainSocket::new(StreamTransport::new(kprocess::current_user_thread().pid()));
    let uuid = with_tee_ta_ctx(|ctx| Ok(ctx.uuid.clone()))?;
    let path = ta_unix_socket_path(&uuid)?;
    let remote_addr = SocketAddrEx::Unix(UnixAddr::Path(path.into()));
    socket.connect(remote_addr).map_err(|_| TEE_ERROR_GENERIC)?;

    // Send panic command request to current TA
    let req = TeeRequest::Panic { panic_code };
    let encoded = protocal::encode_message(&req).map_err(|_| TEE_ERROR_GENERIC)?;
    send_framed_message(&socket, &encoded)?;
    Ok(())
}
