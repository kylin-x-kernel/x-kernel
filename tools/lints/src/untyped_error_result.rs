// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Detects untyped errors in function return signatures. See CG-010.

use rustc_hir::{
    ForeignItem, ForeignItemKind, ImplItem, ImplItemKind, Item, ItemKind, TraitItem, TraitItemKind,
};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::symbol::sym;

rustc_session::declare_lint! {
    /// ### What it does
    /// Reports function signatures returning `Result<T, ()>`, `Result<T, &str>` or `Result<T, String>`.
    /// ### Why is this bad?
    /// These error types do not express a layer's failure contract.
    /// ### Example
    /// `fn parse() -> Result<Value, String>`
    pub UNTYPED_ERROR_RESULT, Deny, "Result return signatures must use typed errors", report_in_external_macro
}
rustc_session::declare_lint_pass!(UntypedErrorResult => [UNTYPED_ERROR_RESULT]);

/// `enum_dispatch` generates conversion impls whose error type is fixed to
/// `&'static str`; the source enum cannot choose another associated type.
fn is_enum_dispatch_conversion(cx: &LateContext<'_>, id: rustc_hir::def_id::DefId) -> bool {
    let impl_id = cx.tcx.parent(id);
    if !matches!(
        cx.tcx.def_kind(impl_id),
        rustc_hir::def::DefKind::Impl { of_trait: true }
    ) {
        return false;
    }
    let trait_ref = cx.tcx.impl_trait_ref(impl_id);
    let trait_id = trait_ref.instantiate_identity().def_id;
    let try_from = cx
        .tcx
        .get_diagnostic_item(rustc_span::symbol::Symbol::intern("TryFrom"));
    let try_into = cx
        .tcx
        .get_diagnostic_item(rustc_span::symbol::Symbol::intern("TryInto"));
    if try_from != Some(trait_id) && try_into != Some(trait_id) {
        return false;
    }

    let mut expansion = cx.tcx.expn_that_defined(id);
    while expansion != rustc_span::ExpnId::root() {
        let data = expansion.expn_data();
        if data.macro_def_id.is_some_and(|macro_id| {
            cx.tcx.crate_name(macro_id.krate).as_str() == "enum_dispatch"
                && cx.tcx.item_name(macro_id).as_str() == "enum_dispatch"
        }) {
            return true;
        }
        expansion = data.parent;
    }
    false
}

fn check(
    cx: &LateContext<'_>,
    id: rustc_hir::def_id::DefId,
    hir_id: rustc_hir::HirId,
    span: rustc_span::Span,
) {
    if is_enum_dispatch_conversion(cx, id) {
        return;
    }
    let sig = cx.tcx.fn_sig(id).instantiate_identity().skip_binder();
    let output = sig.output();
    let output = cx
        .tcx
        .try_normalize_erasing_regions(cx.typing_env(), output)
        .unwrap_or(output);
    let ty::Adt(def, args) = output.kind() else {
        return;
    };
    if !cx.tcx.is_diagnostic_item(sym::Result, def.did()) || args.len() < 2 {
        return;
    }
    let error = args.type_at(1);
    let is_unit = matches!(error.kind(), ty::Tuple(fields) if fields.is_empty());
    let is_str = matches!(error.kind(), ty::Ref(_, inner, _) if matches!(inner.kind(), ty::Str));
    let is_string = matches!(error.kind(), ty::Adt(string, _) if cx.tcx.def_path_str(string.did()) == "alloc::string::String" || cx.tcx.def_path_str(string.did()) == "std::string::String");
    if is_unit || is_str || is_string {
        cx.tcx
            .node_span_lint(UNTYPED_ERROR_RESULT, hir_id, span, |diag| {
                diag.primary_message("function returns a Result with an untyped error");
                diag.help(
                    "use a layer-appropriate error type; see \
                     docs/code-guidelines/CG-010-typed-error-result.md",
                );
            });
    }
}

impl<'tcx> LateLintPass<'tcx> for UntypedErrorResult {
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        if matches!(item.kind, ItemKind::Fn { .. }) {
            check(cx, item.owner_id.to_def_id(), item.hir_id(), item.span);
        }
    }

    fn check_impl_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx ImplItem<'tcx>) {
        if matches!(item.kind, ImplItemKind::Fn { .. }) {
            check(cx, item.owner_id.to_def_id(), item.hir_id(), item.span);
        }
    }

    fn check_trait_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx TraitItem<'tcx>) {
        if matches!(item.kind, TraitItemKind::Fn { .. }) {
            check(cx, item.owner_id.to_def_id(), item.hir_id(), item.span);
        }
    }

    fn check_foreign_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx ForeignItem<'tcx>) {
        if matches!(item.kind, ForeignItemKind::Fn(..)) {
            check(cx, item.owner_id.to_def_id(), item.hir_id(), item.span);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ui() {
        crate::testing::run_ui(
            "untyped_error_result",
            r#"#![allow(dead_code)]
use std::string::String;
type Alias = String;
fn unit() -> Result<(), ()> { Ok(()) }
fn borrowed() -> Result<(), &'static str> { Ok(()) }
fn owned() -> Result<(), Alias> { Ok(()) }
fn typed() -> Result<(), u8> { Ok(()) }
struct S;
impl S { fn private_test(&self) -> Result<(), String> { Ok(()) } }
trait T { fn trait_method() -> Result<(), String>; }
struct GeneratedDispatch;
struct GeneratedInto;
#[enum_dispatch::enum_dispatch]
trait MacroTrait { fn handwritten(&self) -> Result<(), ()> { Ok(()) } }
mod custom_result {
    struct Result<T, const N: usize>(T);
    fn custom_result() -> Result<(), { 1 }> {
        Result(())
    }
}
unsafe extern "Rust" { fn foreign() -> Result<(), String>; }
fn ordinary() -> Result<(), String> { Ok(()) }
fn main() {}
"#,
            r###"error: function returns a Result with an untyped error
  --> $DIR/untyped_error_result.rs:4:1
   |
LL | fn unit() -> Result<(), ()> { Ok(()) }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a layer-appropriate error type; see docs/code-guidelines/CG-010-typed-error-result.md
   = note: `#[deny(untyped_error_result)]` on by default

error: function returns a Result with an untyped error
  --> $DIR/untyped_error_result.rs:5:1
   |
LL | fn borrowed() -> Result<(), &'static str> { Ok(()) }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a layer-appropriate error type; see docs/code-guidelines/CG-010-typed-error-result.md

error: function returns a Result with an untyped error
  --> $DIR/untyped_error_result.rs:6:1
   |
LL | fn owned() -> Result<(), Alias> { Ok(()) }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a layer-appropriate error type; see docs/code-guidelines/CG-010-typed-error-result.md

error: function returns a Result with an untyped error
  --> $DIR/untyped_error_result.rs:9:10
   |
LL | impl S { fn private_test(&self) -> Result<(), String> { Ok(()) } }
   |          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a layer-appropriate error type; see docs/code-guidelines/CG-010-typed-error-result.md

error: function returns a Result with an untyped error
  --> $DIR/untyped_error_result.rs:10:11
   |
LL | trait T { fn trait_method() -> Result<(), String>; }
   |           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a layer-appropriate error type; see docs/code-guidelines/CG-010-typed-error-result.md

error: function returns a Result with an untyped error
  --> $DIR/untyped_error_result.rs:14:20
   |
LL | trait MacroTrait { fn handwritten(&self) -> Result<(), ()> { Ok(()) } }
   |                    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a layer-appropriate error type; see docs/code-guidelines/CG-010-typed-error-result.md

error: function returns a Result with an untyped error
  --> $DIR/untyped_error_result.rs:21:24
   |
LL | unsafe extern "Rust" { fn foreign() -> Result<(), String>; }
   |                        ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a layer-appropriate error type; see docs/code-guidelines/CG-010-typed-error-result.md

error: function returns a Result with an untyped error
  --> $DIR/untyped_error_result.rs:22:1
   |
LL | fn ordinary() -> Result<(), String> { Ok(()) }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a layer-appropriate error type; see docs/code-guidelines/CG-010-typed-error-result.md

error: aborting due to 8 previous errors

"###,
            &[crate::testing::MockCrate {
                name: "enum_dispatch",
                proc_macro: true,
                source: r##"extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn enum_dispatch(_: TokenStream, mut item: TokenStream) -> TokenStream {
    let generated: TokenStream = r#"
        impl ::core::convert::TryFrom<u8> for GeneratedDispatch {
            type Error = &'static str;
            fn try_from(_: u8) -> ::core::result::Result<Self, Self::Error> { Err("invalid") }
        }
        impl ::core::convert::TryInto<u16> for GeneratedInto {
            type Error = &'static str;
            fn try_into(self) -> ::core::result::Result<u16, Self::Error> { Err("invalid") }
        }
    "#.parse().unwrap();
    item.extend(generated);
    item
}
"##,
            }],
        );
    }
}
