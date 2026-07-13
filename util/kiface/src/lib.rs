// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Procedural macros for kernel interfaces.
//!
//! `kiface` models a single-implementation interface boundary. An interface is
//! defined in the crate that owns the call contract and provided by exactly one
//! other crate linked into the final image:
//!
//! ```ignore
//! #[kiface::interface]
//! pub trait KernelEntry {
//!     fn primary(boot_info: usize) -> !;
//! }
//!
//! #[kiface::provide]
//! impl KernelEntry {
//!     fn primary(boot_info: usize) -> ! {
//!         rust_main(boot_info)
//!     }
//! }
//! ```

use proc_macro::TokenStream;
use syn::{ItemImpl, ItemTrait, parse_macro_input};

mod args;
mod errors;
mod interface;
mod naming;
mod provide;
mod validator;

/// Defines a single-implementation kernel interface.
///
/// The input is written as a trait-shaped contract. The macro generates a facade
/// type with inherent methods. Callers invoke the interface directly through
/// `InterfaceName::method(...)`.
#[proc_macro_attribute]
pub fn interface(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as args::InterfaceArgs);
    let interface = parse_macro_input!(item as ItemTrait);

    interface::interface(interface, args)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Provides the implementation for an interface.
///
/// The input may be written as `impl InterfaceName { ... }` or
/// `impl InterfaceName for Provider { ... }`. The macro exports one link
/// symbol per method and removes the impl from the expanded code, so the
/// provider may live in a different crate from the interface facade type.
#[proc_macro_attribute]
pub fn provide(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as args::ProvideArgs);
    let implementation = parse_macro_input!(item as ItemImpl);

    provide::provide(implementation, args)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
