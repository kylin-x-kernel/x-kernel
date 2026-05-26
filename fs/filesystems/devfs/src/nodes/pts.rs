// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pseudo-terminal slave directory and master multiplexer.

use alloc::{borrow::Cow, boxed::Box, string::ToString, sync::Arc, vec::Vec};
use core::any::Any;

use flatten_objects::FlattenObjects;
use kerrno::{KError, KResult};
use kspin::SpinNoIrq;
use ktty::tty::{PtyDriver, create_pty_pair};
use kvfs::{DeviceFileOps, DeviceId, NodeType, VfsResult};
use kvfs_simple::{NodeOpsMux, SimpleDirOps, SimpleFs};

use crate::DeviceFile;

static PTS_TABLE: SpinNoIrq<FlattenObjects<Arc<DeviceFile>, 16>> =
    SpinNoIrq::new(FlattenObjects::new());

/// Add a slave pseudo-terminal to the PTS table.
fn add_slave(fs: Arc<SimpleFs>, pty: Arc<PtyDriver>) -> KResult<u32> {
    let mut table = PTS_TABLE.lock();
    let pty_number = table
        .add(DeviceFile::new(
            fs,
            NodeType::CharacterDevice,
            DeviceId::default(),
            pty.clone(),
        ))
        .map_err(|_| KError::TooManyOpenFiles)? as u32;
    pty.set_pty_number(pty_number);
    table
        .get(pty_number as usize)
        .unwrap()
        .set_device_id(DeviceId::new(136, pty_number));
    Ok(pty_number)
}

/// Master pseudo-terminal multiplexer (/dev/ptmx).
pub struct Ptmx(pub Arc<SimpleFs>);

impl Ptmx {
    /// Create a new PTY pair and return the master device file.
    pub fn create_pty(&self) -> KResult<(Arc<DeviceFile>, u32)> {
        let (master, slave) = create_pty_pair();
        add_slave(self.0.clone(), slave)?;
        let pty_number = master.pty_number();
        let device = DeviceFile::new(
            self.0.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(128, pty_number),
            master,
        );
        Ok((device, pty_number))
    }
}

impl DeviceFileOps for Ptmx {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    fn ioctl(&self, _cmd: u32, _arg: usize) -> KResult<usize> {
        Err(KError::NotATty)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// /dev/pts directory containing slave pseudo-terminal devices.
pub struct PtsDir;

impl SimpleDirOps for PtsDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        let ids = PTS_TABLE
            .lock()
            .ids()
            .map(|it| Cow::Owned(it.to_string()))
            .collect::<Vec<_>>();
        Box::new(ids.into_iter())
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let id = name.parse::<usize>().map_err(|_| KError::InvalidData)?;
        let pty = PTS_TABLE.lock().get(id).ok_or(KError::NotFound)?.clone();
        Ok(NodeOpsMux::File(pty))
    }
}
