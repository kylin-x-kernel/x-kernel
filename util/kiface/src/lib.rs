// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Procedural macros for kernel interfaces.
//!
//! `kiface` models a single-implementation interface boundary. An interface is
//! defined in the crate that owns the call contract and provided by exactly one
//! other crate linked into the final image:
//!
//! ```no_run
//! # fn rust_main(boot_info: usize) -> ! { loop {} }
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
//!
//! # Interface ownership
//!
//! Organize shared contracts by capability-owning domain, not by consumer.
//! API crates contain contracts and necessary boundary types, never concrete
//! implementations or global runtime state. They must not depend on provider
//! crates or pull unrelated APIs through an umbrella dependency. Before using
//! an interface to break a dependency cycle, correct any misplaced ownership;
//! use a domain API only for the reverse calls that remain. See
//! `docs/design.md`, "Interface Ownership and Dependency Rules", for the policy.

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
///
/// # Arguments
///
/// - `namespace = "name"` (optional): disambiguates the exported symbols so
///   two interfaces with the same method names can coexist; the provider
///   must repeat the identical namespace.
/// - `optional` (reserved): rejected with a compile error; the optional
///   provider mode is not implemented yet.
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
/// Each method is type-checked against the interface facade at provider
/// compile time; a missing or mismatched interface is a compile error,
/// and a missing provider surfaces at final link time.
///
/// # Arguments
///
/// - `namespace = "name"` (optional): must match the namespace used on
///   the corresponding `#[kiface::interface]` definition.
#[proc_macro_attribute]
pub fn provide(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as args::ProvideArgs);
    let implementation = parse_macro_input!(item as ItemImpl);

    provide::provide(implementation, args)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
