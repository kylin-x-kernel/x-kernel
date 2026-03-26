// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network syscalls.
//!
//! This module implements network operations including:
//! - Socket creation and management (socket, bind, listen, connect, etc.)
//! - Socket I/O operations (send, recv, sendto, recvfrom, etc.)
//! - Socket options (getsockopt, setsockopt, etc.)
//! - Network name resolution (getaddrinfo, etc.)
//! - Socket address handling (getsockname, getpeername, etc.)

mod cmsg;
mod io;
mod name;
mod opt;
mod socket;

pub use self::{cmsg::*, io::*, name::*, opt::*, socket::*};
