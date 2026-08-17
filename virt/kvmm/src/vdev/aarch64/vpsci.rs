// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal virtual PSCI handler for AArch64 guest CPU bring-up.

const PSCI_VERSION: u64 = 0x8400_0000;
const PSCI_CPU_ON_32: u64 = 0x8400_0003;
const PSCI_CPU_ON_64: u64 = 0xC400_0003;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;
const PSCI_FEATURES: u64 = 0x8400_000A;

pub const PSCI_RET_SUCCESS: u64 = 0;
pub const PSCI_RET_NOT_SUPPORTED: u64 = (-1_i64) as u64;
pub const PSCI_RET_INVALID_PARAMS: u64 = (-2_i64) as u64;
pub const PSCI_RET_ALREADY_ON: u64 = (-4_i64) as u64;

const PSCI_VERSION_0_2: u64 = 0x0000_0002;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsciAction {
    Continue,
    Shutdown,
    CpuOn {
        target_cpu: u64,
        entry_addr: u64,
        context_id: u64,
    },
}

pub fn handle_psci(gprs: &mut [u64; 31]) -> Option<PsciAction> {
    let fid = gprs[0];
    let prefix = (fid >> 24) as u8;
    if prefix != 0x84 && prefix != 0xC4 {
        return None;
    }

    let action = match fid {
        PSCI_VERSION => {
            gprs[0] = PSCI_VERSION_0_2;
            PsciAction::Continue
        }
        PSCI_CPU_ON_32 | PSCI_CPU_ON_64 => PsciAction::CpuOn {
            target_cpu: gprs[1],
            entry_addr: gprs[2],
            context_id: gprs[3],
        },
        PSCI_SYSTEM_OFF | PSCI_SYSTEM_RESET => {
            log::info!("[vpsci] shutdown fid={:#x}", fid);
            PsciAction::Shutdown
        }
        PSCI_FEATURES => {
            gprs[0] = match gprs[1] {
                PSCI_VERSION | PSCI_CPU_ON_32 | PSCI_CPU_ON_64 | PSCI_SYSTEM_OFF
                | PSCI_SYSTEM_RESET | PSCI_FEATURES => PSCI_RET_SUCCESS,
                _ => PSCI_RET_NOT_SUPPORTED,
            };
            PsciAction::Continue
        }
        _ => {
            log::debug!("[vpsci] unsupported function {:#x}", fid);
            gprs[0] = PSCI_RET_NOT_SUPPORTED;
            PsciAction::Continue
        }
    };

    Some(action)
}
