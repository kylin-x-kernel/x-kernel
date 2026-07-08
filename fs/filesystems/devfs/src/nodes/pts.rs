// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pseudo-terminal slave directory and master multiplexer.

use alloc::{borrow::Cow, boxed::Box, string::ToString, sync::Arc, vec::Vec};

use flatten_objects::FlattenObjects;
use kerrno::{KError, KResult};
use kspin::SpinNoIrq;
use ktty::tty::{PtyDriver, create_pty_pair};
use kvfs::{
    Dentry, DeviceFileOps, DeviceId, NodeType, SimpleDirLookup, SimpleDirOps, SimpleFs, VfsFile,
    VfsFileBuilder, VfsInode, VfsResult,
};

use crate::{DeviceFile, device_dentry};

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
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let (master, _) = self.create_pty()?;
        file.replace_fops(master);
        Ok(())
    }

    fn ioctl(&self, _file: &VfsFile, _cmd: u32, _arg: usize) -> KResult<usize> {
        Err(KError::NotATty)
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

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let id = name.parse::<usize>().map_err(|_| KError::InvalidData)?;
        let pty = PTS_TABLE.lock().get(id).ok_or(KError::NotFound)?.clone();
        Ok(device_dentry(lookup, name, pty))
    }
}
