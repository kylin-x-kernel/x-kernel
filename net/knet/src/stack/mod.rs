// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network stack orchestration around smoltcp.

pub(crate) mod listen_table;
pub(crate) mod router;
pub(crate) mod service;
pub(crate) mod wrapper;
