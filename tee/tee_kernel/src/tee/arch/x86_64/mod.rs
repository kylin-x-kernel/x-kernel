// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(feature = "csv_huk_key")]
pub(crate) mod hygon_csv;
#[cfg(feature = "csv_huk_key")]
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    reason = "CG-024-lint-exceptions: dead_code, non_camel_case_types, non_snake_case, \
              non_upper_case_globals, unreachable_pub, clippy::missing_safety_doc, \
              clippy::undocumented_unsafe_blocks: rust-bindgen output for the Hygon CSV guest ABI \
              is regenerated, not edited; it keeps C names, unused public bindings and unsafe \
              helpers without Rust safety documentation"
)]
mod hygon_csv_bindings;
