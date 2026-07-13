// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Attribute argument parsers.

use syn::{
    Ident, LitStr, Result, Token,
    parse::{Parse, ParseStream},
};

/// Arguments for `#[interface]`.
#[derive(Default)]
pub struct InterfaceArgs {
    /// Optional namespace used to disambiguate exported symbols.
    pub namespace: Option<String>,
    /// Whether the provider may be absent.
    pub optional: bool,
}

impl Parse for InterfaceArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut args = Self::default();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            if key == "optional" {
                args.optional = true;
            } else if key == "namespace" {
                input.parse::<Token![=]>()?;
                if input.peek(LitStr) {
                    args.namespace = Some(input.parse::<LitStr>()?.value());
                } else {
                    args.namespace = Some(input.parse::<Ident>()?.to_string());
                }
            } else {
                return Err(syn::Error::new_spanned(key, "unknown interface argument"));
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(args)
    }
}

/// Arguments for `#[provide]`.
#[derive(Default)]
pub struct ProvideArgs {
    /// Optional namespace used to match the interface definition.
    pub namespace: Option<String>,
}

impl Parse for ProvideArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut args = Self::default();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            if key == "namespace" {
                input.parse::<Token![=]>()?;
                if input.peek(LitStr) {
                    args.namespace = Some(input.parse::<LitStr>()?.value());
                } else {
                    args.namespace = Some(input.parse::<Ident>()?.to_string());
                }
            } else {
                return Err(syn::Error::new_spanned(key, "unknown provide argument"));
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(args)
    }
}
