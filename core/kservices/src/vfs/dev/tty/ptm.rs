// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::any::Any;

use fs_ng_vfs::{DeviceId, NodeType};
use kcore::vfs::{Device, DeviceOps, SimpleFs};
use kerrno::KResult;

/// Master pseudoterminal device (/dev/ptmx)
pub struct Ptmx(pub Arc<SimpleFs>);
impl Ptmx {
    /// Create a new pseudo-terminal
    pub fn create_pty(&self) -> KResult<(Arc<Device>, u32)> {
        let (master, slave) = super::pty::create_pty_pair();
        super::pts::add_slave(self.0.clone(), slave)?;
        let pty_number = master.pty_number();
        let device = Device::new(
            self.0.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(128, pty_number),
            master,
        );
        Ok((device, pty_number))
    }
}

// This is implemented as null-ops since opening `Ptmx` would result in a new
// tty file and these implementations wouldn't actually be used
impl DeviceOps for Ptmx {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> KResult<usize> {
        unreachable!()
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> KResult<usize> {
        unreachable!()
    }

    fn ioctl(&self, _cmd: u32, _arg: usize) -> KResult<usize> {
        unreachable!()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
