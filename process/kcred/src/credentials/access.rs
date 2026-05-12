// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Credential snapshots for access checks.

use alloc::sync::Arc;

use super::{Credentials, Gid, Uid};

/// Credential ID set used for access checks.
#[derive(Clone)]
pub struct AccessCredentials {
    uid: Uid,
    gid: Gid,
    groups: Arc<[Gid]>,
}

impl AccessCredentials {
    /// Creates an access credential snapshot from explicit IDs and groups.
    pub fn new(uid: Uid, gid: Gid, groups: Arc<[Gid]>) -> Self {
        Self { uid, gid, groups }
    }

    /// Returns the user ID used for the access check.
    pub fn uid(&self) -> Uid {
        self.uid
    }

    /// Returns the group ID used for the access check.
    pub fn gid(&self) -> Gid {
        self.gid
    }

    /// Returns the sorted supplementary group list.
    pub fn groups(&self) -> &[Gid] {
        &self.groups
    }

    /// Returns whether `gid` matches this credential's group set.
    pub fn has_group(&self, gid: Gid) -> bool {
        self.gid == gid || self.groups.binary_search(&gid).is_ok()
    }
}

/// Selects which credential IDs are used for an access check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessIdKind {
    /// Real user and group IDs.
    Real,
    /// Effective user and group IDs.
    Effective,
    /// Filesystem user and group IDs.
    Filesystem,
}

impl Credentials {
    /// Returns a credential snapshot for access checks.
    pub fn access_snapshot(&self, kind: AccessIdKind) -> AccessCredentials {
        let (uid, gid) = match kind {
            AccessIdKind::Real => (self.ruid(), self.rgid()),
            AccessIdKind::Effective => (self.euid(), self.egid()),
            AccessIdKind::Filesystem => (self.fsuid(), self.fsgid()),
        };
        AccessCredentials::new(uid, gid, self.supplementary_groups_snapshot())
    }
}
