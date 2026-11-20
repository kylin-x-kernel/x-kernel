// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The Linux `security.capability` extended attribute.
//!
//! A file carries two capability sets: the permitted set that `execve(2)` may
//! install, and the inheritable set that participates in the ambient
//! intersection. One flag records whether the permitted set also becomes
//! effective. The wire format has three little-endian revisions: version 1
//! stores one 32-bit word per set, version 2 stores two, and version 3 appends
//! the user-namespace root ID that owns the set.
//!
//! This kernel has a single user namespace, so a version 3 attribute applies
//! at exec only when its root ID is zero: any other value names a namespace
//! that cannot exist here. Writes still accept a structurally valid version 3
//! blob with any root ID, as Linux `cap_convert_nscap()` does; the read path
//! decides whether it applies. Version 1 is readable for compatibility, while
//! only the two-word revisions are accepted on write. Bits above
//! [`CAP_LAST_CAP`](super::CAP_LAST_CAP) are clipped on parse, mirroring the
//! valid-capability mask the syscall ABI applies.

use alloc::vec::Vec;

use kerrno::{KError, KResult};

use super::CapSet;

/// Extended-attribute name that stores on-disk file capabilities.
pub const SECURITY_CAPABILITY_XATTR: &str = "security.capability";

/// Revision 1: one 32-bit word per set.
pub const VFS_CAP_REVISION_1: u32 = 0x0100_0000;
/// Revision 2: two 32-bit words per set.
pub const VFS_CAP_REVISION_2: u32 = 0x0200_0000;
/// Revision 3: revision 2 plus the owning user-namespace root ID.
pub const VFS_CAP_REVISION_3: u32 = 0x0300_0000;

/// The permitted set also becomes effective at `execve(2)`.
pub const VFS_CAP_FLAGS_EFFECTIVE: u32 = 0x0000_0001;

const VFS_CAP_REVISION_MASK: u32 = 0xff00_0000;
const VFS_CAP_FLAGS_MASK: u32 = !VFS_CAP_REVISION_MASK;

const XATTR_CAPS_SZ_1: usize = 4 * (1 + 2);
const XATTR_CAPS_SZ_2: usize = 4 * (1 + 2 * 2);
const XATTR_CAPS_SZ_3: usize = 4 * (2 + 2 * 2);

fn read_word(bytes: &[u8], index: usize) -> Option<u32> {
    let start = index.checked_mul(4)?;
    let end = start.checked_add(4)?;
    let word = bytes.get(start..end)?;
    Some(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
}

/// File capabilities decoded from the `security.capability` attribute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileCapabilities {
    permitted: CapSet,
    inheritable: CapSet,
    effective: bool,
    root_id: u32,
}

impl FileCapabilities {
    /// Builds a set tuple owned by the initial user namespace.
    pub const fn new(permitted: CapSet, inheritable: CapSet, effective: bool) -> Self {
        Self {
            permitted,
            inheritable,
            effective,
            root_id: 0,
        }
    }

    /// Decodes the on-disk attribute, as `execve(2)` reads it.
    ///
    /// A malformed attribute or one owned by a foreign user namespace is
    /// rejected; callers treat any error as "the file carries no capabilities",
    /// which is how Linux ignores attributes it cannot apply.
    pub fn parse(bytes: &[u8]) -> KResult<Self> {
        let caps = Self::decode(bytes)?;
        // Linux `rootid_owns_currentns()` only accepts a root ID that names
        // zero in this namespace or an ancestor; any other owner cannot apply
        // here.
        if caps.root_id != 0 {
            return Err(KError::OperationNotPermitted);
        }
        Ok(caps)
    }

    /// Decodes the wire format without judging the owning namespace.
    fn decode(bytes: &[u8]) -> KResult<Self> {
        let magic = read_word(bytes, 0).ok_or(KError::InvalidInput)?;
        let flags = magic & VFS_CAP_FLAGS_MASK;
        if flags & !VFS_CAP_FLAGS_EFFECTIVE != 0 {
            return Err(KError::InvalidInput);
        }
        let (permitted, inheritable, root_id) = match magic & VFS_CAP_REVISION_MASK {
            VFS_CAP_REVISION_1 if bytes.len() == XATTR_CAPS_SZ_1 => (
                CapSet::from_words(read_word(bytes, 1).ok_or(KError::InvalidInput)?, 0),
                CapSet::from_words(read_word(bytes, 2).ok_or(KError::InvalidInput)?, 0),
                0,
            ),
            VFS_CAP_REVISION_2 if bytes.len() == XATTR_CAPS_SZ_2 => (
                // struct vfs_cap_data: data[0].permitted, data[0].inheritable,
                // data[1].permitted, data[1].inheritable.
                CapSet::from_words(
                    read_word(bytes, 1).ok_or(KError::InvalidInput)?,
                    read_word(bytes, 3).ok_or(KError::InvalidInput)?,
                ),
                CapSet::from_words(
                    read_word(bytes, 2).ok_or(KError::InvalidInput)?,
                    read_word(bytes, 4).ok_or(KError::InvalidInput)?,
                ),
                0,
            ),
            VFS_CAP_REVISION_3 if bytes.len() == XATTR_CAPS_SZ_3 => (
                CapSet::from_words(
                    read_word(bytes, 1).ok_or(KError::InvalidInput)?,
                    read_word(bytes, 3).ok_or(KError::InvalidInput)?,
                ),
                CapSet::from_words(
                    read_word(bytes, 2).ok_or(KError::InvalidInput)?,
                    read_word(bytes, 4).ok_or(KError::InvalidInput)?,
                ),
                read_word(bytes, 5).ok_or(KError::InvalidInput)?,
            ),
            _ => return Err(KError::InvalidInput),
        };
        Ok(Self {
            permitted: permitted.intersection(CapSet::FULL),
            inheritable: inheritable.intersection(CapSet::FULL),
            effective: flags & VFS_CAP_FLAGS_EFFECTIVE != 0,
            root_id,
        })
    }

    /// Validates an attribute a caller wants to store.
    ///
    /// Linux `cap_convert_nscap()` only accepts the two-word revisions; the
    /// one-word format is a migration input, never a new write. Revision 3 may
    /// name any root ID: Linux stores it, and only exec-time
    /// `rootid_owns_currentns()` decides whether it applies.
    pub fn validate_for_write(bytes: &[u8]) -> KResult<()> {
        let magic = read_word(bytes, 0).ok_or(KError::InvalidInput)?;
        let revision = magic & VFS_CAP_REVISION_MASK;
        if revision != VFS_CAP_REVISION_2 && revision != VFS_CAP_REVISION_3 {
            return Err(KError::InvalidInput);
        }
        Self::decode(bytes).map(|_| ())
    }

    /// Encodes the attribute in the revision that matches its owner.
    pub fn to_xattr(self) -> Vec<u8> {
        let revision = if self.root_id == 0 {
            VFS_CAP_REVISION_2
        } else {
            VFS_CAP_REVISION_3
        };
        let magic = if self.effective {
            revision | VFS_CAP_FLAGS_EFFECTIVE
        } else {
            revision
        };
        let mut bytes = Vec::with_capacity(if self.root_id == 0 {
            XATTR_CAPS_SZ_2
        } else {
            XATTR_CAPS_SZ_3
        });
        bytes.extend_from_slice(&magic.to_le_bytes());
        // struct vfs_cap_data interleaves the sets by word:
        // data[0].permitted, data[0].inheritable, data[1].permitted,
        // data[1].inheritable.
        bytes.extend_from_slice(&self.permitted.low_word().to_le_bytes());
        bytes.extend_from_slice(&self.inheritable.low_word().to_le_bytes());
        bytes.extend_from_slice(&self.permitted.high_word().to_le_bytes());
        bytes.extend_from_slice(&self.inheritable.high_word().to_le_bytes());
        if self.root_id != 0 {
            bytes.extend_from_slice(&self.root_id.to_le_bytes());
        }
        bytes
    }

    /// Returns the permitted set the file grants.
    pub const fn permitted(self) -> CapSet {
        self.permitted
    }

    /// Returns the inheritable set the file grants.
    pub const fn inheritable(self) -> CapSet {
        self.inheritable
    }

    /// Returns whether the permitted set also becomes effective at exec.
    pub const fn effective(self) -> bool {
        self.effective
    }

    /// Returns the user-namespace root ID that owns the set.
    pub const fn root_id(self) -> u32 {
        self.root_id
    }
}
