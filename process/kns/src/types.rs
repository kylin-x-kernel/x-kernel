// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace types, flags, and metadata.

bitflags::bitflags! {
    /// Flags for namespace creation (CLONE_NEW* family).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct NamespaceFlags: u64 {
        /// Request a copy of the mount namespace.
        const NEWNS     = linux_raw_sys::general::CLONE_NEWNS as u64;
        /// Request a new cgroup namespace; currently rejected by `NsProxy::clone_for_child`.
        const NEWCGROUP = linux_raw_sys::general::CLONE_NEWCGROUP as u64;
        /// Request a private copy of hostname and domainname.
        const NEWUTS    = linux_raw_sys::general::CLONE_NEWUTS as u64;
        /// Request a distinct IPC namespace identity.
        const NEWIPC    = linux_raw_sys::general::CLONE_NEWIPC as u64;
        /// Request a new user namespace; currently unsupported.
        const NEWUSER   = linux_raw_sys::general::CLONE_NEWUSER as u64;
        /// Request a new PID namespace for children; currently unsupported.
        const NEWPID    = linux_raw_sys::general::CLONE_NEWPID as u64;
        /// Request a new network namespace; currently unsupported.
        const NEWNET    = linux_raw_sys::general::CLONE_NEWNET as u64;
        /// Request a new time namespace; currently unsupported.
        const NEWTIME   = linux_raw_sys::general::CLONE_NEWTIME as u64;
    }
}

/// The type of a namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum NamespaceType {
    /// Mount-tree namespace.
    Mnt,
    /// Hostname and domainname namespace.
    Uts,
    /// System V IPC namespace identity.
    Ipc,
    /// Credential owner namespace.
    User,
    /// Process-number namespace.
    Pid,
    /// Network namespace identity.
    Net,
    /// Cgroup hierarchy view.
    Cgroup,
    /// Clock namespace identity.
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

unittest::path_mod_test!(tests_types, "../ut-tests/types.rs");
