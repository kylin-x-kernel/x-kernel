// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel-direct inter-TA RPC. Wire format NOTE/TODO: [`protocal`](protocal).

use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use knet::{
    ConnectOptions, RecvOptions, SendOptions, SocketAddrEx, SocketOps,
    unix::{StreamTransport, UnixAddr, UnixDomainSocket},
};
use kprocess;
use tee_raw_sys::{TEE_ERROR_GENERIC, TEE_ERROR_ITEM_NOT_FOUND, TEE_SUCCESS, utee_params};
use tee_task_iface::SessionIdentity;

use crate::tee::{
    TeeResult,
    protocal::{self, Parameters, TeeRequest, TeeResponse},
    tee_session::{with_tee_ta_ctx, with_tee_ta_ctx_mut},
    uuid::{Uuid, ta_unix_socket_path},
};

const TEE_TA_MAX_FRAME_PAYLOAD_LEN: usize = 64 * 1024;

fn validated_payload_len(len: usize) -> TeeResult<usize> {
    if len > TEE_TA_MAX_FRAME_PAYLOAD_LEN {
        Err(TEE_ERROR_GENERIC)
    } else {
        Ok(len)
    }
}

fn recv_exact(socket: &UnixDomainSocket, buf: &mut [u8]) -> TeeResult<()> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = socket
            .recv(&mut buf[offset..], RecvOptions::default())
            .map_err(|_| TEE_ERROR_GENERIC)?;
        if n == 0 {
            return Err(TEE_ERROR_GENERIC);
        }
        offset += n;
    }
    Ok(())
}

fn recv_framed_payload(socket: &UnixDomainSocket) -> TeeResult<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    recv_exact(socket, &mut len_buf)?;
    let len = validated_payload_len(u32::from_ne_bytes(len_buf) as usize)?;
    let mut payload = vec![0u8; len];
    recv_exact(socket, &mut payload)?;
    Ok(payload)
}

pub(crate) fn send_framed_message(socket: &UnixDomainSocket, encoded: &[u8]) -> TeeResult<()> {
    validated_payload_len(encoded.len())?;
    let mut message = Vec::with_capacity(4 + encoded.len());
    message.extend_from_slice(&(encoded.len() as u32).to_ne_bytes());
    message.extend_from_slice(encoded);
    socket
        .send(message.as_slice(), SendOptions::default())
        .map_err(|_| TEE_ERROR_GENERIC)?;
    Ok(())
}

fn recv_tee_response(socket: &UnixDomainSocket) -> TeeResult<TeeResponse> {
    let payload = recv_framed_payload(socket)?;
    protocal::decode_message(&payload).map_err(|_| TEE_ERROR_GENERIC)
}

pub fn tee_ta_init_session(uuid: String) -> TeeResult<u32> {
    let parsed = Uuid::parse_str(&uuid)?;
    let path = parsed.ta_unix_socket_path();
    let uuid = parsed.to_string();
    // Connect to dest TA via Unix socket
    let socket = UnixDomainSocket::new(StreamTransport::new(kprocess::current_user_thread().pid()));
    let remote_addr = SocketAddrEx::Unix(UnixAddr::Path(path.into()));
    socket
        .connect(remote_addr, ConnectOptions::default())
        .map_err(|_| TEE_ERROR_GENERIC)?;

    // Send open session request to dest TA
    let req = TeeRequest::OpenSession {
        params: Parameters::default(),
        uuid: uuid.clone(),
        connection_method: 0,
    };
    let encoded = protocal::encode_message(&req).map_err(|_| TEE_ERROR_GENERIC)?;
    send_framed_message(&socket, &encoded)?;

    let resp = recv_tee_response(&socket)?;
    match resp {
        TeeResponse::OpenSession { session_id, result } => match result {
            TEE_SUCCESS => with_tee_ta_ctx_mut(|ctx| {
                let dispatch_irq = ctx.session_dispatch_irq;
                ctx.open_sessions
                    .insert(dispatch_irq, SessionIdentity { uuid, session_id });
                ctx.session_dispatch_irq += 1;
                Ok(dispatch_irq)
            }),
            _ => Err(result),
        },
        _ => Err(TEE_ERROR_GENERIC),
    }
}

pub fn tee_ta_close_session(sess_id: SessionIdentity) -> TeeResult {
    // Connect to dest TA via Unix socket
    let socket = UnixDomainSocket::new(StreamTransport::new(kprocess::current_user_thread().pid()));
    let path = ta_unix_socket_path(&sess_id.uuid)?;
    let remote_addr = SocketAddrEx::Unix(UnixAddr::Path(path.into()));
    socket
        .connect(remote_addr, ConnectOptions::default())
        .map_err(|_| TEE_ERROR_GENERIC)?;

    // Send close session request to dest TA
    let req = TeeRequest::CloseSession {
        session_id: sess_id.session_id,
    };
    let encoded = protocal::encode_message(&req).map_err(|_| TEE_ERROR_GENERIC)?;
    send_framed_message(&socket, &encoded)?;

    Ok(())
}

pub fn tee_ta_invoke_command(
    sess_id: SessionIdentity,
    cmd_id: u32,
    _usr_param: *mut utee_params,
) -> TeeResult {
    // Connect to dest TA via Unix socket
    let socket = UnixDomainSocket::new(StreamTransport::new(kprocess::current_user_thread().pid()));
    let path = ta_unix_socket_path(&sess_id.uuid)?;
    let remote_addr = SocketAddrEx::Unix(UnixAddr::Path(path.into()));
    socket
        .connect(remote_addr, ConnectOptions::default())
        .map_err(|_| TEE_ERROR_GENERIC)?;

    // Send invoke command request to dest TA
    let req = TeeRequest::InvokeCommand {
        session_id: sess_id.session_id,
        cmd_id,
        params: Parameters::default(),
    };
    let encoded = protocal::encode_message(&req).map_err(|_| TEE_ERROR_GENERIC)?;
    send_framed_message(&socket, &encoded)?;

    let resp = recv_tee_response(&socket)?;
    match resp {
        TeeResponse::InvokeCommand { params: _, result } => match result {
            TEE_SUCCESS => Ok(()),
            _ => Err(result),
        },
        _ => Err(TEE_ERROR_GENERIC),
    }
}

pub fn tee_ta_get_session(dispatch_irq: u32) -> TeeResult<SessionIdentity> {
    with_tee_ta_ctx(|ctx| match ctx.open_sessions.get(&dispatch_irq) {
        Some(sess_id) => Ok(sess_id.clone()),
        None => Err(TEE_ERROR_ITEM_NOT_FOUND),
    })
}

#[cfg(unittest)]
mod tests {
    #[test]
    fn validated_payload_len_accepts_within_limit() {
        assert_eq!(
            validated_payload_len(TEE_TA_MAX_FRAME_PAYLOAD_LEN).unwrap(),
            TEE_TA_MAX_FRAME_PAYLOAD_LEN
        );
    }

    #[test]
    fn validated_payload_len_rejects_oversized_frames() {
        assert_eq!(
            validated_payload_len(TEE_TA_MAX_FRAME_PAYLOAD_LEN + 1),
            Err(TEE_ERROR_GENERIC)
        );
    }
}
