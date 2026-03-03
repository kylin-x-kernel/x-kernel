// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! # JBD2 日志系统模块
//!
//! 提供 ext4 文件系统的日志功能，确保文件系统的一致性和可靠性。

#[allow(clippy::module_inception)]
pub mod jbd2;
/// JBD2 系统实现
pub mod jbdstruct;
