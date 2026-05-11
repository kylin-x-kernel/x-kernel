// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing time structure conversions.

use kerrno::{KError, KResult};
use khal::time::TimeValue;
#[cfg(target_arch = "x86_64")]
use linux_raw_sys::general::__kernel_old_time_t;
use linux_raw_sys::general::{
    __kernel_old_timespec, __kernel_old_timeval, __kernel_sock_timeval, __kernel_timespec,
    itimerspec, itimerval, timespec, timeval,
};

use crate::{UserRead, UserWrite};

/// Convert between kernel `TimeValue` and Linux time structures.
pub trait TimeValueLike {
    /// Convert from kernel [`TimeValue`].
    fn from_time_value(tv: TimeValue) -> Self;

    /// Try to convert into kernel [`TimeValue`].
    fn try_into_time_value(self) -> KResult<TimeValue>;
}

impl TimeValueLike for TimeValue {
    fn from_time_value(tv: TimeValue) -> Self {
        tv
    }

    fn try_into_time_value(self) -> KResult<TimeValue> {
        Ok(self)
    }
}

impl TimeValueLike for timespec {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_nsec: tv.subsec_nanos() as _,
        }
    }

    fn try_into_time_value(self) -> KResult<TimeValue> {
        if self.tv_nsec < 0 || self.tv_nsec > 999_999_999 || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeValue::new(self.tv_sec as u64, self.tv_nsec as u32))
    }
}

impl TimeValueLike for __kernel_timespec {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_nsec: tv.subsec_nanos() as _,
        }
    }

    fn try_into_time_value(self) -> KResult<TimeValue> {
        if self.tv_nsec < 0 || self.tv_nsec > 999_999_999 || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeValue::new(self.tv_sec as u64, self.tv_nsec as u32))
    }
}

impl TimeValueLike for __kernel_old_timespec {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_nsec: tv.subsec_nanos() as _,
        }
    }

    fn try_into_time_value(self) -> KResult<TimeValue> {
        if self.tv_nsec < 0 || self.tv_nsec > 999_999_999 || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeValue::new(self.tv_sec as u64, self.tv_nsec as u32))
    }
}

impl TimeValueLike for timeval {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_usec: tv.subsec_micros() as _,
        }
    }

    fn try_into_time_value(self) -> KResult<TimeValue> {
        if self.tv_usec < 0 || self.tv_usec > 999_999 || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeValue::new(
            self.tv_sec as u64,
            self.tv_usec as u32 * 1000,
        ))
    }
}

impl TimeValueLike for __kernel_old_timeval {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_usec: tv.subsec_micros() as _,
        }
    }

    fn try_into_time_value(self) -> KResult<TimeValue> {
        if self.tv_usec < 0 || self.tv_usec > 999_999 || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeValue::new(
            self.tv_sec as u64,
            self.tv_usec as u32 * 1000,
        ))
    }
}

impl TimeValueLike for __kernel_sock_timeval {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_usec: tv.subsec_micros() as _,
        }
    }

    fn try_into_time_value(self) -> KResult<TimeValue> {
        if self.tv_usec < 0 || self.tv_usec > 999_999 || self.tv_sec < 0 {
            return Err(KError::InvalidInput);
        }
        Ok(TimeValue::new(
            self.tv_sec as u64,
            self.tv_usec as u32 * 1000,
        ))
    }
}

unsafe impl UserRead for itimerspec {}
unsafe impl UserRead for itimerval {}
unsafe impl UserRead for timespec {}
unsafe impl UserRead for timeval {}
#[cfg(target_arch = "x86_64")]
unsafe impl UserRead for utimbuf {}

unsafe impl UserWrite for itimerspec {}
unsafe impl UserWrite for itimerval {}
unsafe impl UserWrite for timespec {}
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
    fn test_timespec_from_time_value() {
        let tv = TimeValue::new(42, 123_456_789);
        let ts = timespec::from_time_value(tv);
        assert_eq!(ts.tv_sec, 42);
        assert_eq!(ts.tv_nsec, 123_456_789);
    }

    #[def_test]
    fn test_timespec_try_into_time_value() {
        let ts = timespec {
            tv_sec: 10,
            tv_nsec: 500_000_000,
        };
        let tv = ts.try_into_time_value().unwrap();
        assert_eq!(tv.as_secs(), 10);
        assert_eq!(tv.subsec_nanos(), 500_000_000);
    }

    #[def_test]
    fn test_timespec_invalid_nsec_negative() {
        let ts = timespec {
            tv_sec: 0,
            tv_nsec: -1,
        };
        assert!(ts.try_into_time_value().is_err());
    }

    #[def_test]
    fn test_timespec_invalid_nsec_overflow() {
        let ts = timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000_000,
        };
        assert!(ts.try_into_time_value().is_err());
    }

    #[def_test]
    fn test_timespec_invalid_sec_negative() {
        let ts = timespec {
            tv_sec: -1,
            tv_nsec: 0,
        };
        assert!(ts.try_into_time_value().is_err());
    }

    #[def_test]
    fn test_timeval_from_time_value() {
        let tv = TimeValue::new(5, 123_456_000);
        let tval = timeval::from_time_value(tv);
        assert_eq!(tval.tv_sec, 5);
        assert_eq!(tval.tv_usec, 123_456);
    }

    #[def_test]
    fn test_timeval_try_into_time_value() {
        let tval = timeval {
            tv_sec: 7,
            tv_usec: 999_999,
        };
        let tv = tval.try_into_time_value().unwrap();
        assert_eq!(tv.as_secs(), 7);
        assert_eq!(tv.subsec_nanos(), 999_999_000);
    }

    #[def_test]
    fn test_timeval_invalid_usec_overflow() {
        let tval = timeval {
            tv_sec: 0,
            tv_usec: 1_000_000,
        };
        assert!(tval.try_into_time_value().is_err());
    }

    #[def_test]
    fn test_timeval_invalid_usec_negative() {
        let tval = timeval {
            tv_sec: 0,
            tv_usec: -1,
        };
        assert!(tval.try_into_time_value().is_err());
    }

    #[def_test]
    fn test_timespec_zero() {
        let ts = timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let tv = ts.try_into_time_value().unwrap();
        assert_eq!(tv.as_secs(), 0);
        assert_eq!(tv.subsec_nanos(), 0);
    }

    #[def_test]
    fn test_timespec_boundary_nsec() {
        let ts = timespec {
            tv_sec: 0,
            tv_nsec: 999_999_999,
        };
        let tv = ts.try_into_time_value().unwrap();
        assert_eq!(tv.subsec_nanos(), 999_999_999);
    }

    #[def_test]
    fn test_time_value_identity() {
        let tv = TimeValue::new(100, 200);
        let tv2 = TimeValue::from_time_value(tv);
        assert_eq!(tv.as_secs(), tv2.as_secs());
        assert_eq!(tv.subsec_nanos(), tv2.subsec_nanos());
    }
}
