// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing time structure conversions.

use kerrno::{KError, KResult};
use ktime_types::{MICROS_PER_SEC, NANOS_PER_MICROS, NANOS_PER_SEC, SystemTime, TimeSpan};
#[cfg(target_arch = "x86_64")]
use linux_raw_sys::general::__kernel_old_time_t;
use linux_raw_sys::general::{
    __kernel_old_timespec, __kernel_old_timeval, __kernel_sock_timeval, __kernel_timespec,
    itimerspec, itimerval, timespec, timeval,
};

use crate::ptr::{UserRead, UserWrite};

/// Converts between kernel [`TimeSpan`] values and Linux duration structures.
pub trait TimeSpanLike {
    /// Converts from a kernel time span.
    fn from_time_span(span: TimeSpan) -> Self;

    /// Tries to convert into a non-negative kernel time span.
    fn try_into_time_span(self) -> KResult<TimeSpan>;
}

impl TimeSpanLike for TimeSpan {
    fn from_time_span(span: TimeSpan) -> Self {
        span
    }

    fn try_into_time_span(self) -> KResult<TimeSpan> {
        Ok(self)
    }
}

impl TimeSpanLike for timespec {
    fn from_time_span(span: TimeSpan) -> Self {
        Self {
            tv_sec: span.as_secs() as _,
            tv_nsec: span.subsec_nanos() as _,
        }
    }

    fn try_into_time_span(self) -> KResult<TimeSpan> {
        if self.tv_nsec < 0 || self.tv_nsec >= NANOS_PER_SEC as _ || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeSpan::new(self.tv_sec as u64, self.tv_nsec as u32))
    }
}

impl TimeSpanLike for __kernel_timespec {
    fn from_time_span(span: TimeSpan) -> Self {
        Self {
            tv_sec: span.as_secs() as _,
            tv_nsec: span.subsec_nanos() as _,
        }
    }

    fn try_into_time_span(self) -> KResult<TimeSpan> {
        if self.tv_nsec < 0 || self.tv_nsec >= NANOS_PER_SEC as _ || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeSpan::new(self.tv_sec as u64, self.tv_nsec as u32))
    }
}

impl TimeSpanLike for __kernel_old_timespec {
    fn from_time_span(span: TimeSpan) -> Self {
        Self {
            tv_sec: span.as_secs() as _,
            tv_nsec: span.subsec_nanos() as _,
        }
    }

    fn try_into_time_span(self) -> KResult<TimeSpan> {
        if self.tv_nsec < 0 || self.tv_nsec >= NANOS_PER_SEC as _ || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeSpan::new(self.tv_sec as u64, self.tv_nsec as u32))
    }
}

impl TimeSpanLike for timeval {
    fn from_time_span(span: TimeSpan) -> Self {
        Self {
            tv_sec: span.as_secs() as _,
            tv_usec: span.subsec_micros() as _,
        }
    }

    fn try_into_time_span(self) -> KResult<TimeSpan> {
        if self.tv_usec < 0 || self.tv_usec >= MICROS_PER_SEC as _ || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeSpan::new(
            self.tv_sec as u64,
            self.tv_usec as u32 * NANOS_PER_MICROS as u32,
        ))
    }
}

impl TimeSpanLike for __kernel_old_timeval {
    fn from_time_span(span: TimeSpan) -> Self {
        Self {
            tv_sec: span.as_secs() as _,
            tv_usec: span.subsec_micros() as _,
        }
    }

    fn try_into_time_span(self) -> KResult<TimeSpan> {
        if self.tv_usec < 0 || self.tv_usec >= MICROS_PER_SEC as _ || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeSpan::new(
            self.tv_sec as u64,
            self.tv_usec as u32 * NANOS_PER_MICROS as u32,
        ))
    }
}

impl TimeSpanLike for __kernel_sock_timeval {
    fn from_time_span(span: TimeSpan) -> Self {
        Self {
            tv_sec: span.as_secs() as _,
            tv_usec: span.subsec_micros() as _,
        }
    }

    fn try_into_time_span(self) -> KResult<TimeSpan> {
        if self.tv_usec < 0 || self.tv_usec >= MICROS_PER_SEC as _ || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeSpan::new(
            self.tv_sec as u64,
            self.tv_usec as u32 * NANOS_PER_MICROS as u32,
        ))
    }
}

/// Converts between kernel [`SystemTime`] values and Linux timestamp structures.
pub trait SystemTimeLike {
    /// Converts from a kernel system timestamp.
    fn from_system_time(time: SystemTime) -> Self;

    /// Tries to convert into a kernel system timestamp.
    fn try_into_system_time(self) -> KResult<SystemTime>;
}

/// Parses an absolute `CLOCK_REALTIME` deadline, rejecting negative values.
///
/// Linux validates absolute wall-clock deadlines with `timespec64_valid()`,
/// which rejects negative `tv_sec` with `EINVAL` — the rule used by
/// `clock_nanosleep` with `TIMER_ABSTIME` and by `futex` with
/// `FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME`. Negative seconds are
/// intentionally *not* rejected by [`SystemTimeLike::try_into_system_time`],
/// because pre-epoch timestamps are valid for file `atime`/`mtime`; this
/// helper applies the stricter syscall-boundary rule to deadline parsing only.
pub fn try_into_realtime_deadline<T: SystemTimeLike>(ts: T) -> KResult<SystemTime> {
    let deadline = ts.try_into_system_time()?;
    if deadline.unix_seconds() < 0 {
        return Err(KError::InvalidInput);
    }
    Ok(deadline)
}

macro_rules! impl_system_timespec {
    ($ty:ty) => {
        impl SystemTimeLike for $ty {
            fn from_system_time(time: SystemTime) -> Self {
                Self {
                    tv_sec: time.unix_seconds() as _,
                    tv_nsec: time.subsec_nanos() as _,
                }
            }

            fn try_into_system_time(self) -> KResult<SystemTime> {
                if self.tv_nsec < 0 || self.tv_nsec >= NANOS_PER_SEC as _ {
                    return Err(KError::InvalidInput);
                }
                SystemTime::from_unix_parts(self.tv_sec as i64, self.tv_nsec as u32)
                    .ok_or(KError::InvalidInput)
            }
        }
    };
}

macro_rules! impl_system_timeval {
    ($ty:ty) => {
        impl SystemTimeLike for $ty {
            fn from_system_time(time: SystemTime) -> Self {
                Self {
                    tv_sec: time.unix_seconds() as _,
                    tv_usec: (time.subsec_nanos() / NANOS_PER_MICROS as u32) as _,
                }
            }

            fn try_into_system_time(self) -> KResult<SystemTime> {
                if self.tv_usec < 0 || self.tv_usec >= MICROS_PER_SEC as _ {
                    return Err(KError::InvalidInput);
                }
                SystemTime::from_unix_parts(
                    self.tv_sec as i64,
                    self.tv_usec as u32 * NANOS_PER_MICROS as u32,
                )
                .ok_or(KError::InvalidInput)
            }
        }
    };
}

impl_system_timespec!(timespec);
impl_system_timespec!(__kernel_timespec);
impl_system_timespec!(__kernel_old_timespec);
impl_system_timeval!(timeval);
impl_system_timeval!(__kernel_old_timeval);
impl_system_timeval!(__kernel_sock_timeval);

// SAFETY: these time structs are POD syscall carriers whose fields are validated
// by the conversion helpers when semantic checks are needed.
unsafe impl UserRead for itimerspec {}
// SAFETY: these time structs are POD syscall carriers whose fields are validated
// by the conversion helpers when semantic checks are needed.
unsafe impl UserRead for itimerval {}
// SAFETY: these time structs are POD syscall carriers whose fields are validated
// by the conversion helpers when semantic checks are needed.
unsafe impl UserRead for timespec {}
// SAFETY: these time structs are POD syscall carriers whose fields are validated
// by the conversion helpers when semantic checks are needed.
unsafe impl UserRead for timeval {}
#[cfg(target_arch = "x86_64")]
// SAFETY: `utimbuf` is a POD syscall carrier with explicit integer fields.
unsafe impl UserRead for utimbuf {}

// SAFETY: these time structs are POD syscall carriers whose fields are validated
// by the conversion helpers when semantic checks are needed.
unsafe impl UserWrite for itimerspec {}
// SAFETY: these time structs are POD syscall carriers whose fields are validated
// by the conversion helpers when semantic checks are needed.
unsafe impl UserWrite for itimerval {}
// SAFETY: these time structs are POD syscall carriers whose fields are validated
// by the conversion helpers when semantic checks are needed.
unsafe impl UserWrite for timespec {}
// SAFETY: these time structs are POD syscall carriers whose fields are validated
// by the conversion helpers when semantic checks are needed.
unsafe impl UserWrite for timeval {}

#[cfg(target_arch = "x86_64")]
#[allow(non_camel_case_types)]
#[repr(C)]
pub struct utimbuf {
    pub actime: __kernel_old_time_t,
    pub modtime: __kernel_old_time_t,
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_timespec_from_time_span() {
        let tv = TimeSpan::new(42, 123_456_789);
        let ts = timespec::from_time_span(tv);
        assert_eq!(ts.tv_sec, 42);
        assert_eq!(ts.tv_nsec, 123_456_789);
    }

    #[def_test]
    fn test_timespec_try_into_time_span() {
        let ts = timespec {
            tv_sec: 10,
            tv_nsec: 500_000_000,
        };
        let tv = ts.try_into_time_span().unwrap();
        assert_eq!(tv.as_secs(), 10);
        assert_eq!(tv.subsec_nanos(), 500_000_000);
    }

    #[def_test]
    fn test_timespec_invalid_nsec_negative() {
        let ts = timespec {
            tv_sec: 0,
            tv_nsec: -1,
        };
        assert!(ts.try_into_time_span().is_err());
    }

    #[def_test]
    fn test_timespec_invalid_nsec_overflow() {
        let ts = timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000_000,
        };
        assert!(ts.try_into_time_span().is_err());
    }

    #[def_test]
    fn test_timespec_invalid_sec_negative() {
        let ts = timespec {
            tv_sec: -1,
            tv_nsec: 0,
        };
        assert!(ts.try_into_time_span().is_err());
    }

    #[def_test]
    fn test_timeval_from_time_span() {
        let tv = TimeSpan::new(5, 123_456_000);
        let tval = timeval::from_time_span(tv);
        assert_eq!(tval.tv_sec, 5);
        assert_eq!(tval.tv_usec, 123_456);
    }

    #[def_test]
    fn test_timeval_try_into_time_span() {
        let tval = timeval {
            tv_sec: 7,
            tv_usec: 999_999,
        };
        let tv = tval.try_into_time_span().unwrap();
        assert_eq!(tv.as_secs(), 7);
        assert_eq!(tv.subsec_nanos(), 999_999_000);
    }

    #[def_test]
    fn test_timeval_invalid_usec_overflow() {
        let tval = timeval {
            tv_sec: 0,
            tv_usec: 1_000_000,
        };
        assert!(tval.try_into_time_span().is_err());
    }

    #[def_test]
    fn test_timeval_invalid_usec_negative() {
        let tval = timeval {
            tv_sec: 0,
            tv_usec: -1,
        };
        assert!(tval.try_into_time_span().is_err());
    }

    #[def_test]
    fn test_timespec_zero() {
        let ts = timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let tv = ts.try_into_time_span().unwrap();
        assert_eq!(tv.as_secs(), 0);
        assert_eq!(tv.subsec_nanos(), 0);
    }

    #[def_test]
    fn test_timespec_boundary_nsec() {
        let ts = timespec {
            tv_sec: 0,
            tv_nsec: 999_999_999,
        };
        let tv = ts.try_into_time_span().unwrap();
        assert_eq!(tv.subsec_nanos(), 999_999_999);
    }

    #[def_test]
    fn test_time_span_identity() {
        let tv = TimeSpan::new(100, 200);
        let tv2 = TimeSpan::from_time_span(tv);
        assert_eq!(tv.as_secs(), tv2.as_secs());
        assert_eq!(tv.subsec_nanos(), tv2.subsec_nanos());
    }
}
