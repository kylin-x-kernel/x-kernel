//! Procedural macros for kernel utility helpers.
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{Error, Item, ItemFn, parse_macro_input};

/// Register a constructor function to be called before `main`.
///
/// The function should have no input arguments and return nothing.
#[proc_macro_attribute]
pub fn register_init(attr: TokenStream, function: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return Error::new(
            Span::call_site(),
            "expect an empty attribute: `#[register_ctor]`",
        )
        .to_compile_error()
        .into();
    }

    let item: Item = parse_macro_input!(function as Item);
    if let Item::Fn(func) = item {
        let name = &func.sig.ident;
        let name_str = name.to_string();
        let name_ident = format_ident!("_INIT_{}", name_str);
        let output = &func.sig.output;
        // Constructor functions should not have any return value.
        if let syn::ReturnType::Type(..) = output {
            return Error::new(
                Span::call_site(),
                "expect no return value for the constructor function",
            )
            .to_compile_error()
            .into();
        }
        let inputs = &func.sig.inputs;
        // Constructor functions should not have any input arguments.
        if !inputs.is_empty() {
            return Error::new(
                Span::call_site(),
                "expect no input arguments for the constructor function",
            )
            .to_compile_error()
            .into();
        }
        let block = &func.block;

        quote! {
            #[unsafe(link_section = ".init_array")]
            #[used]
            #[allow(non_upper_case_globals)]
            static #name_ident: extern "C" fn() = #name;

            #[unsafe(no_mangle)]
            #[allow(non_upper_case_globals)]
            pub extern "C" fn #name() {
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
/// This allows using `assert_eq!` and other assertion macros that use `return`.
///
/// # Attributes
/// - `#[def_test]` - Normal test
/// - `#[def_test(ignore)]` - Test will be skipped
/// - `#[def_test(should_panic)]` - Test expects panic (not fully supported in no_std)
#[proc_macro_attribute]
pub fn def_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);

    // Parse attributes
    let attr_str = attr.to_string();
    let ignore = attr_str.contains("ignore");
    let should_panic = attr_str.contains("should_panic");

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

    // The test function itself becomes the wrapper - body is embedded directly
    // This way assert macros can use `return TestResult::Failed` correctly
    let test_fn = if has_return_type {
        // Function already returns TestResult
        quote! {
            #(#fn_attrs)*
            fn #fn_name() -> unittest::TestResult {
                #(#fn_stmts)*
            }
        }
    } else {
        // Function doesn't return anything, wrap it to return TestResult
        quote! {
            #(#fn_attrs)*
            fn #fn_name() -> unittest::TestResult {
                #(#fn_stmts)*
                unittest::TestResult::Ok
            }
        }
    };

    let ignore_val = ignore;
    let should_panic_val = should_panic;
    let fn_name_str = fn_name.to_string();

    // Use linker section to collect test descriptors
    // The linker script defines __unittest_start and __unittest_end symbols
    // The generated code is gated by #[cfg(unittest)] so tests
    // are only compiled when --cfg unittest is passed via RUSTFLAGS
    let output = quote! {
        #[cfg(unittest)]
        #test_fn

        #[cfg(unittest)]
        #[used]
        #[unsafe(link_section = ".unittest")]
        #[allow(non_upper_case_globals)]
        static #descriptor_name: unittest::TestDescriptor = unittest::TestDescriptor::new(
            #fn_name_str,
            #fn_name,
            #should_panic_val,
            #ignore_val,
        );
    };

    output.into()
}
