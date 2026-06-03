// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing task and capability structure traits.

use linux_raw_sys::general::{__user_cap_data_struct, __user_cap_header_struct};

use crate::ptr::{UserRead, UserWrite};

unsafe impl UserRead for __user_cap_header_struct {}

unsafe impl UserWrite for __user_cap_data_struct {}
unsafe impl UserWrite for __user_cap_header_struct {}
