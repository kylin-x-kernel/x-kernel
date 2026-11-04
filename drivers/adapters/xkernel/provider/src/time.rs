// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`driver_os_api::TimeOp`] provider contract.

use driver_os_api::TimeOp;
use ktime_types::MonotonicInstant;

use crate::XKernelResourceProvider;

impl TimeOp for XKernelResourceProvider {
    fn monotonic_time(&self) -> MonotonicInstant {
        khal::time::monotonic_time()
    }
}
