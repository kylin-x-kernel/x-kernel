// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User program loading and exec image setup over the memory-management APIs.
//!
//! Create an [`ExecRequest`], optionally call [`ExecRequest::prepare`] to pin and
//! inspect the executable. [`prepare_user_app_request`] validates and owns the
//! image/stack; [`load_prepared_user_app`] installs it into a user-empty MM.
//! The loader owns image layout; MM owns mappings and pages. Callers own the
//! commitment boundary and failure policy; neither loader operation clears an MM.
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

#[cfg(unittest)]
pub use elf_image::tests::executable_bytes as test_executable_bytes;
#[cfg(unittest)]
pub use loader::PartialInstallProbe;

pub use self::loader::{
    BinPrm, ExecRequest, ExecSource, PreparedExecImage, clear_elf_cache, load_prepared_user_app,
    prepare_user_app_request,
};
