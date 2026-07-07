// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `static_lock!` registers a static lock for per-class contention statistics.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{
    Error, Expr, ExprCall, Ident, ItemStatic, Type, TypePath,
    parse::{Parse, ParseStream},
};

enum LockFamily {
    KsyncMutex,
    KsyncRwLock,
    Kspin { kind: Ident },
}

struct LockPlan {
    family: LockFamily,
    kind: &'static str,
}

enum StaticLockInput {
    Item(ItemStatic),
}

impl Parse for StaticLockInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.fork().parse::<ItemStatic>().is_ok() {
            return Ok(Self::Item(input.parse()?));
        }

        let content;
        syn::braced!(content in input);
        Ok(Self::Item(content.parse()?))
    }
}

fn dependency_path(name: &str) -> syn::Result<proc_macro2::TokenStream> {
    match crate_name(name) {
        Ok(FoundCrate::Itself) => Ok(quote!(crate)),
        Ok(FoundCrate::Name(crate_name)) => {
            let ident = Ident::new(&crate_name, Span::call_site());
            Ok(quote!(#ident))
        }
        Err(err) => Err(Error::new(
            Span::call_site(),
            format!("static_lock! requires `{name}` in Cargo.toml: {err}"),
        )),
    }
}

fn type_last_ident(ty: &Type) -> syn::Result<Ident> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return Err(Error::new_spanned(
            ty,
            "expected a path type such as `Mutex<T>` or `SpinNoIrq<T>`",
        ));
    };
    path.segments
        .last()
        .map(|segment| segment.ident.clone())
        .ok_or_else(|| Error::new_spanned(ty, "expected a named lock type"))
}

fn classify_lock_type(ty: &Type) -> syn::Result<LockPlan> {
    match type_last_ident(ty)?.to_string().as_str() {
        "Mutex" => Ok(LockPlan {
            family: LockFamily::KsyncMutex,
            kind: "Mutex",
        }),
        "RwLock" => Ok(LockPlan {
            family: LockFamily::KsyncRwLock,
            kind: "RwLock",
        }),
        "SpinNoIrq" => Ok(LockPlan {
            family: LockFamily::Kspin {
                kind: Ident::new("SpinNoIrq", Span::call_site()),
            },
            kind: "SpinNoIrq",
        }),
        "SpinNoPreempt" => Ok(LockPlan {
            family: LockFamily::Kspin {
                kind: Ident::new("SpinNoPreempt", Span::call_site()),
            },
            kind: "SpinNoPreempt",
        }),
        "SpinRaw" => Ok(LockPlan {
            family: LockFamily::Kspin {
                kind: Ident::new("SpinRaw", Span::call_site()),
            },
            kind: "SpinRaw",
        }),
        other => Err(Error::new_spanned(
            ty,
            format!(
                "unsupported lock type `{other}`; expected Mutex, RwLock, SpinNoIrq, \
                 SpinNoPreempt, or SpinRaw"
            ),
        )),
    }
}

fn extract_init(expr: &Expr) -> proc_macro2::TokenStream {
    if let Expr::Call(ExprCall { func, args, .. }) = expr
        && let Expr::Path(path) = &**func
        && path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "new")
        && let Some(first) = args.first()
    {
        return quote!(#first);
    }
    quote!(#expr)
}

pub(crate) fn expand_static_lock(input: TokenStream) -> TokenStream {
    expand_static_lock_result(input)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn expand_static_lock_result(input: TokenStream) -> syn::Result<proc_macro2::TokenStream> {
    let StaticLockInput::Item(item) = syn::parse(input)?;
    // Counter-set types and `linkme` are re-exported by `ksync` / `kspin`, so callers
    // only need the lock crate that matches the static lock type in Cargo.toml.

    let ItemStatic {
        attrs,
        vis,
        ident,
        ty,
        expr,
        ..
    } = item;
    let init = &*expr;
    let plan = classify_lock_type(&ty)?;
    let init_tokens = extract_init(init);
    let stats_ident = format_ident!("{}_STATS", ident);
    let entry_ident = format_ident!("{}_LOCK_STATS_ENTRY", ident);
    let kind = plan.kind;

    let (lock_static, klockstat) = match &plan.family {
        LockFamily::KsyncMutex => {
            let ksync = dependency_path("ksync")?;
            let lock_static = quote! {
                #vis static #ident: #ty =
                    #ksync::Mutex::new_with_stats(#init_tokens, &#stats_ident);
            };
            (lock_static, ksync)
        }
        LockFamily::KsyncRwLock => {
            let ksync = dependency_path("ksync")?;
            let lock_static = quote! {
                #vis static #ident: #ty =
                    #ksync::RwLock::new_with_stats(#init_tokens, &#stats_ident);
            };
            (lock_static, ksync)
        }
        LockFamily::Kspin { kind: spin_kind } => {
            let kspin = dependency_path("kspin")?;
            let lock_static = quote! {
                #vis static #ident: #ty =
                    #kspin::#spin_kind::new_with_stats(#init_tokens, &#stats_ident);
            };
            (lock_static, kspin)
        }
    };

    Ok(quote! {
        static #stats_ident: #klockstat::LockClassStats = #klockstat::LockClassStats::new(
            concat!(file!(), ":", line!()),
            #kind,
        );

        #[#klockstat::linkme::distributed_slice(#klockstat::LOCK_CLASSES)]
        #[linkme(crate = #klockstat::linkme)]
        static #entry_ident: &'static #klockstat::LockClassStats = &#stats_ident;

        #(#attrs)*
        #lock_static
    })
}
