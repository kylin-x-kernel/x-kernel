use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{Error, FnArg, ItemFn, ItemTrait, ReturnType, TraitItem};

/// Helper function to generate a compiler error.
fn generate_error(err: Error) -> TokenStream {
    err.to_compile_error().into()
}

/// A shared function for validating main functions for primary and secondary cores.
fn validate_main_fn(
    item: TokenStream,
    expected_args: usize,
    name: &str,
    error_message: &str,
) -> TokenStream {
    let parsed_fn = syn::parse_macro_input!(item as ItemFn);

    // Check if the return type is `!`
    let mut is_valid = if let ReturnType::Type(_, ty) = &parsed_fn.sig.output {
        quote! { #ty }.to_string() != "!"
    } else {
        true
    };

    // Validate function arguments
    let fn_args = &parsed_fn.sig.inputs;
    for arg in fn_args.iter() {
        if let FnArg::Typed(pat) = arg {
            let arg_type = &pat.ty;
            if quote! { #arg_type }.to_string() != "usize" {
                is_valid = true;
                break;
            }
        }
    }

    if fn_args.len() != expected_args {
        is_valid = true;
    }

    if is_valid {
        generate_error(Error::new(Span::call_site(), error_message))
    } else {
        quote! {
            #[unsafe(export_name = #name)]
            #parsed_fn
        }
        .into()
    }
}

/// Marks a function for execution on the primary core after platform initialization.
/// The function should have the signature `fn(cpu_id: usize, arg: usize) -> !`.
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return generate_error(Error::new(
            Span::call_site(),
            "Expected an empty attribute or `#[axplat::main]`",
        ));
    }

    validate_main_fn(
        item,
        2,
        "__axplat_main",
        "Expected a function with the signature `fn(cpu_id: usize, arg: usize) -> !`",
    )
}

/// Marks a function for execution on secondary cores after platform initialization.
/// The function should have the signature `fn(cpu_id: usize) -> !`.
#[proc_macro_attribute]
pub fn secondary_main(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return generate_error(Error::new(
            Span::call_site(),
            "Expected an empty attribute or `#[axplat::secondary_main]`",
        ));
    }

    validate_main_fn(
        item,
        1,
        "__axplat_secondary_main",
        "Expected a function with the signature `fn(cpu_id: usize) -> !`",
    )
}

#[doc(hidden)]
/// Marks a trait to define platform interfaces.
#[proc_macro_attribute]
pub fn def_plat_interface(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return generate_error(Error::new(
            Span::call_site(),
            "Expected an empty attribute: `#[def_plat_interface]`",
        ));
    }

    let parsed_trait = syn::parse_macro_input!(item as ItemTrait);
    let trait_ident = &parsed_trait.ident;

    let mut method_definitions = vec![];

    for trait_item in &parsed_trait.items {
        if let TraitItem::Fn(method) = trait_item {
            let method_attrs = &method.attrs;
            let method_sig = &method.sig;
            let method_name = &method_sig.ident;

            let mut method_args = vec![];
            for method_arg in &method_sig.inputs {
                match method_arg {
                    FnArg::Receiver(_) => {
                        return generate_error(Error::new_spanned(
                            method_arg,
                            "`self` is not allowed in the interface definition",
                        ));
                    }
                    FnArg::Typed(ty) => method_args.push(ty.pat.clone()),
                }
            }

            method_definitions.push(quote! {
                #(#method_attrs)*
                #[inline]
                pub #method_sig {
                    crate::__priv::call_interface!(#trait_ident::#method_name, #(#method_args),* )
                }
            });
        }
    }

    quote! {
        #[crate::__priv::def_interface]
        #parsed_trait

        #(#method_definitions)*
    }
    .into()
}
