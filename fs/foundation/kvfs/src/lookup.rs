// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Path lookup policy and magic-link interfaces.
//!
//! This module contains VFS-foundation types only. Filesystems such as procfs
//! can implement [`MagicLinkOps`] for Linux-style magic links without making
//! this crate depend on process or file-descriptor state.

use alloc::{string::String, sync::Arc};

use crate::{Location, VfsResult};

/// Semantic reason for resolving a path.
///
/// Lookup intent lets special filesystems make Linux-compatible decisions for
/// entries whose follow behavior is not a normal dentry lookup. For example,
/// `/proc/<pid>/fd/N` may allow `readlink` display while rejecting `exec`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LookupIntent {
    /// Resolve a path in order to open it.
    Open,
    /// Resolve a path in order to query metadata.
    Stat,
    /// Resolve a path in order to read a link target.
    Readlink,
    /// Resolve a path in order to execute a file.
    Exec,
}

bitflags::bitflags! {
    /// Typed VFS lookup policy.
    ///
    /// These flags are intentionally independent from raw Linux ABI bits.
    /// Syscall crates should translate `AT_*`, `O_*`, or future `RESOLVE_*`
    /// values before calling into the VFS layer.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct LookupFlags: u32 {
        /// Follow the final path component if it is a link.
        const FOLLOW_FINAL = 1 << 0;
        /// Permit an empty path to resolve through the supplied fd/source.
        const EMPTY_PATH = 1 << 1;
        /// Reject Linux-style magic links during path walk.
        const NO_MAGIC_LINKS = 1 << 2;
        /// Reject a resolution that crosses a mount boundary.
        const NO_XDEV = 1 << 3;
    }
}

impl LookupFlags {
    /// Returns flags for ordinary Linux path lookup, following the final link.
    pub const fn follow() -> Self {
        Self::FOLLOW_FINAL
    }

    /// Returns flags for no-follow lookup of the final component.
    pub const fn no_follow() -> Self {
        Self::empty()
    }

    /// Returns whether the final component may be followed.
    pub const fn follows_final(self) -> bool {
        self.contains(Self::FOLLOW_FINAL)
    }

    /// Returns whether magic-link following is forbidden.
    pub const fn rejects_magic_links(self) -> bool {
        self.contains(Self::NO_MAGIC_LINKS)
    }
}

/// Result of a VFS lookup before a syscall-specific consumer opens or uses it.
#[derive(Clone)]
pub enum ResolvedObject {
    /// A normal VFS location.
    Location(Location),
    /// A Linux-style magic link. The consumer may read its display target or
    /// follow it depending on [`LookupIntent`] and [`LookupFlags`].
    MagicLink(Arc<dyn MagicLinkOps>),
}

impl ResolvedObject {
    /// Creates a normal resolved VFS object.
    pub fn location(location: Location) -> Self {
        Self::Location(location)
    }

    /// Creates a resolved magic-link object.
    pub fn magic_link(link: Arc<dyn MagicLinkOps>) -> Self {
        Self::MagicLink(link)
    }

    /// Returns the normal location, if this object is not a magic link.
    pub fn as_location(&self) -> Option<&Location> {
        match self {
            Self::Location(location) => Some(location),
            Self::MagicLink(_) => None,
        }
    }

    /// Converts the resolved object into a normal location.
    pub fn into_location(self) -> VfsResult<Location> {
        match self {
            Self::Location(location) => Ok(location),
            Self::MagicLink(_) => Err(crate::VfsError::FilesystemLoop),
        }
    }
}

/// Operations for Linux-style magic links.
///
/// A magic link has symlink-like display behavior but typed follow behavior.
/// Its display string is not the authority for path walking. Implementations
/// should snapshot any external state, such as a file descriptor table entry,
/// before returning from [`Self::follow`].
pub trait MagicLinkOps: Send + Sync + 'static {
    /// Returns the userspace-visible `readlink` display target.
    fn readlink_display(&self) -> VfsResult<String>;

    /// Resolves the magic link for the requested operation.
    ///
    /// Implementations must not return with locks held that could be re-entered
    /// by later VFS open, file I/O, or exec loading paths.
    fn follow(&self, intent: LookupIntent, flags: LookupFlags) -> VfsResult<ResolvedObject>;
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::LookupFlags;

    #[def_test]
    fn lookup_flags_encode_follow_policy() {
        assert!(LookupFlags::follow().follows_final());
        assert!(!LookupFlags::no_follow().follows_final());
    }

    #[def_test]
    fn lookup_flags_encode_magic_link_rejection() {
        let flags = LookupFlags::follow() | LookupFlags::NO_MAGIC_LINKS;
        assert!(flags.follows_final());
        assert!(flags.rejects_magic_links());
    }
}
