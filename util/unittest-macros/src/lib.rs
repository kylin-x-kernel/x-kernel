//! Procedural macros for the unittest framework
//!
//! This crate provides the `#[unittest]` attribute macro for marking test functions.
//! Tests are automatically collected using linker sections and can be run
//! with `unittest::test_run()`.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, ItemFn};

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
    let output = quote! {
        #test_fn

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
