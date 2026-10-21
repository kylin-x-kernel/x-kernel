// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS and path-based syscall adapters.

pub(crate) use posix_fs::*;

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../../ut-tests/vfs/tests.rs");
