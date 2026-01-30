use alloc::{borrow::Cow, boxed::Box, string::ToString, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use flatten_objects::FlattenObjects;
use fs_ng_vfs::{DeviceId, NodeType, VfsResult};
use kcore::vfs::{Device, NodeOpsMux, SimpleDirOps, SimpleFs};
use kerrno::{KError, KResult};
use kspin::SpinNoIrq;

use crate::vfs::dev::tty::pty::PtyDriver;

static PTS_TABLE: SpinNoIrq<FlattenObjects<Arc<Device>, 16>> =
    SpinNoIrq::new(FlattenObjects::new());

/// Add a slave pseudo-terminal to /dev/pts
pub fn add_slave(fs: Arc<SimpleFs>, pty: Arc<PtyDriver>) -> KResult<u32> {
    let terminal = pty.terminal.clone();
    let mut table = PTS_TABLE.lock();
    let pty_number = table
        .add(Device::new(
            fs,
            NodeType::CharacterDevice,
            DeviceId::default(),
            pty,
        ))
        .map_err(|_| KError::TooManyOpenFiles)? as u32;
    terminal.pty_number.store(pty_number, Ordering::Release);
    table
        .get(pty_number as usize)
        .unwrap()
        .set_device_id(DeviceId::new(136, pty_number));
    Ok(pty_number)
}

/// /dev/pts directory
/// /dev/pts directory containing slave pseudo-terminal devices
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
