// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn chain_validates_user_defined_name() {
    assert_eq!(
        Chain::user_defined(String::new(), Vec::new()),
        Err(ChainError::EmptyName)
    );
    // Pin the legacy chain-name boundary independently of the implementation constant.
    assert!(Chain::user_defined("X".repeat(28), Vec::new()).is_ok());
    assert_eq!(
        Chain::user_defined("X".repeat(29), Vec::new()),
        Err(ChainError::NameTooLong)
    );
    // Fourteen two-byte code points fit; one additional ASCII byte exceeds the limit.
    assert!(Chain::user_defined("é".repeat(14), Vec::new()).is_ok());
    assert_eq!(
        Chain::user_defined(alloc::format!("{}X", "é".repeat(14)), Vec::new()),
        Err(ChainError::NameTooLong)
    );
}
