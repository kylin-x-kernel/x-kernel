// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Control message handling for socket operations.
//!
//! This module provides parsing and handling of control messages (ancillary data)
//! in socket I/O operations, including file descriptor passing and other protocol-specific data.

use alloc::{sync::Arc, vec::Vec};

use kerrno::{KError, KResult};
use linux_raw_sys::net::{SCM_RIGHTS, SOL_SOCKET, cmsghdr};

use crate::{
    file::{FileLike, get_file_like},
    mm::{UserConstPtr, UserPtr},
};

/// Control message types for socket operations (ancillary data)
pub enum CMsg {
    /// SCM_RIGHTS: file descriptor passing between processes
    Rights { fds: Vec<Arc<dyn FileLike>> },
}
impl CMsg {
    /// Parse a control message header and extract its data
    pub fn parse(hdr: &cmsghdr) -> KResult<Self> {
        if hdr.cmsg_len < size_of::<cmsghdr>() {
            return Err(KError::InvalidInput);
        }

        let data =
            UserConstPtr::<u8>::from((hdr as *const cmsghdr as usize) + size_of::<cmsghdr>())
                .get_as_slice(hdr.cmsg_len - size_of::<cmsghdr>())?;
        Ok(match (hdr.cmsg_level as u32, hdr.cmsg_type as u32) {
            (SOL_SOCKET, SCM_RIGHTS) => {
                if data.len() % size_of::<i32>() != 0 {
                    return Err(KError::InvalidInput);
                }
                let mut fds = Vec::new();
                for fd in data.chunks_exact(size_of::<i32>()) {
                    let fd = i32::from_ne_bytes(fd.try_into().unwrap());
                    if fd < 0 {
                        return Err(KError::BadFileDescriptor);
                    }
                    let f = get_file_like(fd)?;
                    fds.push(f);
                }
                Self::Rights { fds }
            }
            _ => {
                return Err(KError::InvalidInput);
            }
        })
    }
}

/// Builder for constructing control message buffers for socket I/O
pub struct CMsgBuilder<'a> {
    hdr: UserPtr<cmsghdr>,
    len: &'a mut usize,
    capacity: usize,
}
impl<'a> CMsgBuilder<'a> {
    /// Create a new control message builder with a given buffer and capacity
    pub fn new(msg: UserPtr<cmsghdr>, len: &'a mut usize) -> Self {
        let capacity = *len;
        *len = 0;
        Self {
            hdr: msg,
            len,
            capacity,
        }
    }

    /// Add a control message with the specified level and type to the buffer
    pub fn push(
        &mut self,
        level: u32,
        ty: u32,
        body: impl FnOnce(&mut [u8]) -> KResult<usize>,
    ) -> KResult<bool> {
        let Some(body_capacity) = (self.capacity - *self.len).checked_sub(size_of::<cmsghdr>())
        else {
            return Ok(false);
        };

        let hdr = self.hdr.get_as_mut()?;
        hdr.cmsg_level = level as _;
        hdr.cmsg_type = ty as _;

        let data = UserPtr::<u8>::from(self.hdr.address().as_usize() + size_of::<cmsghdr>())
            .get_as_mut_slice(body_capacity)?;
        let body_len = body(data)?;

        let cmsg_len = size_of::<cmsghdr>() + body_len;
        hdr.cmsg_len = cmsg_len;
        self.hdr = UserPtr::from(hdr as *const _ as usize + cmsg_len);
        *self.len += cmsg_len;
        Ok(true)
    }
}
