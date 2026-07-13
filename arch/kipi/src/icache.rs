// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Implementation of [`karch::IcacheFlushIf`]: sends an IPI to all other CPUs
//! requesting they flush their local instruction cache.

use karch::IcacheFlushIf;

#[kiface::provide]
impl IcacheFlushIf {
    fn flush_others() {
        // Queue callbacks on all other CPUs and send IPIs. The callback calls
        // flush_icache_remote() which is the IPI-dedicated variant — it does NOT
        // re-trigger cross-CPU shootdown, avoiding recursion.
        if let Err(e) = crate::run_on_each_cpu(karch::flush_icache_remote) {
            warn!("Failed to send icache flush IPI: {:?}", e);
        }
    }
}
