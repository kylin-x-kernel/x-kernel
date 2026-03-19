// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::TestDescriptor;

pub type StackTestEntry = extern "C" fn(*const TestDescriptor) -> u8;

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::call_on_stack;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::call_on_stack;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod fallback;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub use fallback::call_on_stack;
