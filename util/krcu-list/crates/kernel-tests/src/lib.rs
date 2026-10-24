// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel-backed RCU list validation, linked by kfeat only for unittest.
//! Keeping scheduler and interrupt fixtures here avoids dependencies from the
//! reusable container back into its intended consumers such as kirq.
//! Otherwise adopting the list would create `kirq -> krcu-list -> kirq`
//! (also through `ktask`). This is a dependency-cycle exception, not a default
//! test layout: basic UT, host tests and doctests stay in the container crate.

#![no_std]

#[cfg(all(unittest, target_os = "none"))]
extern crate alloc;

#[cfg(target_os = "none")]
unittest::path_mod_test!(contexts, "../ut-tests/contexts.rs");
#[cfg(target_os = "none")]
unittest::path_mod_test!(#[cfg(feature = "smp")] mod concurrency, "../ut-tests/concurrency.rs");
#[cfg(target_os = "none")]
unittest::path_mod_test!(mod stress, "../ut-tests/stress.rs");
