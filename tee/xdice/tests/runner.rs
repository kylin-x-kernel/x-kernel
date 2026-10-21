// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub use xdice::*;
#[path = "support/test_data.rs"]
mod test_data;
#[path = "cases/android.rs"]
mod tests_android;
#[path = "cases/cbor_cert_op.rs"]
mod tests_cbor_cert_op;
#[path = "cases/dice.rs"]
mod tests_dice;
#[path = "cases/lib.rs"]
mod tests_lib;
#[path = "cases/mbedtls_sm2dsa.rs"]
mod tests_mbedtls_sm2dsa;
#[path = "cases/tee_dice.rs"]
mod tests_tee_dice;
