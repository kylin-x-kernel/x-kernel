// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing filesystem structure traits.

use linux_raw_sys::general::{flock64, stat, statfs, statx};

use crate::ptr::{UserRead, UserWrite};

// SAFETY: these Linux ABI structs are plain-old-data syscall carriers whose
// byte representations may be copied across the user/kernel boundary as declared.
unsafe impl UserRead for flock64 {}

// SAFETY: these Linux ABI structs are plain-old-data syscall carriers whose
// byte representations may be copied across the user/kernel boundary as declared.
unsafe impl UserWrite for flock64 {}
// SAFETY: these Linux ABI structs are plain-old-data syscall carriers whose
// byte representations may be copied across the user/kernel boundary as declared.
unsafe impl UserWrite for stat {}
// SAFETY: these Linux ABI structs are plain-old-data syscall carriers whose
// byte representations may be copied across the user/kernel boundary as declared.
unsafe impl UserWrite for statfs {}
// SAFETY: these Linux ABI structs are plain-old-data syscall carriers whose
// byte representations may be copied across the user/kernel boundary as declared.
unsafe impl UserWrite for statx {}
