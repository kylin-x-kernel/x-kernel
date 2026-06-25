// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[macro_use]
mod macros;

#[cfg(any(target_arch = "aarch64", feature = "aarch64"))]
pub mod aarch64;
#[cfg(any(target_arch = "arm", feature = "arm"))]
pub mod arm;
#[cfg(any(target_arch = "loongarch64", feature = "loongarch64"))]
pub mod loongarch64;
#[cfg(any(target_arch = "riscv64", feature = "riscv64"))]
pub mod riscv64;
#[cfg(any(target_arch = "x86_64", feature = "x86_64"))]
pub mod x86_64;

#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
#[cfg(target_arch = "arm")]
pub use arm::*;
#[cfg(target_arch = "loongarch64")]
pub use loongarch64::*;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;
