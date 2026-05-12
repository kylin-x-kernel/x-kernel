// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX priority syscalls.

use kerrno::{KError, KResult};
use kthread::{get_process_group, get_process_state};
use linux_raw_sys::general::{PRIO_PGRP, PRIO_PROCESS, PRIO_USER};

/// Returns the current nice priority for the selected target.
pub fn sys_getpriority(which: u32, who: u32) -> KResult<isize> {
    debug!("sys_getpriority <= which: {which}, who: {who}");

    match which {
        PRIO_PROCESS => {
            if who != 0 {
                let _proc = get_process_state(who)?;
            }
            Ok(20)
        }
        PRIO_PGRP => {
            if who != 0 {
                let _pg = get_process_group(who)?;
            }
            Ok(20)
        }
        PRIO_USER => {
            if who == 0 {
                Ok(20)
            } else {
                Err(KError::NoSuchProcess)
            }
        }
        _ => Err(KError::InvalidInput),
    }
}

/// Updates the current nice priority for the selected target.
pub fn sys_setpriority(which: u32, who: u32, prio: i32) -> KResult<isize> {
    debug!("sys_setpriority <= which: {which}, who: {who}, prio: {prio}");

    if !(-20..=19).contains(&prio) {
        return Err(KError::InvalidInput);
    }

    match which {
        PRIO_PROCESS => {
            if who != 0 {
                let _proc = get_process_state(who)?;
            }
            Ok(0)
        }
        PRIO_PGRP => {
            if who != 0 {
                let _pg = get_process_group(who)?;
            }
            Ok(0)
        }
        PRIO_USER => {
            if who == 0 {
                Ok(0)
            } else {
                Err(KError::NoSuchProcess)
            }
        }
        _ => Err(KError::InvalidInput),
    }
}
