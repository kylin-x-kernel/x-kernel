// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn inet_raw_protocol_rejects_values_above_u8() {
    assert_eq!(inet_raw_protocol(0).unwrap(), IpProtocol::from(0));
    assert_eq!(inet_raw_protocol(255).unwrap(), IpProtocol::from(255));
    assert_eq!(
        inet_raw_protocol(256).unwrap_err(),
        KError::from(LinuxError::EINVAL)
    );
}
