// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internet transport socket implementations.

pub mod raw;
pub mod tcp;
pub mod udp;
pub(crate) mod udp_err;
