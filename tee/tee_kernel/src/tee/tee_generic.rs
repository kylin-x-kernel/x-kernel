// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ffi::c_char;

use kerrno::KError;
use knet::{
    ConnectOptions, SocketAddrEx, SocketOps,
    unix::{StreamTransport, UnixAddr, UnixDomainSocket},
};
use kprocess;
use tee_raw_sys::{
    TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_EXCESS_DATA, TEE_ERROR_GENERIC, TEE_ERROR_OUT_OF_MEMORY,
};

use crate::{
    mm::{VM_STRING_MAX_LEN, vm_load_string_with_len},
    tee::{
        TeeResult, protocal, protocal::TeeRequest, tee_session::with_tee_ta_ctx,
        tee_ta_manager::send_framed_message, uuid::ta_unix_socket_path,
    },
};

/// Return from a TEE syscall with a return code
pub(super) fn sys_tee_scn_return(_return_code: u32) -> TeeResult {
    // Now we just ignore the return code and return Ok
    Ok(())
}

/// Log a message from TEE userspace
pub(super) fn sys_tee_scn_log(buf: *const c_char, len: usize) -> TeeResult {
    // Implementation for TEE log syscall we use info to output the log now
    info!("TEE log syscall invoked with len: {}", len);

    // `len` is untrusted syscall input: reject an oversized record before it can
    // reach any heap reservation, so a bogus length fails as a normal error
    // instead of driving an unbounded allocation through the log path.
    if len > VM_STRING_MAX_LEN {
        debug!(
            "TEE log syscall rejected: len {} exceeds limit {}",
            len, VM_STRING_MAX_LEN
        );
        return Err(TEE_ERROR_EXCESS_DATA);
    }

    let message = match vm_load_string_with_len(buf, len) {
        Ok(message) => message,
        Err(KError::NoMemory) => return Err(TEE_ERROR_OUT_OF_MEMORY),
        Err(err) => {
            debug!("TEE log syscall failed to load user buffer: {:?}", err);
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
    };

    info!("TEE Log: {}", message);

    Ok(())
}

/// Kernel-direct panic notification. Wire format NOTE/TODO: [`crate::tee::protocal`].
pub(super) fn sys_tee_scn_panic(panic_code: u32) -> TeeResult {
    // Connect to current TA via Unix socket
    let socket = UnixDomainSocket::new(StreamTransport::new(kprocess::current_user_thread().pid()));
    let uuid = with_tee_ta_ctx(|ctx| Ok(ctx.uuid.clone()))?;
    let path = ta_unix_socket_path(&uuid)?;
    let remote_addr = SocketAddrEx::Unix(UnixAddr::Path(path.into()));
    socket
        .connect(remote_addr, ConnectOptions::default())
        .map_err(|_| TEE_ERROR_GENERIC)?;

    // Send panic command request to current TA
    let req = TeeRequest::Panic { panic_code };
    let encoded = protocal::encode_message(&req).map_err(|_| TEE_ERROR_GENERIC)?;
    send_framed_message(&socket, &encoded)?;
    Ok(())
}

#[unittest::mod_test]
pub(super) mod tests_tee_generic {
    use unittest::assert_eq;

    use super::*;

    #[unittest::def_test(user)]
    fn test_scn_log_normal_message() {
        let user_buf = crate::TestUserBuffer::new(5).unwrap();
        user_buf.write_bytes(b"loghi").unwrap();

        assert_eq!(sys_tee_scn_log(user_buf.as_user_ptr(), 5), Ok(()));
    }

    #[unittest::def_test(user)]
    fn test_scn_log_rejects_oversized_len() {
        // No mapped user buffer is supplied on purpose: the length check has to
        // reject these requests before any allocation or user-memory access.
        assert_eq!(
            sys_tee_scn_log(core::ptr::null(), usize::MAX),
            Err(TEE_ERROR_EXCESS_DATA)
        );
        assert_eq!(
            sys_tee_scn_log(core::ptr::null(), VM_STRING_MAX_LEN + 1),
            Err(TEE_ERROR_EXCESS_DATA)
        );
    }

    #[unittest::def_test(user)]
    fn test_scn_log_rejects_invalid_addr() {
        assert_eq!(
            sys_tee_scn_log(core::ptr::null(), 8),
            Err(TEE_ERROR_BAD_PARAMETERS)
        );
    }

    #[unittest::def_test(user)]
    fn test_scn_log_rejects_non_utf8() {
        let user_buf = crate::TestUserBuffer::new(1).unwrap();
        user_buf.write_bytes(&[0xFF]).unwrap();

        assert_eq!(
            sys_tee_scn_log(user_buf.as_user_ptr(), 1),
            Err(TEE_ERROR_BAD_PARAMETERS)
        );
    }
}
