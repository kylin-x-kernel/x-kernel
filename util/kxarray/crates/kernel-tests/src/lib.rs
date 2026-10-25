// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! XArray integration fixtures against initialized task, IRQ and RCU services.
//!
//! The unittest entry selects optional linking through `kfeat/kernel-tests`;
//! ordinary kernel images do not select this crate. Separating these fixtures
//! prevents `kirq -> kxarray -> kirq` dependency cycles, including via `ktask`.
//! This is dependency-cycle isolation, not a general test architecture mandate.
//! Basic UT, host tests and doctests remain in the container.
#![no_std]

#[cfg(all(unittest, target_os = "none"))]
extern crate alloc;

#[cfg(target_os = "none")]
unittest::path_mod_test!(tests_tasks, "../ut-tests/tasks.rs");
#[cfg(target_os = "none")]
unittest::path_mod_test!(tests_context, "../ut-tests/context.rs");
#[cfg(all(target_os = "none", feature = "smp"))]
unittest::path_mod_test!(tests_migration, "../ut-tests/migration.rs");
