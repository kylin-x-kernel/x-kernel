use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{Error, FnArg, ItemFn, ItemTrait, ReturnType, TraitItem};
fn err_ts(e: Error) -> TokenStream {
    e.to_compile_error().into()
}
fn check_fn(t: TokenStream, cnt: usize, exp_name: &str, msg: &str) -> TokenStream {
    let f = syn::parse_macro_input!(t as ItemFn);
    let mut bad = if let ReturnType::Type(_, ty) = &f.sig.output {
        quote! { #ty }.to_string() != "!"
    } else {
        true
    };
    let inputs = &f.sig.inputs;
    // for i in inputs.iter() {
    // if let FnArg::Typed(pt) = i {
    // if quote! { #pt.ty }.to_string() != "usize" {
    // bad = true;
    // break;
    // }
    // }
    // }
    if inputs.len() != cnt {
        bad = true;
    }
    if bad {
        err_ts(Error::new(Span::call_site(), msg))
    } else {
        quote! {
            #[unsafe(export_name = #exp_name)]
            #f
        }
        .into()
    }
}
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return err_ts(Error::new(Span::call_site(), "Attr must be empty"));
    }
    check_fn(
        item,
        2,
        "__kplat_main",
        "Sign: fn(cpu: usize, arg: usize) -> !",
    )
}
#[proc_macro_attribute]
pub fn secondary_main(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return err_ts(Error::new(Span::call_site(), "Attr must be empty"));
    }
    check_fn(
        item,
        1,
        "__kplat_secondary_main",
        "Sign: fn(cpu: usize) -> !",
    )
}
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
