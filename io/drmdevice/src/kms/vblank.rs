// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `WAIT_VBLANK` (`drm_vblank.c`).

use crate::prelude::*;

/// `DRM_IOCTL_WAIT_VBLANK`.
impl Ioctl for DrmWaitVblank {
    const NAME: &'static str = "WAIT_VBLANK";

    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, request: &mut Self) -> DrmResult<usize> {
        let is_relative = request.rep_type & DRM_VBLANK_RELATIVE != 0;
        let current = dev.vblank_sequence();
        let target = if is_relative {
            current.wrapping_add(request.sequence)
        } else {
            request.sequence
        };
        let raw_wait = target.wrapping_sub(current);
        let wait_count = if raw_wait == 0 || raw_wait >= i32::MAX as u32 {
            1
        } else {
            raw_wait
        };
        const FRAME_PERIOD_NS: u64 = ktime_types::NANOS_PER_SEC / 60;
        let delay =
            ktime_types::TimeSpan::from_nanos(FRAME_PERIOD_NS.saturating_mul(wait_count as u64));
        ktask::sleep(delay);
        dev.advance_vblank(wait_count);
        let now = khal::time::monotonic_time().span_since_origin();
        *request = DrmWaitVblank {
            rep_type: 0,
            sequence: dev.vblank_sequence(),
            tv_sec: now.as_secs() as i64,
            tv_usec: now.subsec_micros() as i64,
        };
        Ok(0)
    }
}
