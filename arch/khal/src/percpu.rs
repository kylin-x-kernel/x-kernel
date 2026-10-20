// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-local platform initialization and CPU identity.

#[cfg(feature = "smp")]
pub use kplat::cpu::ap_cpu_init as init_secondary;
pub use kplat::cpu::{boot_cpu_init as init_primary, id as this_cpu_id, *};

#[cfg(unittest)]
unittest::path_mod_test!(pub mod tests_percpu, "../ut-tests/percpu.rs");
