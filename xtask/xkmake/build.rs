// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Declares build inputs that Cargo cannot discover automatically.
//!
//! `linker.rs` embeds the kernel linker-script template via `include_str!`;
//! without this declaration Cargo would not rebuild `xkmake` when the
//! template changes.

fn main() {
    println!("cargo:rerun-if-changed=../../linker.lds.S");
}
