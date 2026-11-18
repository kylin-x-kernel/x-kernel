// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt affinity procfs nodes.
//!
//! Names and text presentation live here; KIRQ retains Line identity, owner
//! policy and the admitted routing transaction.

pub(crate) mod dir;
mod error;
pub(crate) mod file;
pub(crate) mod namespace;
pub(crate) mod root;
