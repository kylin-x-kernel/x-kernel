// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing socket message structures.

use linux_raw_sys::net::{cmsghdr, mmsghdr, msghdr, ucred};

use crate::ptr::UserRead;

// SAFETY: these socket message headers are POD syscall carriers whose bytes
// can be copied from user memory directly.
unsafe impl UserRead for cmsghdr {}
// SAFETY: these socket message headers are POD syscall carriers whose bytes
// can be copied from user memory directly.
unsafe impl UserRead for mmsghdr {}
// SAFETY: these socket message headers are POD syscall carriers whose bytes
// can be copied from user memory directly.
unsafe impl UserRead for msghdr {}
// SAFETY: these socket message headers are POD syscall carriers whose bytes
// can be copied from user memory directly.
unsafe impl UserRead for ucred {}
