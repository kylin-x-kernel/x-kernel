// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing socket message structures.

use linux_raw_sys::net::{cmsghdr, mmsghdr, msghdr, ucred};

use crate::UserRead;

unsafe impl UserRead for cmsghdr {}
unsafe impl UserRead for mmsghdr {}
unsafe impl UserRead for msghdr {}
unsafe impl UserRead for ucred {}
