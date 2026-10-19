// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::MsiMessage;

#[def_test]
fn test_msi_message_accessors() {
    let message = MsiMessage::new(0xfee0_0000, 0x80);
    assert_eq!(message.address(), 0xfee0_0000);
    assert_eq!(message.data(), 0x80);
}
