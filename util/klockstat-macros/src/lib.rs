// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Procedural macros for `klockstat`.

mod static_lock;

use proc_macro::TokenStream;

/// Registers a static lock for per-class contention statistics.
///
/// # Examples
///
/// ```ignore
/// static_lock! {
///     static PROCESS_TABLE: RwLock<ProcessTable> = RwLock::new(ProcessTable::new());
/// }
/// ```
#[proc_macro]
pub fn static_lock(input: TokenStream) -> TokenStream {
    static_lock::expand_static_lock(input)
}
