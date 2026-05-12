// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX CPU affinity syscalls.

use kerrno::{KError, KResult};
use ktask::{KCpuMask, current};
use posix_types::{UserConstPtr, UserPtr};

/// Returns the current CPU affinity mask.
pub fn sys_sched_getaffinity(
    pid: i32,
    cpusetsize: usize,
    user_mask: UserPtr<u8>,
) -> KResult<isize> {
    if cpusetsize * 8 < kbuild_config::CPU_NUM {
        return Err(KError::InvalidInput);
    }

    // TODO: support other threads
    if pid != 0 {
        return Err(KError::OperationNotPermitted);
    }

    let mask = current().cpumask();
    let mask_bytes = mask.as_bytes();

    user_mask.write_vm_slice(mask_bytes)?;

    Ok(mask_bytes.len() as _)
}

/// Updates the current CPU affinity mask.
pub fn sys_sched_setaffinity(
    _pid: i32,
    cpusetsize: usize,
    user_mask: UserConstPtr<u8>,
) -> KResult<isize> {
    let size = cpusetsize.min(kbuild_config::CPU_NUM.div_ceil(8));
    let user_mask = user_mask.load_vm_vec(size)?;
    let mut cpu_mask = KCpuMask::new();

    for i in 0..(size * 8).min(kbuild_config::CPU_NUM) {
        if user_mask[i / 8] & (1 << (i % 8)) != 0 {
            cpu_mask.set(i, true);
        }
    }

    // TODO: support other threads
    ktask::set_current_affinity(cpu_mask);

    Ok(0)
}
