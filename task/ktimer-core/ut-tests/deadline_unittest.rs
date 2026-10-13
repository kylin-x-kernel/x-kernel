// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

    use ktime_types::TimeSpan;
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn deadline_accepts_zero_and_horizon_but_rejects_out_of_range() {
        for ns in [0, 1, u64::MAX - 1, u64::MAX] {
            let instant = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns));
            assert_eq!(Deadline::new(instant).unwrap().deadline_ns(), ns);
        }
        let beyond = TimeSpan::try_from_nanos(u128::from(u64::MAX) + 1).unwrap();
        assert_eq!(
            Deadline::new(MonotonicInstant::from_span_since_origin(beyond)),
            Err(KError::InvalidInput)
        );
    }