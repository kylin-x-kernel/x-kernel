// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use khal::time::monotonic_time;
use ktime_types::{SystemTime, TimeSpan};
use osvm::MemError;
use posix_types::{UserConstPtr, UserPtr};
use tee_raw_sys::{
    TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_OVERFLOW, TEE_ERROR_TIME_NOT_SET, TEE_UUID, TeeTime,
};

use crate::tee::{
    TeeResult,
    tee_session::{with_tee_session_ctx, with_tee_session_ctx_mut},
};

fn map_user_mem_error(err: MemError) -> u32 {
    match err {
        MemError::InvalidAddr | MemError::NoAccess => TEE_ERROR_BAD_PARAMETERS,
        _ => tee_raw_sys::TEE_ERROR_GENERIC,
    }
}

pub fn tee_time_get_sys_time() -> SystemTime {
    ktime::realtime()
}
fn tee_time_get_ree_time() -> SystemTime {
    ktime::realtime()
}

/// Get the current time from the specified time category
pub fn sys_tee_scn_get_time(cat: u64, teetime: *mut TeeTime) -> TeeResult {
    // Get current session context
    let uuid = with_tee_session_ctx(|ctx| Ok(ctx.clnt_id.uuid))?;

    // Get time based on category
    let time_result: TeeResult<TeeTime> = match cat {
        0 => {
            // UTEE_TIME_CAT_SYSTEM
            let sys_time = tee_time_get_sys_time();
            Ok(TeeTime {
                seconds: sys_time.unix_seconds() as u32,
                millis: sys_time.subsec_nanos() / 1_000_000,
            })
        }
        1 => {
            // UTEE_TIME_CAT_TA_PERSISTENT
            tee_time_get_ta_time(&uuid)
        }
        2 => {
            // UTEE_TIME_CAT_REE
            let ree_time = tee_time_get_ree_time();
            Ok(TeeTime {
                seconds: ree_time.unix_seconds() as u32,
                millis: ree_time.subsec_nanos() / 1_000_000,
            })
        }
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };

    // Handle time retrieval result
    match time_result {
        Ok(time_value) => UserPtr::<TeeTime>::from(teetime)
            .write_vm(time_value)
            .map_err(map_user_mem_error),
        Err(e) if e == TEE_ERROR_OVERFLOW => {
            // Copy data even on overflow
            let time_value = tee_time_get_sys_time();
            let fallback_time = TeeTime {
                seconds: time_value.unix_seconds() as u32,
                millis: time_value.subsec_nanos() / 1_000_000,
            };
            UserPtr::<TeeTime>::from(teetime)
                .write_vm(fallback_time)
                .map_err(map_user_mem_error)?;
            Err(TEE_ERROR_OVERFLOW)
        }
        Err(e) => Err(e),
    }
}

/// Set the TA-specific time offset
pub fn sys_tee_scn_set_ta_time(mytime: *const TeeTime) -> TeeResult {
    let t = UserConstPtr::<TeeTime>::from(mytime)
        .read_vm()
        .map_err(map_user_mem_error)?;

    // Get current session context and set TA time
    with_tee_session_ctx_mut(|ctx| tee_time_set_ta_time(&ctx.clnt_id.uuid, &t))?;

    Ok(())
}

// TA time offset structure
struct TeeTaTimeOffs {
    uuid: TEE_UUID,
    offs: TeeTime,
    positive: bool,
}

// Global time offset storage
use ksync::{Mutex, static_lock};

static_lock! {
    static TEE_TIME_OFFS: Mutex<Option<vec::Vec<TeeTaTimeOffs>>> = Mutex::new(None);
}

// Helper function: compare UUIDs
fn uuid_equal(uuid1: &TEE_UUID, uuid2: &TEE_UUID) -> bool {
    uuid1.timeLow == uuid2.timeLow
        && uuid1.timeMid == uuid2.timeMid
        && uuid1.timeHiAndVersion == uuid2.timeHiAndVersion
        && uuid1.clockSeqAndNode == uuid2.clockSeqAndNode
}

// Get TA time offset
fn tee_time_ta_get_offs(uuid: &TEE_UUID) -> TeeResult<(TeeTime, bool)> {
    let offs_guard = TEE_TIME_OFFS.lock();

    if let Some(ref offsets) = *offs_guard {
        for entry in offsets {
            if uuid_equal(uuid, &entry.uuid) {
                return Ok((
                    TeeTime {
                        seconds: entry.offs.seconds,
                        millis: entry.offs.millis,
                    },
                    entry.positive,
                ));
            }
        }
    }

    Err(TEE_ERROR_TIME_NOT_SET)
}

// Set TA time offset
fn tee_time_ta_set_offs(uuid: &TEE_UUID, offs: &TeeTime, positive: bool) -> TeeResult {
    let mut offs_guard = TEE_TIME_OFFS.lock();

    if let Some(ref mut offsets) = *offs_guard {
        // Find existing entry and update
        for entry in offsets.iter_mut() {
            if uuid_equal(uuid, &entry.uuid) {
                entry.offs.seconds = offs.seconds;
                entry.offs.millis = offs.millis;
                entry.positive = positive;
                return Ok(());
            }
        }

        // Add new entry
        offsets.push(TeeTaTimeOffs {
            uuid: *uuid,
            offs: TeeTime {
                seconds: offs.seconds,
                millis: offs.millis,
            },
            positive,
        });
    } else {
        // Initialize vector and add first entry
        let new_offsets = vec![TeeTaTimeOffs {
            uuid: *uuid,
            offs: TeeTime {
                seconds: offs.seconds,
                millis: offs.millis,
            },
            positive,
        }];
        *offs_guard = Some(new_offsets);
    }

    Ok(())
}

// Get TA time
pub fn tee_time_get_ta_time(uuid: &TEE_UUID) -> TeeResult<TeeTime> {
    let (offs, positive) = tee_time_ta_get_offs(uuid)?;
    let t = tee_time_get_sys_time();

    let offset = TimeSpan::new(offs.seconds as u64, offs.millis * 1_000_000);
    let t2 = if positive {
        t.checked_add(offset)
    } else {
        t.checked_sub(offset)
    }
    .ok_or(TEE_ERROR_OVERFLOW)?;
    let seconds = u32::try_from(t2.unix_seconds()).map_err(|_| TEE_ERROR_OVERFLOW)?;

    Ok(TeeTime {
        seconds,
        millis: t2.subsec_nanos() / 1_000_000,
    })
}

// Set TA time
pub fn tee_time_set_ta_time(uuid: &TEE_UUID, time: &TeeTime) -> TeeResult {
    // Check if time is normalized
    if time.millis >= 1000 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let t = tee_time_get_sys_time();
    let time_value = SystemTime::from_unix_parts(time.seconds as i64, time.millis * 1_000_000)
        .ok_or(TEE_ERROR_BAD_PARAMETERS)?;
    let (duration, positive) = if time_value >= t {
        (
            time_value
                .duration_since(t)
                .map_err(|_| TEE_ERROR_OVERFLOW)?,
            true,
        )
    } else {
        (
            t.duration_since(time_value)
                .map_err(|_| TEE_ERROR_OVERFLOW)?,
            false,
        )
    };
    let offs = TeeTime {
        seconds: u32::try_from(duration.as_secs()).map_err(|_| TEE_ERROR_OVERFLOW)?,
        millis: duration.subsec_nanos() / 1_000_000,
    };
    tee_time_ta_set_offs(uuid, &offs, positive)
}

// Busy wait function
pub fn tee_time_busy_wait(milliseconds_delay: u32) -> TeeResult {
    let start_time = monotonic_time();
    let delay = TimeSpan::from_millis(milliseconds_delay as u64);
    let end_time = start_time.checked_add(delay).ok_or(TEE_ERROR_OVERFLOW)?;

    loop {
        if monotonic_time() >= end_time {
            break Ok(());
        }
        // Can add brief CPU yield here to avoid excessive CPU usage
        core::hint::spin_loop();
    }
}

/// Wait for a specified number of milliseconds
pub fn sys_tee_scn_wait(milliseconds_delay: u32) -> TeeResult {
    tee_time_busy_wait(milliseconds_delay)
}
