// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-visible scheduling policy types and the kernel class rank.

/// Kernel dispatch class for a scheduled task.
///
/// Rank is Linux order: Stop, Deadline, RT, Fair, Idle. Smaller
/// discriminant wins. Only Fair and Idle have class operations today;
/// the other ranks are reserved so later PRs can plug in without
/// renumbering.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchedClass {
    /// Highest rank. Kernel-only; not a user `SCHED_*` policy.
    Stop     = 0,
    /// `SCHED_DEADLINE`. Not admitted until a Deadline class exists.
    Deadline = 1,
    /// `SCHED_FIFO` / `SCHED_RR`. Not admitted until an RT class exists.
    Rt       = 2,
    /// `SCHED_NORMAL` / `SCHED_BATCH` / `SCHED_IDLE`.
    Fair     = 3,
    /// Per-CPU idle thread. Kernel-only; lowest rank.
    Idle     = 4,
}

impl SchedClass {
    /// `true` when `self` ranks above `other`.
    pub const fn is_above(self, other: SchedClass) -> bool {
        (self as u8) < (other as u8)
    }

    /// Stop and Idle are not user ABI policies.
    pub const fn is_kernel_only(self) -> bool {
        matches!(self, Self::Stop | Self::Idle)
    }

    /// Decode a stored class byte. Unknown values become Fair.
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Stop,
            1 => Self::Deadline,
            2 => Self::Rt,
            4 => Self::Idle,
            _ => Self::Fair,
        }
    }

    /// Encode as the rank byte stored on [`crate::SchedTask`].
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Linux `SCHED_*` values.
///
/// FIFO, RR, and Deadline decode and map to a class, but
/// [`SchedAttr::validate`] still rejects them until those classes are wired.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedPolicy {
    /// `SCHED_NORMAL`.
    Normal   = 0,
    /// `SCHED_FIFO`.
    Fifo     = 1,
    /// `SCHED_RR`.
    Rr       = 2,
    /// `SCHED_BATCH`. Same class as [`Self::Normal`].
    Batch    = 3,
    /// `SCHED_IDLE`. Same class as [`Self::Normal`].
    Idle     = 5,
    /// `SCHED_DEADLINE`.
    Deadline = 6,
}

impl SchedPolicy {
    /// Parse a raw Linux policy word.
    ///
    /// # Errors
    ///
    /// Returns [`SchedError::InvalidPolicy`] when `raw` is not a known
    /// `SCHED_*` value.
    pub const fn from_raw(raw: u32) -> Result<Self, SchedError> {
        match raw {
            0 => Ok(Self::Normal),
            1 => Ok(Self::Fifo),
            2 => Ok(Self::Rr),
            3 => Ok(Self::Batch),
            5 => Ok(Self::Idle),
            6 => Ok(Self::Deadline),
            _ => Err(SchedError::InvalidPolicy),
        }
    }

    /// Kernel dispatch class for this ABI policy.
    pub const fn class(self) -> SchedClass {
        match self {
            Self::Fifo | Self::Rr => SchedClass::Rt,
            Self::Deadline => SchedClass::Deadline,
            Self::Normal | Self::Batch | Self::Idle => SchedClass::Fair,
        }
    }
}

/// Linux nice in `-20..=19`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nice(i8);

impl Nice {
    /// `nice 0`.
    pub const DEFAULT: Nice = Nice(0);
    /// `nice 19`.
    pub const MAX: Nice = Nice(19);
    /// `nice -20`.
    pub const MIN: Nice = Nice(-20);

    /// Validate a user nice value.
    ///
    /// # Errors
    ///
    /// Returns [`SchedError::InvalidNice`] when `v` is outside Linux nice
    /// `-20..=19`. Use [`Self::new_clamped`] to saturate instead of failing.
    pub const fn new(v: i32) -> Result<Self, SchedError> {
        if v < -20 || v > 19 {
            Err(SchedError::InvalidNice)
        } else {
            Ok(Nice(v as i8))
        }
    }

    /// Clamp into `-20..=19`.
    pub const fn new_clamped(v: i32) -> Self {
        if v < -20 {
            Self::MIN
        } else if v > 19 {
            Self::MAX
        } else {
            Nice(v as i8)
        }
    }

    /// Signed nice used by `setpriority` after conversion.
    pub const fn as_i32(self) -> i32 {
        self.0 as i32
    }

    /// Linux `getpriority` user value: `20 - nice`.
    pub const fn to_getpriority_raw(self) -> i32 {
        20 - self.as_i32()
    }

    /// Inverse of [`Self::to_getpriority_raw`].
    ///
    /// Values outside `1..=40` clamp to [`Self::MIN`] or [`Self::MAX`].
    pub const fn from_getpriority_raw(raw: i32) -> Self {
        // Saturate first: `20 - i32::MIN` overflows before `new_clamped`.
        Self::new_clamped(20i32.saturating_sub(raw))
    }

    /// Nice-to-weight. Nice 0 is 1024.
    pub const fn weight(self) -> u32 {
        NICE_TO_WEIGHT[(self.0 as i32 + 20) as usize]
    }
}

/// Nice-to-weight table. Index 0 is nice -20.
const NICE_TO_WEIGHT: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904,
    3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87,
    70, 56, 45, 36, 29, 23, 18, 15,
];

/// Nice-0 load. At this weight 1 wall-clock ns = 1 vruntime unit.
pub(crate) const NICE_0_WEIGHT: i64 = 1024;

const _: () = assert!(NICE_TO_WEIGHT[20] as i64 == NICE_0_WEIGHT);

/// User-visible scheduling attributes. Unique fact on [`crate::SchedTask`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedAttr {
    /// Linux `sched_policy`.
    pub policy: SchedPolicy,
    /// Fair weight source. Kept so a later nice change can rescale lag.
    pub nice: Nice,
}

impl SchedAttr {
    /// `SCHED_NORMAL`, nice 0.
    pub const DEFAULT: SchedAttr = SchedAttr {
        policy: SchedPolicy::Normal,
        nice: Nice::DEFAULT,
    };

    /// Dispatch class implied by [`Self::policy`].
    pub const fn class(&self) -> SchedClass {
        self.policy.class()
    }

    /// Only Fair policies are admitted until other classes are wired.
    pub const fn validate(&self) -> Result<(), SchedError> {
        if matches!(self.class(), SchedClass::Fair) {
            Ok(())
        } else {
            Err(SchedError::InvalidPolicy)
        }
    }
}

/// Policy / placement errors returned to `ktask` and mapped to errno there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedError {
    /// Unknown or unsupported `SCHED_*` value.
    InvalidPolicy,
    /// Nice outside `-20..=19`.
    InvalidNice,
    /// Idle tasks are not user-modifiable.
    KernelClassImmutable,
}
