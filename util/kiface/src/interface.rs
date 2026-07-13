// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Implementation of the `#[interface]` macro.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Error, ItemTrait, TraitItem};

use crate::{
    args::InterfaceArgs,
    naming::{exported_symbol, extern_mod_name, extract_caller_args},
    validator::validate_interface_method,
};

/// Expands an interface trait into a facade type with direct call methods.
pub fn interface(interface: ItemTrait, args: InterfaceArgs) -> Result<TokenStream, Error> {
    if !interface.generics.params.is_empty() {
        return Err(crate::errors::generic_not_allowed_error(
            &interface.generics,
        ));
    }

    if args.optional {
        return Err(Error::new_spanned(
            &interface.ident,
            "`#[kiface::interface(optional)]` is reserved but not implemented yet; optional \
             interfaces will wait for stable `extern_weak` support so missing providers can be \
             represented as `None` without a registry dependency",
        ));
    }

    let attrs = &interface.attrs;
    let vis = &interface.vis;
    let interface_name = &interface.ident;
    let extern_mod_name = extern_mod_name(interface_name);
    let namespace = args.namespace.as_deref();

    let mut extern_decls = Vec::new();
    let mut methods = Vec::new();

    for item in &interface.items {
        let TraitItem::Fn(method) = item else {
            return Err(Error::new_spanned(
                item,
                "interfaces may only contain function items",
            ));
        };

        validate_interface_method(method)?;

        let method_attrs = &method.attrs;
        let sig = &method.sig;
        let fn_name = &sig.ident;
        let symbol = exported_symbol(namespace, interface_name, fn_name);
        let caller_args = extract_caller_args(sig)?;

        extern_decls.push(quote! {
            #(#method_attrs)*
            #[link_name = #symbol]
            pub #sig;
        });

        methods.push(quote! {
            #(#method_attrs)*
            #[inline]
            #vis #sig {
                // SAFETY: the provider signature is checked against this facade
                // method by the provider macro's const function-pointer type
                // check, and the final image must link exactly one matching
                // exported provider symbol.
                unsafe { #extern_mod_name::#fn_name(#caller_args) }
            }
        });
    }

    Ok(quote! {
        #(#attrs)*
        #vis enum #interface_name {}

        #[doc(hidden)]
        #[allow(non_snake_case)]
        mod #extern_mod_name {
            use super::*;

            unsafe extern "Rust" {
                #(#extern_decls)*
            }
        }

        impl #interface_name {
            #(#methods)*
        }
    })
}
