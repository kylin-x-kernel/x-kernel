// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing filesystem structure traits.

use linux_raw_sys::general::{flock64, stat, statfs, statx};

use crate::ptr::{UserRead, UserWrite};

unsafe impl UserRead for flock64 {}

unsafe impl UserWrite for flock64 {}
unsafe impl UserWrite for stat {}
unsafe impl UserWrite for statfs {}
unsafe impl UserWrite for statx {}
