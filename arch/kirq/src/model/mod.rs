// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Values shared by IRQ consumers, control transactions and irqchip providers.
//!
//! Descriptors, identities, errors and handler results describe requests; they
//! do not own live mappings, controller claims or registered actions. Runtime
//! validates these values against retained Lines before changing live state.

pub(crate) mod affinity;
pub(crate) mod desc;
