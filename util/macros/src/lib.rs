// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Procedural macros for kernel utility helpers.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{
    AttrStyle, Attribute, DeriveInput, Error, Ident, Item, ItemFn, ItemImpl, ItemMod, LitInt,
    Token, parse_macro_input, parse_quote, punctuated::Punctuated,
};

/// Registers a function in the kernel runtime init array.
///
/// The function should have no input arguments and return nothing.
#[proc_macro_attribute]
pub fn register_init(attr: TokenStream, function: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return Error::new(
            Span::call_site(),
            "expect an empty attribute: `#[register_init]`",
        )
        .to_compile_error()
        .into();
    }

    let item: Item = parse_macro_input!(function as Item);
    if let Item::Fn(func) = item {
        let attributes = &func.attrs;
        let name = &func.sig.ident;
        let name_str = name.to_string();
        let name_ident = format_ident!("_INIT_{}", name_str);
        let output = &func.sig.output;
        // Init functions should not have any return value.
        if let syn::ReturnType::Type(..) = output {
            return Error::new(
                Span::call_site(),
                "expect no return value for the init function",
            )
            .to_compile_error()
            .into();
        }
        let inputs = &func.sig.inputs;
        // Init functions should not have any input arguments.
        if !inputs.is_empty() {
            return Error::new(
                Span::call_site(),
                "expect no input arguments for the init function",
            )
            .to_compile_error()
            .into();
        }
        let block = &func.block;
        let visibility = &func.vis;

        quote! {
            #[unsafe(link_section = ".init_array")]
            #[used]
            #[allow(non_upper_case_globals)]
            static #name_ident: extern "C" fn() = #name;

            #(#attributes)*
            #visibility extern "C" fn #name() {
                #block
            }
        }
        .into()
    } else {
        Error::new(Span::call_site(), "expect a function to be registered")
            .to_compile_error()
            .into()
    }
}

/// Marks a module as test-only code.
///
/// This is equivalent to `#[cfg(unittest)]` but more readable.
/// The module will only be compiled when `--test` flag is passed to the build.
///
/// The module also allows `clippy::unwrap_used`, `clippy::expect_used`,
/// `clippy::panic` and `missing_docs`: clippy's own test exemptions only
/// recognise `#[cfg(test)]`, not `cfg(unittest)`.
///
/// Only inline modules (`mod name { ... }`) are supported on stable Rust;
/// attribute macros on file modules (`mod name;`) are rejected with E0658.
///
/// # Example
///
/// ```rust
/// use unittest::{def_test, mod_test};
///
/// #[mod_test]
/// mod tests {
///     use super::*;
///
///     #[def_test]
///     fn test_addition() {
///         assert_eq!(2 + 2, 4);
///     }
///
///     #[def_test]
///     fn test_string() {
///         assert_eq!("hello", "hello");
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn mod_test(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut module = parse_macro_input!(item as ItemMod);

    // An inline module takes the allowances as inner attributes, a file module
    // (`mod foo;`) as outer ones. rustc currently rejects attribute macros on
    // file modules (E0658), so the outer form is not reachable on stable.
    let mut allows: [Attribute; 3] = [
        parse_quote! {
            #[allow(
                clippy::unwrap_used,
                clippy::expect_used,
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: unittest module: test helpers may panic on failure"
            )]
        },
        parse_quote! {
            #[allow(missing_docs, reason = "CG-024-lint-exceptions: missing_docs: unittest module: test code is outside rustdoc scope")]
        },
        parse_quote! {
            #[allow(
                clippy::too_many_lines,
                reason = "CG-028-focused-functions: unittest module: a test scenario reads as one linear script"
            )]
        },
    ];
    if module.content.is_some() {
        for attr in &mut allows {
            attr.style = AttrStyle::Inner(Token![!](Span::call_site()));
        }
    }
    module.attrs.extend(allows);

    quote! {
        #[cfg(unittest)]
        #module
    }
    .into()
}

/// Marks a function as a unit test.
///
/// # Example
///
/// ```rust
/// use unittest::def_test;
///
/// #[def_test]
/// fn test_addition() {
///     let a = 2 + 2;
///     assert_eq!(a, 4);
/// }
/// ```
///
/// The test function can optionally return `TestResult`. If it doesn't return anything,
/// the function body is wrapped to return `TestResult::Ok` on success.
///
/// # Attributes
/// - `#[def_test]` - Normal test
/// - `#[def_test(ignore)]` - Test will be skipped
/// - `#[def_test(should_panic)]` - Test expects panic (not fully supported in no_std)
/// - `#[def_test(user)]` - Test runs in a newly constructed user task
/// - `#[def_test(serial)]` - Test must run sequentially (not parallel-safe)
#[proc_macro_attribute]
pub fn def_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    let args = parse_macro_input!(attr with Punctuated::<Ident, Token![,]>::parse_terminated);
    generate_function_test(args, input)
}

/// Generate test code for a single function
fn generate_function_test(args: Punctuated<Ident, Token![,]>, input: ItemFn) -> TokenStream {
    let mut ignore = false;
    let mut should_panic = false;
    let mut serial = false;
    let mut use_user_executor = false;

    for arg in args {
        match arg.to_string().as_str() {
            "ignore" => ignore = true,
            "should_panic" => should_panic = true,
            "serial" => serial = true,
            "user" => use_user_executor = true,
            other => {
                return Error::new(
                    arg.span(),
                    format!("unsupported def_test argument: {other}"),
                )
                .to_compile_error()
                .into();
            }
        }
    }

    let fn_name = &input.sig.ident;
    let fn_attrs = &input.attrs;
    let fn_stmts = &input.block.stmts;

    // Check if function returns TestResult
    let has_return_type = !matches!(input.sig.output, syn::ReturnType::Default);

    // Generate a unique identifier for the test descriptor
    let descriptor_name = format_ident!(
        "__UNITTEST_DESCRIPTOR_{}",
        fn_name.to_string().to_uppercase()
    );

    // clippy's `allow-*-in-tests` only recognises `#[cfg(test)]` / `#[test]`,
    // so the unittest body is exempted here.
    let unittest_allow = quote! {
        #[allow(
            clippy::unwrap_used,
            clippy::expect_used,
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: unittest body: a failed assertion is the intended outcome"
        )]
        #[allow(
            clippy::too_many_lines,
            reason = "CG-028-focused-functions: unittest body: a test scenario reads as one linear script"
        )]
    };

    // The test function itself becomes the wrapper - body is embedded directly
    // This way assert macros can use `return TestResult::Failed` correctly
    let test_fn = if has_return_type {
        // Function already returns TestResult
        quote! {
            #(#fn_attrs)*
            #unittest_allow
            fn #fn_name() -> unittest::TestResult {
                #(#fn_stmts)*
            }
        }
    } else {
        // Function doesn't return anything, wrap it to return TestResult
        quote! {
            #(#fn_attrs)*
            #unittest_allow
            fn #fn_name() -> unittest::TestResult {
                #(#fn_stmts)*
                unittest::TestResult::Ok
            }
        }
    };

    let ignore_val = ignore;
    let should_panic_val = should_panic;
    let serial_val = serial;
    let execution_mode = if use_user_executor {
        quote!(unittest::TestExecutionMode::User)
    } else {
        quote!(unittest::TestExecutionMode::Standard)
    };
    let fn_name_str = fn_name.to_string();

    // Use linker section to collect test descriptors
    // The linker script defines __unittest_start and __unittest_end symbols
    let output = quote! {
        #test_fn

        #[used]
        #[unsafe(link_section = ".unittest")]
        #[allow(non_upper_case_globals)]
        static #descriptor_name: unittest::TestDescriptor = unittest::TestDescriptor::new(
            #fn_name_str,
            module_path!(),
            #fn_name,
            #should_panic_val,
            #ignore_val,
            #serial_val,
            #execution_mode,
        );
    };

    output.into()
}

// ======== UserRead / UserWrite derive macros ========

/// Resolve the crate path for `posix_types::__private` trait aliases.
///
/// Returns `crate` when invoked inside `posix-types` itself,
/// and the external crate name otherwise. Missing dependencies produce a
/// derive-site compile error rather than a procedural-macro panic.
fn posix_types_private_path() -> syn::Result<proc_macro2::TokenStream> {
    use proc_macro_crate::{FoundCrate, crate_name};
    let found = crate_name("posix-types").map_err(|error| {
        syn::Error::new(
            Span::call_site(),
            format!("posix-types dependency is required: {error}"),
        )
    })?;
    Ok(match found {
        FoundCrate::Itself => quote!(crate),
        FoundCrate::Name(name) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!(#ident)
        }
    })
}

/// Derive macro that generates `unsafe impl UserRead for T {}`.
///
/// The caller asserts that any bit pattern read from user memory is a valid `T`.
#[proc_macro_derive(UserRead)]
pub fn derive_user_read(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let path = match posix_types_private_path() {
        Ok(path) => path,
        Err(error) => return error.to_compile_error().into(),
    };
    quote! {
        unsafe impl #impl_generics #path::__private::UserReadTrait for #name #ty_generics #where_clause {}
    }
    .into()
}

/// Derive macro that generates `unsafe impl UserWrite for T {}`.
///
/// The caller asserts that writing `T` to user memory as raw bytes is always safe.
#[proc_macro_derive(UserWrite)]
pub fn derive_user_write(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let path = match posix_types_private_path() {
        Ok(path) => path,
        Err(error) => return error.to_compile_error().into(),
    };
    quote! {
        unsafe impl #impl_generics #path::__private::UserWriteTrait for #name #ty_generics #where_clause {}
    }
    .into()
}

// ======== DRM ioctl attribute macro ========

/// Arguments parsed from `#[drm_ioctl(iowr, 0x00)]` or `#[drm_ioctl(cmd = 0x1234)]`.
enum DrmIoctlArgs {
    Formula { dir: Ident, nr: LitInt },
    Raw { cmd: LitInt },
}

impl syn::parse::Parse for DrmIoctlArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let lookahead = input.lookahead1();
        if lookahead.peek(Ident) {
            let ident: Ident = input.parse()?;
            if ident == "cmd" {
                let _: Token![=] = input.parse()?;
                let cmd: LitInt = input.parse()?;
                Ok(DrmIoctlArgs::Raw { cmd })
            } else {
                let _: Token![,] = input.parse()?;
                let nr: LitInt = input.parse()?;
                Ok(DrmIoctlArgs::Formula { dir: ident, nr })
            }
        } else {
            Err(lookahead.error())
        }
    }
}

/// Attribute macro that injects `const CMD` into a `DrmIoctl` impl block.
///
/// # Usage
///
/// ```ignore
/// #[drm_ioctl(iowr, 0x00)]
/// impl DrmIoctl for DrmVersion {
///     fn handle(dev: &dyn DeviceFileOps, arg: UserPtr<Self>) -> VfsResult<usize> {
///         Ok(0)
///     }
/// }
/// ```
///
/// The macro parses the impl block, injects `const CMD: u32 = iowr::<DrmVersion>(DRM_TYPE, 0x00);`
/// at the top, and keeps everything else intact.
#[proc_macro_attribute]
pub fn drm_ioctl(args: TokenStream, input: TokenStream) -> TokenStream {
    let mut impl_block = parse_macro_input!(input as ItemImpl);
    let args = parse_macro_input!(args as DrmIoctlArgs);

    // Extract the struct name from `impl DrmIoctl for T`.
    // self_ty is the type after `for`, e.g. `DrmVersion`.
    let struct_name = if let syn::Type::Path(type_path) = &*impl_block.self_ty {
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: syn parses a Type::Path with at least \
                      one path segment"
        )]
        &type_path.path.segments.last().unwrap().ident
    } else {
        return Error::new_spanned(&impl_block.self_ty, "expected a path type")
            .to_compile_error()
            .into();
    };

    // Build the CMD expression.
    let cmd_expr = match args {
        DrmIoctlArgs::Formula { dir, nr } => {
            quote! { #dir::<#struct_name>(DRM_TYPE, #nr) }
        }
        DrmIoctlArgs::Raw { cmd } => {
            quote! { #cmd }
        }
    };

    // Inject `const CMD` at the beginning of the impl block.
    let const_cmd: syn::ImplItem = syn::parse_quote! {
        const CMD: u32 = #cmd_expr;
    };
    impl_block.items.insert(0, const_cmd);

    let expanded = quote! { #impl_block };
    TokenStream::from(expanded)
}
