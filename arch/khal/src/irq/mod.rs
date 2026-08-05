// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt trap adapter.
//!
//! Generic IRQ state, descriptors, dispatch, softirq, and interrupt-controller
//! contracts live in `kirq`. This module owns only architecture trap-entry
//! registration.

mod manager;

pub use manager::*;
