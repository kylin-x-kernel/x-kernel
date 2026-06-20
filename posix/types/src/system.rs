// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing system information structure traits.

use linux_raw_sys::system::{new_utsname, sysinfo};

use crate::ptr::UserWrite;

// SAFETY: these system-info structs are POD syscall output carriers.
unsafe impl UserWrite for new_utsname {}
// SAFETY: these system-info structs are POD syscall output carriers.
unsafe impl UserWrite for sysinfo {}
