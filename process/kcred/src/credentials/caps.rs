// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux capability sets.
//!
//! A [`CapSet`] stores all 64 bits of the Linux capability ABI. Only defined
//! capabilities participate in [`CapSet::contains`] and in the named constants
//! below. The syscall ABI applies Linux's valid-capability mask to `capset(2)`
//! input, so user space cannot install bits above [`CAP_LAST_CAP`].
//!
//! [`CapSets`] groups the effective, permitted and inheritable sets and owns
//! the Linux transition rules used by `capset(2)`. Credential authority checks
//! use the effective set; the permitted set is the upper bound that a
//! capability-aware exec can install into the effective set.

use kerrno::{KError, KResult};

/// Highest capability number defined by this kernel.
pub const CAP_LAST_CAP: u32 = 40;

macro_rules! capability_constants {
    ($($(#[$meta:meta])* $name:ident = $value:expr),* $(,)?) => {
        $(
            $(#[$meta])*
            pub const $name: u32 = $value;
        )*
    };
}

capability_constants! {
    /// Bypass file ownership checks for `chown(2)` and related operations.
    CAP_CHOWN = 0,
    /// Bypass discretionary file read, write and execute checks.
    CAP_DAC_OVERRIDE = 1,
    /// Bypass file read and directory search checks.
    CAP_DAC_READ_SEARCH = 2,
    /// Bypass file ownership checks for inode metadata and extended attributes.
    CAP_FOWNER = 3,
    /// Preserve set-user-ID and set-group-ID bits on file creation and chmod.
    CAP_FSETID = 4,
    /// Send signals to processes owned by other users.
    CAP_KILL = 5,
    /// Change group IDs and supplementary groups.
    CAP_SETGID = 6,
    /// Change user IDs and filesystem user IDs.
    CAP_SETUID = 7,
    /// Relax the `capset(2)` inheritable-set restriction and drop
    /// capabilities from the bounding set.
    CAP_SETPCAP = 8,
    /// Set immutable and append-only inode flags.
    CAP_LINUX_IMMUTABLE = 9,
    /// Bind to privileged network ports.
    CAP_NET_BIND_SERVICE = 10,
    /// Broadcast packets and enable multicast promiscuity.
    CAP_NET_BROADCAST = 11,
    /// Configure networking interfaces, routing and firewall state.
    CAP_NET_ADMIN = 12,
    /// Open raw network sockets.
    CAP_NET_RAW = 13,
    /// Lock memory pages.
    CAP_IPC_LOCK = 14,
    /// Bypass IPC ownership checks.
    CAP_IPC_OWNER = 15,
    /// Load and unload kernel modules.
    CAP_SYS_MODULE = 16,
    /// Perform privileged raw I/O operations.
    CAP_SYS_RAWIO = 17,
    /// Change the process root directory with `chroot(2)`.
    CAP_SYS_CHROOT = 18,
    /// Trace arbitrary processes.
    CAP_SYS_PTRACE = 19,
    /// Enable or disable process accounting.
    CAP_SYS_PACCT = 20,
    /// Perform broad system administration operations.
    CAP_SYS_ADMIN = 21,
    /// Reboot, halt or power off the system.
    CAP_SYS_BOOT = 22,
    /// Change scheduling priority and policy.
    CAP_SYS_NICE = 23,
    /// Override resource limits and reserve system resources.
    CAP_SYS_RESOURCE = 24,
    /// Set the system clock.
    CAP_SYS_TIME = 25,
    /// Configure virtual terminals.
    CAP_SYS_TTY_CONFIG = 26,
    /// Create device nodes with `mknod(2)`.
    CAP_MKNOD = 27,
    /// Establish file leases.
    CAP_LEASE = 28,
    /// Write audit records.
    CAP_AUDIT_WRITE = 29,
    /// Configure kernel auditing.
    CAP_AUDIT_CONTROL = 30,
    /// Set file capabilities.
    CAP_SETFCAP = 31,
    /// Override mandatory access controls.
    CAP_MAC_OVERRIDE = 32,
    /// Configure mandatory access controls.
    CAP_MAC_ADMIN = 33,
    /// Read the kernel log buffer and configure logging.
    CAP_SYSLOG = 34,
    /// Set alarms that wake the system from suspend.
    CAP_WAKE_ALARM = 35,
    /// Block system suspend.
    CAP_BLOCK_SUSPEND = 36,
    /// Read audit records.
    CAP_AUDIT_READ = 37,
    /// Use performance monitoring facilities.
    CAP_PERFMON = 38,
    /// Load BPF programs and manage BPF maps.
    CAP_BPF = 39,
    /// Checkpoint and restore processes.
    CAP_CHECKPOINT_RESTORE = 40,
}

/// Capabilities affected by a filesystem-ID transition, matching Linux
/// `CAP_FS_MASK`.
pub const CAP_FS_MASK: CapSet = CapSet::from_bits(
    (1u64 << CAP_CHOWN)
        | (1u64 << CAP_DAC_OVERRIDE)
        | (1u64 << CAP_DAC_READ_SEARCH)
        | (1u64 << CAP_FOWNER)
        | (1u64 << CAP_FSETID)
        | (1u64 << CAP_LINUX_IMMUTABLE)
        | (1u64 << CAP_MAC_OVERRIDE)
        | (1u64 << CAP_MKNOD),
);

/// A Linux capability bitmap.
///
/// The representation is a full 64-bit word even though Linux currently
/// defines capabilities only through [`CAP_LAST_CAP`]. Constructors preserve
/// every input bit so the type can represent an arbitrary two-word ABI value and
/// support set algebra. The capability syscall masks user input against
/// [`CapSet::FULL`] before installing it in a credential.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapSet(u64);

impl CapSet {
    /// An empty capability set.
    pub const EMPTY: Self = Self(0);
    /// A set containing every currently defined capability.
    pub const FULL: Self = Self((1u64 << (CAP_LAST_CAP + 1)) - 1);

    /// Creates a set from a raw bitmap, preserving all 64 bits.
    ///
    /// This is a representation constructor, not a capability grant. The
    /// `capset(2)` adapter masks user input against [`Self::FULL`] first.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Creates a set from the low and high 32-bit words of the Linux ABI.
    pub const fn from_words(low: u32, high: u32) -> Self {
        Self((low as u64) | ((high as u64) << 32))
    }

    /// Returns the raw bitmap.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Returns the low 32 bits used by the first Linux ABI word.
    pub const fn low_word(self) -> u32 {
        self.0 as u32
    }

    /// Returns the high 32 bits used by the second Linux ABI word.
    pub const fn high_word(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// Returns whether the set contains `cap`.
    ///
    /// Capability numbers above [`CAP_LAST_CAP`] are not recognized even when
    /// the corresponding raw bit is set.
    pub const fn contains(self, cap: u32) -> bool {
        cap <= CAP_LAST_CAP && (self.0 & (1u64 << cap)) != 0
    }

    /// Returns whether every capability in `self` is also in `other`.
    pub const fn is_subset_of(self, other: Self) -> bool {
        (self.0 & !other.0) == 0
    }

    /// Returns the union of two sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns the intersection of two sets.
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Returns the capabilities in `self` that are absent from `other`.
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Returns a copy without `cap`; unknown capability numbers are ignored.
    pub const fn without(self, cap: u32) -> Self {
        if cap <= CAP_LAST_CAP {
            Self(self.0 & !(1u64 << cap))
        } else {
            self
        }
    }
}

impl core::ops::BitOr for CapSet {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl core::ops::BitAnd for CapSet {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        self.intersection(rhs)
    }
}

/// The effective, permitted and inheritable capability sets of a credential.
///
/// The effective set authorizes operations. The permitted set bounds what the
/// effective set may contain. The inheritable set survives exec together with
/// the file inheritable set. The bounding set is stored separately on
/// [`super::Cred`] because it limits file capabilities rather than the current
/// process capability state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapSets {
    effective: CapSet,
    permitted: CapSet,
    inheritable: CapSet,
}

impl CapSets {
    /// Empty effective, permitted and inheritable sets.
    pub const EMPTY: Self = Self {
        effective: CapSet::EMPTY,
        permitted: CapSet::EMPTY,
        inheritable: CapSet::EMPTY,
    };
    /// The sets installed for a newly created root task.
    pub const FULL_PRIVILEGED: Self = Self {
        effective: CapSet::FULL,
        permitted: CapSet::FULL,
        inheritable: CapSet::EMPTY,
    };

    /// Creates a set triple without validating the relationship between sets.
    ///
    /// [`Cred::set_capabilities`](super::Cred::set_capabilities) applies the
    /// Linux transition rules before committing the value.
    pub const fn new(effective: CapSet, permitted: CapSet, inheritable: CapSet) -> Self {
        Self {
            effective,
            permitted,
            inheritable,
        }
    }

    /// Returns the effective set.
    pub const fn effective(self) -> CapSet {
        self.effective
    }

    /// Returns the permitted set.
    pub const fn permitted(self) -> CapSet {
        self.permitted
    }

    /// Returns the inheritable set.
    pub const fn inheritable(self) -> CapSet {
        self.inheritable
    }

    /// Returns whether the effective set contains `cap`.
    pub const fn has_effective(self, cap: u32) -> bool {
        self.effective.contains(cap)
    }

    /// Validates a transition from these committed sets to `new`.
    ///
    /// `bounding` is the credential's current capability bounding set. A
    /// successful `capset(2)` never changes that set, but Linux uses it as an
    /// upper bound for the new inheritable set. The four rules are checked in
    /// Linux order and before any mutation:
    ///
    /// 1. Without `CAP_SETPCAP` in the old effective set, the new inheritable
    ///    set must be a subset of the old inheritable and permitted sets
    ///    combined.
    /// 2. The new inheritable set must always be a subset of the old
    ///    inheritable set and the bounding set combined.
    /// 3. The new permitted set must be a subset of the old permitted set.
    ///    `CAP_SETPCAP` does not allow the permitted set to grow.
    /// 4. The new effective set must be a subset of the new permitted set.
    ///
    /// # Errors
    ///
    /// Returns [`KError::OperationNotPermitted`] when a rule is violated.
    pub fn validate_transition(&self, new: &Self, bounding: CapSet) -> KResult<()> {
        if !self.effective.contains(CAP_SETPCAP)
            && !new
                .inheritable
                .is_subset_of(self.inheritable.union(self.permitted))
        {
            return Err(KError::OperationNotPermitted);
        }
        if !new
            .inheritable
            .is_subset_of(self.inheritable.union(bounding))
        {
            return Err(KError::OperationNotPermitted);
        }
        if !new.permitted.is_subset_of(self.permitted) {
            return Err(KError::OperationNotPermitted);
        }
        if !new.effective.is_subset_of(new.permitted) {
            return Err(KError::OperationNotPermitted);
        }
        Ok(())
    }
}
