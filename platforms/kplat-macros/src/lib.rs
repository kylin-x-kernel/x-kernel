// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Procedural macros for platform entry points and device interfaces.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{Error, FnArg, ItemTrait, TraitItem};
fn err_ts(e: Error) -> TokenStream {
    e.to_compile_error().into()
}

/// Generates dispatch wrappers for a platform device interface trait.
#[proc_macro_attribute]
pub fn device_interface(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return err_ts(Error::new(Span::call_site(), "Attr must be empty"));
    }
    let tr = syn::parse_macro_input!(item as ItemTrait);
    let tr_id = &tr.ident;
    let mut defs = vec![];
    for it in &tr.items {
        if let TraitItem::Fn(m) = it {
            let m_attrs = &m.attrs;
            let m_sig = &m.sig;
            let m_id = &m_sig.ident;
            let mut args = vec![];
            for arg in &m_sig.inputs {
                match arg {
                    FnArg::Receiver(_) => {
                        return err_ts(Error::new_spanned(arg, "self not allowed"));
                    }
                    FnArg::Typed(t) => args.push(t.pat.clone()),
                }
            }
            defs.push(quote! {
                #(#m_attrs)*
                #[inline]
                pub #m_sig {
                    crate::__priv::dispatch!(#tr_id::#m_id, #(#args),* )
                }
            });
        }
    }
    quote! {
        #[crate::__priv::interface_def]
        #tr
        #(#defs)*
    }
    .into()
}
