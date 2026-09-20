// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User program loading and exec image setup over the memory-management APIs.
//!
//! Create an [`ExecRequest`], optionally call [`ExecRequest::prepare`] to pin and
//! inspect the executable, and use [`load_user_app_request`] to replace an address
//! space. The loader owns image layout; MM owns mappings and pages.
//! [`ExecFailure`] tells callers whether an error occurred before or after the
//! old address space was cleared.
//!
//! # Example
//!
//! With kernel allocation initialized, an owned request can be built without
//! accessing the filesystem; `prepare` later needs a valid current fs context.
//!
//! ```no_run
//! extern crate alloc;
//! use alloc::{string::String, vec};
//!
//! use kexec::ExecRequest;
//! let request = ExecRequest::from_path(
//!     "/bin/app",
//!     vec![String::from("app")],
//!     vec![],
//!     kcred::initial_cred(),
//! );
//! assert_eq!(request.args()[0], "app");
//! ```

#![no_std]
#![warn(missing_docs)]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod elf_image;
mod loader;
mod lru_cache;

pub use self::loader::{
    BinPrm, ExecFailure, ExecRequest, ExecSource, clear_elf_cache, load_user_app_request,
};
