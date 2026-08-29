// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Link-layer socket support.

use kerrno::{KError, KResult};

use crate::SERVICE;

pub(crate) mod buf;
pub mod packet;
pub(crate) mod wire;

/// Sends a complete link-layer frame through the interface identified by `ifindex`.
pub fn send_link_frame(ifindex: i32, frame: &[u8]) -> KResult<usize> {
    if !SERVICE.is_inited() {
        return Err(KError::OperationNotSupported);
    }
    SERVICE.send_link_frame(ifindex, frame)
}
