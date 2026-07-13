// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Naming helpers for generated interface symbols.

use quote::format_ident;
use syn::{
    Error, Expr, FnArg, Ident, Pat, Path, Signature, Type, parse_quote, punctuated::Punctuated,
    token::Comma,
};

/// Extracts argument expressions for forwarding an interface call.
pub fn extract_caller_args(sig: &Signature) -> Result<Punctuated<Expr, Comma>, Error> {
    let mut args = Punctuated::new();
    for arg in &sig.inputs {
        if let FnArg::Typed(arg) = arg {
            if let Pat::Ident(ident) = &*arg.pat {
                args.push(parse_quote! { #ident });
            } else {
                return Err(Error::new_spanned(
                    &arg.pat,
                    "interface arguments must use identifier patterns",
                ));
            }
        }
    }
    Ok(args)
}

/// Extracts argument types for function-pointer signature checks.
pub fn extract_arg_types(sig: &Signature) -> Punctuated<Type, Comma> {
    let mut types = Punctuated::new();
    for arg in &sig.inputs {
        if let FnArg::Typed(arg) = arg {
            types.push((*arg.ty).clone());
        }
    }
    types
}

/// Returns the last path segment as the interface name.
pub fn interface_name_from_path(path: &Path) -> Result<&Ident, Error> {
    path.segments
        .last()
        .map(|segment| &segment.ident)
        .ok_or_else(|| Error::new_spanned(path, "expected an interface path"))
}

/// Generates the hidden module name that owns extern declarations.
pub fn extern_mod_name(interface_name: &Ident) -> Ident {
    format_ident!("__kiface_{}_extern", interface_name)
}

/// Generates the local exported function identifier used by a provider.
pub fn exported_fn_ident(interface_name: &Ident, fn_name: &Ident) -> Ident {
    format_ident!("__kiface_export_{}_{}", interface_name, fn_name)
}

/// Generates the link symbol for an interface method.
pub fn exported_symbol(namespace: Option<&str>, interface_name: &Ident, fn_name: &Ident) -> String {
    if let Some(namespace) = namespace {
        format!("__kiface_{namespace}_{interface_name}_{fn_name}")
    } else {
        format!("__kiface_{interface_name}_{fn_name}")
    }
}
