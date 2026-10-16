// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::ITimerType;

#[def_test]
fn test_itimer_type_from_repr() {
    assert_eq!(ITimerType::from_repr(0), Some(ITimerType::Real));
    assert_eq!(ITimerType::from_repr(1), Some(ITimerType::Virtual));
    assert_eq!(ITimerType::from_repr(2), Some(ITimerType::Prof));
    assert_eq!(ITimerType::from_repr(3), None);
}
