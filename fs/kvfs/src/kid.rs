// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel-side UID/GID identities and user-namespace projection.
//!
//! Filesystems store global IDs (Linux `kuid_t`/`kgid_t`): the value visible
//! in the initial user namespace, independent of any observer. Syscall
//! boundaries translate between a caller's namespace view and these global
//! values. Unmapped IDs never flow through as raw numbers: the projection
//! helpers return [`Kuid::INVALID`]/[`Kgid::INVALID`] when storing, and
//! [`OVERFLOW_ID`] when rendering, matching Linux's overflow-UID contract.

use alloc::sync::Arc;

use kcred::UserNamespace;

/// Value shown for IDs with no mapping in the observer namespace.
pub const OVERFLOW_ID: u32 = 65_534;

/// A global user ID stored by filesystems (Linux `kuid_t`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Kuid(u32);

/// A global group ID stored by filesystems (Linux `kgid_t`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Kgid(u32);

impl Kuid {
    /// The sentinel for "no valid mapping" (Linux `INVALID_KUID`).
    pub const INVALID: Self = Self(u32::MAX);

    /// Wraps a global ID without translation.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the global value.
    pub const fn as_raw(self) -> u32 {
        self.0
    }

    /// Reports whether this ID carries no mapping.
    pub const fn is_invalid(self) -> bool {
        self.0 == u32::MAX
    }

    /// Translates a caller-namespace ID into the global storage value.
    ///
    /// Returns [`Kuid::INVALID`] when the ID has no mapping in `ns`; callers
    /// must reject the operation rather than storing an unmapped value.
    pub fn from_inside(inside: u32, ns: &Arc<UserNamespace>) -> Self {
        ns.map_uid_to_initial(inside)
            .map(Self)
            .unwrap_or(Self::INVALID)
    }

    /// Renders this ID in the observer namespace.
    ///
    /// Unmapped IDs render as [`OVERFLOW_ID`], never as the raw global value.
    pub fn to_inside(self, ns: &Arc<UserNamespace>) -> u32 {
        if self.is_invalid() {
            return OVERFLOW_ID;
        }
        ns.map_uid_from_initial(self.0).unwrap_or(OVERFLOW_ID)
    }
}

impl Kgid {
    /// The sentinel for "no valid mapping" (Linux `INVALID_KGID`).
    pub const INVALID: Self = Self(u32::MAX);

    /// Wraps a global ID without translation.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the global value.
    pub const fn as_raw(self) -> u32 {
        self.0
    }

    /// Reports whether this ID carries no mapping.
    pub const fn is_invalid(self) -> bool {
        self.0 == u32::MAX
    }

    /// Translates a caller-namespace ID into the global storage value.
    pub fn from_inside(inside: u32, ns: &Arc<UserNamespace>) -> Self {
        ns.map_gid_to_initial(inside)
            .map(Self)
            .unwrap_or(Self::INVALID)
    }

    /// Renders this ID in the observer namespace.
    pub fn to_inside(self, ns: &Arc<UserNamespace>) -> u32 {
        if self.is_invalid() {
            return OVERFLOW_ID;
        }
        ns.map_gid_from_initial(self.0).unwrap_or(OVERFLOW_ID)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/kid.rs");
