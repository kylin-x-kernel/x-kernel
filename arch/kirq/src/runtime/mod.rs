// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ core runtime state and dispatch orchestration.

pub(crate) mod action;
pub(crate) mod dispatch;
pub(crate) mod manager;
pub(crate) mod nmi;
pub(crate) mod notify;
pub(crate) mod state;
pub(crate) mod sync_wait;
