// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ARM low-level architecture operations.

mod irq;

pub use irq::{restore_irq, save_irq_and_disable};
