// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared DICE handover helpers for kernel subsystems.

#![no_std]

extern crate alloc;

use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult};
use memaddr::{VirtAddr, pa};
use of::dice_region;
use xdice::{dice_main_flow_chain_codehash, dice_parse_handover};

const MAX_DICE_DATA_SIZE: usize = 0x1000;

pub const DICE_IOCTL_GET_HANDOVER: u32 = 0x9000_7A00;
pub const DICE_IOCTL_GET_RAW_HANDOVER: u32 = 0x9000_7A01;

#[derive(Debug, Clone, Copy)]
pub struct DiceDevice {
    region: (VirtAddr, usize),
}

impl DiceDevice {
    pub fn new() -> KResult<Self> {
        let region = dice_region()
            .map(|reg| (khal::mem::p2v(pa!(reg.starting_address as usize)), reg.size))
            .ok_or(KError::NotFound)?;
        Ok(Self { region })
    }

    pub fn read_raw_handover_data(&self) -> KResult<Vec<u8>> {
        let (addr, size) = self.region;
        if size > MAX_DICE_DATA_SIZE || size == 0 {
            return Err(KError::InvalidInput);
        }

        let mut buffer = vec![0u8; size];
        // SAFETY: `region` comes from the firmware-described DICE reserved
        // range, `size` was range-checked above, and `buffer` owns `size`
        // writable bytes for the copy destination.
        unsafe {
            core::ptr::copy_nonoverlapping(addr.as_usize() as *const u8, buffer.as_mut_ptr(), size);
        }
        Ok(buffer)
    }

    pub fn derive_handover_data(&self, code_hash: &[u8]) -> KResult<Vec<u8>> {
        let handover_data = self.read_raw_handover_data()?;
        let mut handover_buf = vec![0u8; handover_data.len()];
        let handover = dice_main_flow_chain_codehash(&handover_data, code_hash, &mut handover_buf)
            .map_err(|_| KError::InvalidInput)?;
        let (cdi_attest, cdi_seal, chain) =
            dice_parse_handover(handover).map_err(|_| KError::InvalidInput)?;

        let total_len = cdi_attest.len() + cdi_seal.len() + chain.len();
        let mut derived = Vec::new();
        derived
            .try_reserve_exact(total_len)
            .map_err(|_| KError::NoMemory)?;
        derived.extend_from_slice(cdi_attest);
        derived.extend_from_slice(cdi_seal);
        derived.extend_from_slice(chain);
        Ok(derived)
    }
}

pub fn read_raw_handover_data() -> KResult<Vec<u8>> {
    DiceDevice::new()?.read_raw_handover_data()
}

pub fn derive_handover_data(code_hash: &[u8]) -> KResult<Vec<u8>> {
    DiceDevice::new()?.derive_handover_data(code_hash)
}
