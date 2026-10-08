// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network-layer packet processing, fragment reassembly, and routing rules.

pub(crate) mod fragment;
pub(crate) mod ipv4;
pub(crate) mod router;
