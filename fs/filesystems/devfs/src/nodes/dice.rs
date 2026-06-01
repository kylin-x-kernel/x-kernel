// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DICE module for handling DICE handover data.
use alloc::{sync::Arc, vec, vec::Vec};
use core::any::Any;

use dice_driver::{DICE_IOCTL_GET_HANDOVER, DICE_IOCTL_GET_RAW_HANDOVER};
use kerrno::{KError, KResult};
use klazy::Lazy;
use ksync::Mutex;
use kvfs::DeviceFileOps;
use kvfs_simple::{DirMapping, SimpleFs};
use osvm::{VirtMutPtr, VirtPtr, write_vm_mem};
use rand_chacha::{
    ChaCha8Rng,
    rand_core::{RngCore, SeedableRng},
};

use crate::DeviceFile;

#[derive(Debug, Clone, Copy, Default)]
pub struct DiceNodeInfo;

type DiceUserBuffer = [usize; 3];

impl DiceNodeInfo {
    pub fn new() -> Self {
        Self
    }

    fn copy_to_user(&self, arg: usize, data: &[u8]) -> KResult<usize> {
        let [handover_ptr, handover_size, handover_out_size] =
            (arg as *const DiceUserBuffer).read_vm()?;

        (handover_out_size as *mut usize).write_vm(data.len())?;
        if handover_size < data.len() {
            return Err(KError::InvalidInput);
        }

        write_vm_mem(handover_ptr as *mut u8, data)?;
        Ok(data.len())
    }

    fn sys_dice_get_handover(&self, arg: usize) -> KResult<usize> {
        let hash = get_process_hash()?;
        let handover = dice_driver::derive_handover_data(&hash)?;
        let len = self.copy_to_user(arg, &handover)?;
        warn!("dice : get derived handover success.");
        Ok(len)
    }

    fn sys_dice_get_raw_handover(&self, arg: usize) -> KResult<usize> {
        let handover = dice_driver::read_raw_handover_data()?;
        let len = self.copy_to_user(arg, &handover)?;
        warn!("dice : get raw handover success.");
        Ok(len)
    }
}

impl DeviceFileOps for DiceNodeInfo {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> KResult<usize> {
        let data = dice_driver::read_raw_handover_data()?;
        let offset = usize::try_from(offset).map_err(|_| KError::InvalidInput)?;
        if offset >= data.len() {
            return Ok(0);
        }

        let len = buf.len().min(data.len() - offset);
        buf[..len].copy_from_slice(&data[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> KResult<usize> {
        match cmd {
            DICE_IOCTL_GET_HANDOVER => self.sys_dice_get_handover(arg),
            DICE_IOCTL_GET_RAW_HANDOVER => self.sys_dice_get_raw_handover(arg),
            _ => Err(KError::InvalidInput),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn get_process_hash() -> KResult<Vec<u8>> {
    use alloc::format;

    use kthread::current_process_state;
    use mbedtls::hash::{Md, Type};

    let pid = kthread::current_thread().pid();
    let proc_exe_path = format!("/proc/{}/exe", pid);
    let proc_state = current_process_state();
    let fs = proc_state.fs_context().lock();
    let data = fs.read(&proc_exe_path).map_err(|_| KError::NotFound)?;

    let mut sm3_result = vec![0u8; 32];
    let mut ctx = Md::new(Type::SM3).map_err(|_| KError::InvalidInput)?;
    ctx.update(&data).map_err(|_| KError::InvalidInput)?;
    let _len = ctx.finish(&mut sm3_result);

    info!("resm3_resultsult: {:x?}", sm3_result);
    Ok(sm3_result)
}

static GLOBAL_RAND: Lazy<Mutex<ChaCha8Rng>> = Lazy::new(|| {
    let seed = khal::time::now_ticks();
    Mutex::new(ChaCha8Rng::seed_from_u64(seed))
});

#[unsafe(no_mangle)]
pub extern "C" fn get_rand(output: usize, len: usize) -> u32 {
    let mut buf = vec![0u8; len];
    let mut rand = GLOBAL_RAND.lock();
    rand.fill_bytes(&mut buf);
    write_vm_mem(output as *mut u8, &buf).map_or(1, |_| 0)
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "dice",
        DeviceFile::new(
            fs.clone(),
            kvfs::NodeType::CharacterDevice,
            kvfs::DeviceId::new(30, 0),
            Arc::new(DiceNodeInfo::new()),
        ),
    );
}
