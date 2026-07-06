// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup namespace and controller scaffolding for X-Kernel.
//!
//! This crate is intentionally small today. It owns the cgroup namespace type
//! used by `kns::NsProxy`, leaving room for future controller state, hierarchy
//! membership, and `/proc/self/cgroup` path rendering without growing `kns` into
//! a catch-all process-resource crate.

#![no_std]

use core::sync::atomic::{AtomicU64, Ordering};

/// Kernel-local identifier for a cgroup namespace.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CgroupNamespaceId(u64);

impl Default for CgroupNamespaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupNamespaceId {
    /// Allocates a new cgroup namespace identifier.
    pub fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw identifier value.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for CgroupNamespaceId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Cgroup namespace.
///
/// This is a phase-one placeholder. It gives each namespace a stable identity
/// while cgroup hierarchy, controller, and path-view semantics remain global.
pub struct CgroupNamespace {
    id: CgroupNamespaceId,
}

impl Default for CgroupNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupNamespace {
    /// Creates a new cgroup namespace placeholder.
    pub fn new() -> Self {
        Self {
            id: CgroupNamespaceId::new(),
        }
    }

    /// Returns the cgroup namespace identifier.
    pub fn id(&self) -> CgroupNamespaceId {
        self.id
    }
}
