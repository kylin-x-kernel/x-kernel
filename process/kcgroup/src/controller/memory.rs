// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory controller: binds one cgroup to one accounting node.
//!
//! Page ownership is tracked by `memacct`, not here. This controller exists so
//! the generic delegation machinery can create the accounting node at the
//! right place in the memory hierarchy and so control files can find it.

use alloc::sync::Arc;

use memacct::Memcg;

use super::{Controller, ControllerKind};

/// Per-cgroup memory controller.
pub struct MemoryController {
    memcg: Arc<Memcg>,
}

impl MemoryController {
    /// Wraps the accounting node owned by this cgroup.
    pub fn new(memcg: Arc<Memcg>) -> Self {
        Self { memcg }
    }

    /// Returns the accounting node owned by this cgroup.
    pub fn memcg(&self) -> Arc<Memcg> {
        Arc::clone(&self.memcg)
    }
}

impl Controller for MemoryController {
    fn kind(&self) -> ControllerKind {
        ControllerKind::Memory
    }

    // New page charges select the current task's cgroup through memacct's
    // provider. Existing pages keep their recorded owner across task migration
    // and exit; the controller itself carries no per-CPU charge state.
}
