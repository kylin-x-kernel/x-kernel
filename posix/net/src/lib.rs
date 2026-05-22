// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX network syscall implementations.

#![no_std]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod addr;
mod cmsg;
mod io;
mod name;
mod opt;
mod socket;

pub use self::{
    io::{sys_recvfrom, sys_recvmmsg, sys_recvmsg, sys_sendmmsg, sys_sendmsg, sys_sendto},
    name::{sys_getpeername, sys_getsockname},
    opt::{sys_getsockopt, sys_setsockopt},
    socket::{
        sys_accept, sys_accept4, sys_bind, sys_connect, sys_listen, sys_shutdown, sys_socket,
        sys_socketpair,
    },
};
