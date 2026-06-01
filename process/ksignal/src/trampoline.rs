// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Signal trampoline mappings for user address spaces.

use kerrno::KResult;
use khal::{mem::v2p, paging::MappingFlags};
use memaddr::PAGE_SIZE_4K;
use memspace::AddrSpace;

/// Map the signal trampoline to the user address space.
pub fn map_signal_trampoline(aspace: &mut AddrSpace) -> KResult {
    let signal_trampoline_paddr = v2p(crate::arch::signal_trampoline_address().into());
    aspace.map_linear(
        kaddr_layout::SIGNAL_TRAMPOLINE.into(),
        signal_trampoline_paddr,
        PAGE_SIZE_4K,
        MappingFlags::READ | MappingFlags::EXECUTE | MappingFlags::USER,
    )?;
    Ok(())
}
