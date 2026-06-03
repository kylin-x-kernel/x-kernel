// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing process structure traits.

use linux_raw_sys::general::{rlimit64, rusage};

use crate::ptr::{UserRead, UserWrite};

unsafe impl UserRead for rlimit64 {}

unsafe impl UserWrite for rlimit64 {}
unsafe impl UserWrite for rusage {}
