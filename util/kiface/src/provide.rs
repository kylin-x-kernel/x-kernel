// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Implementation of the `#[provide]` macro.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Attribute, Error, ImplItem, ItemImpl, Type};

use crate::{
    args::ProvideArgs,
    naming::{exported_fn_ident, exported_symbol, extract_arg_types, interface_name_from_path},
    validator::validate_interface_provider_method,
};

fn cfg_attrs(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
}

/// Expands a provider impl into exported interface method symbols.
pub fn provide(implementation: ItemImpl, args: ProvideArgs) -> Result<TokenStream, Error> {
    if !implementation.generics.params.is_empty() {
        return Err(crate::errors::generic_not_allowed_error(
            &implementation.generics,
        ));
    }

    let interface_path = if let Some((_, path, _)) = &implementation.trait_ {
        path
    } else {
        match implementation.self_ty.as_ref() {
            Type::Path(path) if path.qself.is_none() => &path.path,
            _ => {
                return Err(Error::new_spanned(
                    &implementation.self_ty,
                    "expected an interface facade path",
                ));
            }
        }
    };
    let interface_name = interface_name_from_path(interface_path)?;
    let namespace = args.namespace.as_deref();

    let mut exported_fns = Vec::new();
    for item in &implementation.items {
        let ImplItem::Fn(method) = item else {
            return Err(Error::new_spanned(
                item,
                "interface providers may only contain function items",
            ));
        };

        validate_interface_provider_method(method)?;

        let attrs = &method.attrs;
        let sig = &method.sig;
        let fn_name = &sig.ident;
        let arg_types = extract_arg_types(sig);
        let output = sig.output.clone();
        let exported_fn_ident = exported_fn_ident(interface_name, fn_name);
        let symbol = exported_symbol(namespace, interface_name, fn_name);
        let body = &method.block;
        let mut exported_sig = sig.clone();
        exported_sig.ident = exported_fn_ident.clone();
        let cfg_attrs = cfg_attrs(attrs);

        exported_fns.push(quote! {
            #(#cfg_attrs)*
            const _: fn(#arg_types) #output = #interface_path::#fn_name;

            #(#attrs)*
            #[doc(hidden)]
            #[allow(non_snake_case)]
            #[unsafe(export_name = #symbol)]
            extern "Rust" #exported_sig #body
        });
    }

    Ok(quote! {
        #(#exported_fns)*
    })
}
