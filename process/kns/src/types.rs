// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace types, flags, and metadata.

use core::sync::atomic::{AtomicU64, Ordering};

bitflags::bitflags! {
    /// Flags for namespace creation (CLONE_NEW* family).
    #[derive(Debug, Clone, Copy, Default)]
    pub struct NamespaceFlags: u64 {
        const NEWNS     = linux_raw_sys::general::CLONE_NEWNS as u64;
        const NEWCGROUP = linux_raw_sys::general::CLONE_NEWCGROUP as u64;
        const NEWUTS    = linux_raw_sys::general::CLONE_NEWUTS as u64;
        const NEWIPC    = linux_raw_sys::general::CLONE_NEWIPC as u64;
        const NEWUSER   = linux_raw_sys::general::CLONE_NEWUSER as u64;
        const NEWPID    = linux_raw_sys::general::CLONE_NEWPID as u64;
        const NEWNET    = linux_raw_sys::general::CLONE_NEWNET as u64;
        const NEWTIME   = linux_raw_sys::general::CLONE_NEWTIME as u64;
    }
}

/// The type of a namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum NamespaceType {
    Mnt,
    Uts,
    Ipc,
    User,
    Pid,
    Net,
    Cgroup,
    Time,
}

impl NamespaceType {
    /// Returns the display name used in `/proc/[pid]/ns/<name>` and readlink output.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Mnt => "mnt",
            Self::Uts => "uts",
            Self::Ipc => "ipc",
            Self::User => "user",
            Self::Pid => "pid",
            Self::Net => "net",
            Self::Cgroup => "cgroup",
            Self::Time => "time",
        }
    }
}

/// Globally unique namespace identifier.
///
/// Used for `/proc/[pid]/ns/*` readlink output like `mnt:[4026531840]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NamespaceId(u64);

impl Default for NamespaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl NamespaceId {
    /// Allocate a new unique namespace ID.
    pub fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw u64 value.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(unittest)]
mod tests_types {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_namespace_id_monotonic() {
        let id1 = NamespaceId::new();
        let id2 = NamespaceId::new();
        let id3 = NamespaceId::new();
        assert!(id2.as_u64() > id1.as_u64());
        assert!(id3.as_u64() > id2.as_u64());
    }

    #[def_test]
    fn test_namespace_type_names() {
        assert_eq!(NamespaceType::Mnt.name(), "mnt");
        assert_eq!(NamespaceType::Uts.name(), "uts");
        assert_eq!(NamespaceType::Ipc.name(), "ipc");
        assert_eq!(NamespaceType::User.name(), "user");
        assert_eq!(NamespaceType::Pid.name(), "pid");
        assert_eq!(NamespaceType::Net.name(), "net");
        assert_eq!(NamespaceType::Cgroup.name(), "cgroup");
        assert_eq!(NamespaceType::Time.name(), "time");
    }

    #[def_test]
    fn test_namespace_id_display() {
        let id = NamespaceId::new();
        let displayed = alloc::format!("{}", id);
        assert!(!displayed.is_empty());
    }
}
