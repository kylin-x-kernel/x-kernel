// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User program loading and exec image setup.

#![no_std]
#![warn(missing_docs)]
#![allow(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod loader;
mod lru_cache;

pub use self::loader::{
    BinPrm, ExecRequest, ExecSource, clear_elf_cache, load_user_app, load_user_app_request,
};
