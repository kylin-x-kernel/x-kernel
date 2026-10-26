// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-visible scheduling policy types and the kernel class rank.

/// Kernel dispatch class for a scheduled task.
///
/// Rank is Linux order: Stop, Deadline, RT, Fair, Idle. Smaller
/// discriminant wins.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchedClass {
    /// Highest rank. Kernel-only; not a user `SCHED_*` policy.
    Stop     = 0,
    /// `SCHED_DEADLINE`.
    Deadline = 1,
    /// `SCHED_FIFO` / `SCHED_RR`.
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
/// FIFO and RR decode onto the RT class. Deadline decodes onto its rank.
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

    /// `true` for `SCHED_FIFO` and `SCHED_RR`.
    pub const fn is_realtime(self) -> bool {
        matches!(self, Self::Fifo | Self::Rr)
    }
}

/// `SCHED_FIFO` / `SCHED_RR` priority. Larger wins. Non-RT is [`Self::NONE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RtPrio(u8);

impl RtPrio {
    /// Highest valid RT priority. Same bound as `config::MAX_RT_PRIORITY`.
    pub const MAX: RtPrio = RtPrio(crate::config::MAX_RT_PRIORITY);
    /// Lowest valid RT priority. Same bound as `config::MIN_RT_PRIORITY`.
    pub const MIN: RtPrio = RtPrio(crate::config::MIN_RT_PRIORITY);
    /// Not a realtime task.
    pub const NONE: RtPrio = RtPrio(0);

    /// Validate a user RT priority (`1..=99`).
    ///
    /// # Errors
    ///
    /// Returns [`SchedError::InvalidPriority`] when `v` is outside `1..=99`.
    /// [`Self::NONE`] (`0`) is the non-RT sentinel and is not accepted here.
    pub const fn new(v: i32) -> Result<Self, SchedError> {
        if v < Self::MIN.get() as i32 || v > Self::MAX.get() as i32 {
            Err(SchedError::InvalidPriority)
        } else {
            Ok(RtPrio(v as u8))
        }
    }

    /// Stored priority byte.
    pub const fn get(self) -> u8 {
        self.0
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

/// `SCHED_DEADLINE` bandwidth parameters, all in nanoseconds.
///
/// The triple describes one sporadic task: it may run for `runtime_ns` in
/// every `period_ns`, and each request must finish within `deadline_ns` of
/// its activation. A period of zero is normalized to the deadline by the
/// syscall boundary, so the stored period is always nonzero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DlParams {
    /// Maximum runtime available in one period.
    pub runtime_ns: u64,
    /// Relative deadline of each request, measured from activation.
    pub deadline_ns: u64,
    /// Minimum inter-arrival time between two requests.
    pub period_ns: u64,
}

impl DlParams {
    /// No reservation. The neutral value carried by a non-Deadline
    /// [`SchedAttr`]; [`SchedAttr::validate`] rejects it as a real triple.
    pub const EMPTY: DlParams = DlParams {
        runtime_ns: 0,
        deadline_ns: 0,
        period_ns: 0,
    };

    /// Validates a deadline parameter triple.
    ///
    /// # Errors
    ///
    /// Returns [`SchedError::InvalidPolicy`] when any field is zero, when
    /// `runtime_ns > deadline_ns`, when `deadline_ns > period_ns`, or when
    /// `runtime_ns` is below [`crate::config::MIN_DL_RUNTIME_NS`].
    pub const fn new(
        runtime_ns: u64,
        deadline_ns: u64,
        period_ns: u64,
    ) -> Result<Self, SchedError> {
        if runtime_ns == 0
            || deadline_ns == 0
            || period_ns == 0
            || runtime_ns > deadline_ns
            || deadline_ns > period_ns
            || runtime_ns < crate::config::MIN_DL_RUNTIME_NS
        {
            return Err(SchedError::InvalidPolicy);
        }
        Ok(Self {
            runtime_ns,
            deadline_ns,
            period_ns,
        })
    }

    /// Bandwidth this task reserves, in parts per million of one CPU.
    ///
    /// Saturating so an extreme `runtime_ns`/`period_ns` pair cannot panic;
    /// the value only feeds the per-RQ admission sum. Returns zero for the
    /// neutral [`Self::EMPTY`] triple.
    pub(crate) const fn load_ppm(self) -> u64 {
        if self.period_ns == 0 {
            return 0;
        }
        let scaled = self.runtime_ns as u128 * 1_000_000u128 / self.period_ns as u128;
        if scaled > u64::MAX as u128 {
            u64::MAX
        } else {
            scaled as u64
        }
    }
}

/// User-visible scheduling attributes. Unique fact on [`crate::SchedTask`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedAttr {
    /// Linux `sched_policy`.
    pub policy: SchedPolicy,
    /// Fair weight source. Kept so a later return to Fair restores it.
    pub nice: Nice,
    /// [`RtPrio::NONE`] unless [`SchedPolicy::is_realtime`].
    pub rt_priority: RtPrio,
    /// `Some` exactly when `policy` is [`SchedPolicy::Deadline`].
    pub dl_params: Option<DlParams>,
}

impl SchedAttr {
    /// `SCHED_NORMAL`, nice 0.
    pub const DEFAULT: SchedAttr = SchedAttr {
        policy: SchedPolicy::Normal,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: None,
    };

    /// Dispatch class implied by [`Self::policy`].
    pub const fn class(&self) -> SchedClass {
        self.policy.class()
    }

    /// Structural consistency between `policy` and the per-class fields.
    ///
    /// FIFO and RR require an RT priority in `1..=99`. Fair policies must
    /// carry [`RtPrio::NONE`]. Deadline requires a valid [`DlParams`]. Any
    /// policy other than Deadline must leave `dl_params` empty.
    ///
    /// # Errors
    ///
    /// Returns [`SchedError::InvalidPriority`] when FIFO/RR is outside
    /// `1..=99`, or when a Fair policy carries a non-[`RtPrio::NONE`] priority.
    ///
    /// Returns [`SchedError::InvalidPolicy`] when a policy other than Deadline
    /// carries `dl_params`, or when a Deadline policy does not carry a valid
    /// triple.
    pub const fn validate(&self) -> Result<(), SchedError> {
        match self.policy {
            SchedPolicy::Fifo | SchedPolicy::Rr => {
                if self.dl_params.is_some() {
                    Err(SchedError::InvalidPolicy)
                } else if self.rt_priority.get() < RtPrio::MIN.get()
                    || self.rt_priority.get() > RtPrio::MAX.get()
                {
                    Err(SchedError::InvalidPriority)
                } else {
                    Ok(())
                }
            }
            SchedPolicy::Deadline => match self.dl_params {
                Some(params) => {
                    match DlParams::new(params.runtime_ns, params.deadline_ns, params.period_ns) {
                        Ok(_) => Ok(()),
                        Err(error) => Err(error),
                    }
                }
                None => Err(SchedError::InvalidPolicy),
            },
            SchedPolicy::Normal | SchedPolicy::Batch | SchedPolicy::Idle => {
                if self.dl_params.is_some() {
                    Err(SchedError::InvalidPolicy)
                } else if self.rt_priority.get() != RtPrio::NONE.get() {
                    Err(SchedError::InvalidPriority)
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// Policy / placement errors returned to `ktask` and mapped to errno there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedError {
    /// Unknown or unsupported `SCHED_*` value.
    InvalidPolicy,
    /// RT priority outside `1..=99`, or a non-RT attr carries a priority.
    InvalidPriority,
    /// Nice outside `-20..=19`.
    InvalidNice,
    /// Per-RQ deadline bandwidth is exhausted; the caller maps this to
    /// `EBUSY`, distinct from the `EINVAL` of an invalid attribute.
    AdmissionFull,
    /// Idle tasks are not user-modifiable.
    KernelClassImmutable,
}
