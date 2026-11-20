// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux credential secure-bits flags.
//!
//! The settings and their lock bits match modern `uapi/linux/securebits.h`.
//! `SECURE_NO_SETUID_FIXUP` suppresses capability fixups on set-ID changes,
//! and `SECURE_KEEP_CAPS` suppresses the permitted/effective drop when any old
//! real, effective, or saved user ID was root and all new IDs leave root. The
//! exec-check bits are stored and locked here but do not gate this kernel's
//! exec path, matching Linux where they are policy inputs read by user space
//! through `AT_EXECVE_CHECK`. Exec clears only `SECURE_KEEP_CAPS`; locks and all
//! other settings survive.

bitflags::bitflags! {
    /// Process secure-bits controlling set-ID and capability transitions.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct SecureBits: u32 {
        /// UID zero has no compatibility privileges during exec.
        const NOROOT = 1 << 0;
        /// Makes [`Self::NOROOT`] immutable.
        const NOROOT_LOCKED = 1 << 1;
        /// Suppress capability fixups on set-user-ID and set-filesystem-ID changes.
        const NO_SETUID_FIXUP = 1 << 2;
        /// Makes [`Self::NO_SETUID_FIXUP`] immutable.
        const NO_SETUID_FIXUP_LOCKED = 1 << 3;
        /// Suppress the drop when any old set-user ID was root and all new ones leave it.
        const KEEP_CAPS = 1 << 4;
        /// Prevent clearing [`Self::KEEP_CAPS`].
        const KEEP_CAPS_LOCKED = 1 << 5;
        /// Deny ambient-capability raises.
        const NO_CAP_AMBIENT_RAISE = 1 << 6;
        /// Makes [`Self::NO_CAP_AMBIENT_RAISE`] immutable.
        const NO_CAP_AMBIENT_RAISE_LOCKED = 1 << 7;
        /// Restrict `AT_EXECVE_CHECK` to files with execute permission.
        const EXEC_RESTRICT_FILE = 1 << 8;
        /// Makes [`Self::EXEC_RESTRICT_FILE`] immutable.
        const EXEC_RESTRICT_FILE_LOCKED = 1 << 9;
        /// Deny `AT_EXECVE_CHECK` on files opened for writing.
        const EXEC_DENY_INTERACTIVE = 1 << 10;
        /// Makes [`Self::EXEC_DENY_INTERACTIVE`] immutable.
        const EXEC_DENY_INTERACTIVE_LOCKED = 1 << 11;

        /// All configurable secure-bits settings.
        const ALL_BITS = Self::NOROOT.bits()
            | Self::NO_SETUID_FIXUP.bits()
            | Self::KEEP_CAPS.bits()
            | Self::NO_CAP_AMBIENT_RAISE.bits()
            | Self::EXEC_RESTRICT_FILE.bits()
            | Self::EXEC_DENY_INTERACTIVE.bits();
        /// All lock bits; each lock protects the setting immediately below it.
        const ALL_LOCKS = Self::ALL_BITS.bits() << 1;
        /// Settings and locks that an unprivileged caller may change.
        const UNPRIVILEGED = Self::EXEC_RESTRICT_FILE.bits()
            | Self::EXEC_DENY_INTERACTIVE.bits()
            | Self::EXEC_RESTRICT_FILE_LOCKED.bits()
            | Self::EXEC_DENY_INTERACTIVE_LOCKED.bits();
    }
}
