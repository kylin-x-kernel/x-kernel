// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal Linux kernel nodes
pub mod chosen;
pub mod dice;
pub mod interrupt;

pub use chosen::Chosen;
pub use dice::Dice;
pub use interrupt::InterruptController;
