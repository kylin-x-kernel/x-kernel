// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! getcpu(2) syscall implementation.

use kerrno::{KError, KResult};
use khal::percpu::this_cpu_id;
use posix_types::UserPtr;

/// Returns the CPU and NUMA node on which the calling thread is running.
///
/// `cpu` and `node` may be null. `tcache` must be null (not supported).
pub fn sys_getcpu(cpu: UserPtr<u32>, node: UserPtr<u32>, tcache: usize) -> KResult<isize> {
    if tcache != 0 {
        return Err(KError::InvalidInput);
    }

    if let Some(cpu) = cpu.check_non_null() {
        cpu.write_vm(this_cpu_id().as_usize() as u32)?;
    }

    if let Some(node) = node.check_non_null() {
        // No NUMA support yet; report node 0.
        node.write_vm(0)?;
    }

    Ok(0)
}
