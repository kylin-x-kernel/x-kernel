// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket address name syscalls.
//!
//! This module implements socket name operations including:
//! - Get socket name (getsockname, etc.)
//! - Get peer name (getpeername, etc.)
//! - Socket address queries

use kerrno::KResult;
use knet::{Socket, SocketOps};
use linux_raw_sys::net::{sockaddr, socklen_t};
use posix_types::UserPtr;

use crate::addr::SocketAddrExt;

/// Get the local address bound to a socket
pub fn sys_getsockname(
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> KResult<isize> {
    let socket = kprocess::current_resources().get_file_like_as::<Socket>(fd)?;
    let local_addr = socket.local_addr()?;
    debug!("sys_getsockname <= fd: {fd}, addr: {local_addr:?}");

    let mut addrlen_value = addrlen.read_vm()?;
    local_addr.write_to_user(addr, &mut addrlen_value)?;
    addrlen.write_vm(addrlen_value)?;
    Ok(0)
}

/// Get the address of the remote peer connected to a socket
pub fn sys_getpeername(
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> KResult<isize> {
    let socket = kprocess::current_resources().get_file_like_as::<Socket>(fd)?;
    let peer_addr = socket.peer_addr()?;
    debug!("sys_getpeername <= fd: {fd}, addr: {peer_addr:?}");

    let mut addrlen_value = addrlen.read_vm()?;
    peer_addr.write_to_user(addr, &mut addrlen_value)?;
    addrlen.write_vm(addrlen_value)?;
    Ok(0)
}
