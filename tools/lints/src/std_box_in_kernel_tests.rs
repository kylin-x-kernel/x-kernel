// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Rejects explicit standard `Box` use in kernel unit tests (CG-049).

use std::sync::{Arc, Mutex};

use rustc_ast::{AssocItem, AttrStyle, Attribute, Item, MetaItemInner, MetaItemKind};
use rustc_hir::{
    Expr, ExprKind, HirId, Path,
    def::{DefKind, Res},
};
use rustc_lint::{EarlyContext, EarlyLintPass, LateContext, LateLintPass, LintContext, LintStore};
use rustc_span::{ExpnId, FileName, Span, Symbol, sym};

rustc_session::declare_lint! {
    /// Detects resolved standard `Box` types, associated functions and calls
    /// returning `Box` in `ut-tests/` source files and explicitly test-gated
    /// items, only when `--cfg unittest` is active. Use fallible kernel owners.
    /// External macro implementations are excluded: `vec!` uses `Box`
    /// internally. Local macros and source-spanned arguments are checked.
    pub STD_BOX_IN_KERNEL_TESTS,
    Deny,
    "standard Box in kernel unit tests"
}

type Scopes = Arc<Mutex<Vec<Span>>>;

struct CollectTestScopes(Scopes);
struct StdBoxInKernelTests(Scopes);

rustc_session::impl_lint_pass!(CollectTestScopes => []);
rustc_session::impl_lint_pass!(StdBoxInKernelTests => [STD_BOX_IN_KERNEL_TESTS]);

pub(crate) fn register(store: &mut LintStore) {
    let scopes = Scopes::default();
    let collected = scopes.clone();
    store.register_pre_expansion_pass(move || Box::new(CollectTestScopes(collected.clone())));
    store.register_late_pass(move |_| Box::new(StdBoxInKernelTests(scopes.clone())));
}

fn is_test_attribute(attribute: &Attribute) -> bool {
    attribute.style == AttrStyle::Outer
        && attribute.has_name(sym::cfg)
        && attribute
            .meta_item_list()
            .is_some_and(|items| items.iter().any(requires_test))
}

fn requires_test(item: &MetaItemInner) -> bool {
    if item.has_name(sym::test) || item.has_name(Symbol::intern("unittest")) {
        return true;
    }
    let MetaItemInner::MetaItem(meta) = item else {
        return false;
    };
    let MetaItemKind::List(items) = &meta.kind else {
        return false;
    };
    if meta.has_name(sym::all) {
        items.iter().any(requires_test)
    } else if meta.has_name(sym::any) {
        !items.is_empty() && items.iter().all(requires_test)
    } else {
        false
    }
}

impl CollectTestScopes {
    fn collect_fields(&self, data: &rustc_ast::VariantData) {
        for field in data.fields() {
            if field.attrs.iter().any(is_test_attribute) {
                self.0.lock().unwrap().push(field.span);
            }
        }
    }
}

impl EarlyLintPass for CollectTestScopes {
    fn check_item(&mut self, _: &EarlyContext<'_>, item: &Item) {
        if item.attrs.iter().any(is_test_attribute) {
            self.0.lock().unwrap().push(item.span);
        }
        if let rustc_ast::ItemKind::Struct(_, _, data) | rustc_ast::ItemKind::Union(_, _, data) =
            &item.kind
        {
            self.collect_fields(data);
        }
    }

    fn check_impl_item(&mut self, _: &EarlyContext<'_>, item: &AssocItem) {
        if item.attrs.iter().any(is_test_attribute) {
            self.0.lock().unwrap().push(item.span);
        }
    }

    fn check_trait_item(&mut self, _: &EarlyContext<'_>, item: &AssocItem) {
        if item.attrs.iter().any(is_test_attribute) {
            self.0.lock().unwrap().push(item.span);
        }
    }

    fn check_expr(&mut self, _: &EarlyContext<'_>, expression: &rustc_ast::Expr) {
        if expression.attrs.iter().any(is_test_attribute) {
            self.0.lock().unwrap().push(expression.span);
        }
    }

    fn check_local(&mut self, _: &EarlyContext<'_>, local: &rustc_ast::Local) {
        if local.attrs.iter().any(is_test_attribute) {
            self.0.lock().unwrap().push(local.span);
        }
    }

    fn check_variant(&mut self, _: &EarlyContext<'_>, variant: &rustc_ast::Variant) {
        if variant.attrs.iter().any(is_test_attribute) {
            self.0.lock().unwrap().push(variant.span);
        }
        self.collect_fields(&variant.data);
    }
}

fn authored_span(span: Span) -> Option<Span> {
    let mut expansion = span.ctxt().outer_expn();
    while expansion != ExpnId::root() {
        let data = expansion.expn_data();
        if data
            .macro_def_id
            .is_some_and(|definition| !definition.is_local())
        {
            return None;
        }
        expansion = data.parent;
    }
    Some(span.source_callsite())
}

fn is_ut_source(cx: &LateContext<'_>, span: Span, scopes: &Scopes) -> bool {
    let Some(span) = authored_span(span) else {
        return false;
    };
    if !cx
        .sess()
        .psess
        .config
        .contains(&(Symbol::intern("unittest"), None))
    {
        return false;
    }
    if scopes
        .lock()
        .unwrap()
        .iter()
        .any(|scope| scope.contains(span))
    {
        return true;
    }
    let file = cx.sess().source_map().lookup_source_file(span.lo());
    let FileName::Real(name) = &file.name else {
        return false;
    };
    name.local_path().is_some_and(|path| {
        path.components()
            .any(|component| component.as_os_str() == "ut-tests")
    })
}

fn resolves_to_box(cx: &LateContext<'_>, resolution: Res) -> bool {
    let Res::Def(kind, definition) = resolution else {
        return false;
    };
    let owner = match kind {
        DefKind::Struct | DefKind::TyAlias => definition,
        DefKind::AssocFn => match cx.tcx.impl_of_assoc(definition) {
            Some(owner) => owner,
            None => return false,
        },
        _ => return false,
    };
    let declared = cx.tcx.type_of(owner).instantiate_identity();
    cx.tcx
        .try_normalize_erasing_regions(cx.typing_env(), declared)
        .unwrap_or(declared)
        .is_box()
}

fn report(cx: &LateContext<'_>, hir_id: HirId, span: Span) {
    cx.tcx.node_span_lint(
        STD_BOX_IN_KERNEL_TESTS,
        hir_id,
        span.source_callsite(),
        |diagnostic| {
            diagnostic.primary_message("standard `Box` is not allowed in kernel unit tests");
            diagnostic
                .help("use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor");
        },
    );
}

impl<'tcx> LateLintPass<'tcx> for StdBoxInKernelTests {
    fn check_path(&mut self, cx: &LateContext<'tcx>, path: &Path<'tcx>, hir_id: HirId) {
        if matches!(cx.tcx.hir_node(hir_id), rustc_hir::Node::Item(item) if matches!(item.kind, rustc_hir::ItemKind::Use(..)))
        {
            return;
        }
        if is_ut_source(cx, path.span, &self.0) && resolves_to_box(cx, path.res) {
            report(cx, hir_id, path.span);
        }
    }

    fn check_expr(&mut self, cx: &LateContext<'tcx>, expression: &'tcx Expr<'tcx>) {
        if !is_ut_source(cx, expression.span, &self.0)
            || !matches!(
                expression.kind,
                ExprKind::Call(..) | ExprKind::MethodCall(..)
            )
            || !cx.typeck_results().expr_ty(expression).is_box()
        {
            return;
        }
        if let ExprKind::Call(callee, _) = expression.kind
            && let ExprKind::Path(ref path) = callee.kind
            && resolves_to_box(cx, cx.qpath_res(path, callee.hir_id))
        {
            return;
        }
        report(cx, expression.hir_id, expression.span);
    }
}

#[cfg(test)]
mod tests {
    const EXTERNAL_MACROS: crate::testing::MockCrate = crate::testing::MockCrate {
        name: "external_macros",
        proc_macro: false,
        source: r#"#[macro_export]
macro_rules! boxed { () => { Box::new(1) }; }
"#,
    };

    const SOURCE: &str = r#"// compile-flags: --cfg unittest --cfg test
#![allow(dead_code, unused_imports)]
#![cfg(unittest)]
extern crate alloc;
use std::boxed::Box as Renamed;
type Alias<T> = Renamed<T>;
fn production() -> Box<u8> { Box::new(1) }
macro_rules! production_macro { () => { Box::new(1) }; }
fn production_macro_caller() { let _ = production_macro!(); }
#[cfg(unittest)]
fn inline_helper() { let _ = Box::new(1); }
#[cfg(test)]
mod inline_tests {
    fn helper() { let _ = Box::new(1); }
}
#[cfg(all(unittest, test))]
fn compound_gate() { let _ = Box::new(1); }
#[cfg(any(unittest, unix))]
fn not_exclusively_tests() { let _ = Box::new(1); }
struct Helper;
impl Helper {
    #[cfg(unittest)]
    fn method() { let _ = Box::new(1); }
}
#[path = "ut-tests/cases.inc"]
mod cases;
#[path = "not-ut-tests/production.inc"]
mod other_production;
fn local_scope() {
    #[cfg(unittest)]
    let probe = Box::new(1);
    let production = Box::new(2);
}
struct State {
    #[cfg(unittest)]
    probe: Box<u8>,
    production: Box<u8>,
}
enum StateKind {
    #[cfg(unittest)]
    Probe(Box<u8>),
    Production(Box<u8>),
    Fields {
        #[cfg(unittest)]
        probe: Box<u8>,
        production: Box<u8>,
    },
}
fn main() {}
"#;

    const CASES: &str = r#"use std::boxed::Box as Renamed;
type LocalAlias<T> = Renamed<T>;
fn explicit(value: alloc::boxed::Box<u8>) {
    let _ = Box::new(1);
    let _ = Renamed::new(2);
    let _ = crate::Alias::new(3);
    let _ = LocalAlias::new(4);
    let _: crate::Alias<u8> = value;
    let _ = vec![1, 2].into_boxed_slice();
    let _ = String::from("hello").into_boxed_str();
    let _ = crate::production();
    let _ = Box::leak(Box::new(5));
}
fn allowed() {
    let _ = vec![1, 2, 3];
    let _ = vec![1; 3];
    let _ = Vec::<u8>::new();
    let _ = String::new();
    let _ = unrelated::Box::new();
    let _ = unrelated::Box::new().into_boxed_slice();
}
mod unrelated {
    pub struct Box;
    impl Box {
        pub fn new() -> Self { Self }
        pub fn into_boxed_slice(self) -> Self { self }
    }
}
macro_rules! generated { () => { Box::new(1) }; }
macro_rules! passthrough { ($value:expr) => { $value }; }
macro_rules! vector { () => { vec![1, 2] }; }
fn macros() {
    let _ = generated!();
    let _ = production_macro!();
    let _ = external_macros::boxed!();
    let _ = vector!();
    let _ = passthrough!(Box::new(1));
    let _ = vec![Box::new(1)];
}
#[allow(unknown_lints, std_box_in_kernel_tests, reason = "CG-049-no-std-box-in-kernel-tests: test scenario: production interface requires Box")]
fn existing_contract() -> Box<u8> { Box::new(1) }
fn qualified() {
    let _ = <crate::Alias<u8>>::new(1);
    let _ = <Box<u8> as From<u8>>::from(1);
    let constructor = Box::<u8>::new;
    let _ = constructor(1);
    let _ = Vec::into_boxed_slice(vec![1]);
    let _ = String::into_boxed_str(String::new());
}
"#;

    const EXPECTED: &str = r#"error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/std_box_in_kernel_tests.rs:11:30
   |
LL | fn inline_helper() { let _ = Box::new(1); }
   |                              ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor
   = note: `#[deny(std_box_in_kernel_tests)]` on by default

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/std_box_in_kernel_tests.rs:14:27
   |
LL |     fn helper() { let _ = Box::new(1); }
   |                           ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/std_box_in_kernel_tests.rs:17:30
   |
LL | fn compound_gate() { let _ = Box::new(1); }
   |                              ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/std_box_in_kernel_tests.rs:23:27
   |
LL |     fn method() { let _ = Box::new(1); }
   |                           ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:2:22
   |
LL | type LocalAlias<T> = Renamed<T>;
   |                      ^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:3:20
   |
LL | fn explicit(value: alloc::boxed::Box<u8>) {
   |                    ^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:4:13
   |
LL |     let _ = Box::new(1);
   |             ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:5:13
   |
LL |     let _ = Renamed::new(2);
   |             ^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:6:13
   |
LL |     let _ = crate::Alias::new(3);
   |             ^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:7:13
   |
LL |     let _ = LocalAlias::new(4);
   |             ^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:8:12
   |
LL |     let _: crate::Alias<u8> = value;
   |            ^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:9:13
   |
LL |     let _ = vec![1, 2].into_boxed_slice();
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:10:13
   |
LL |     let _ = String::from("hello").into_boxed_str();
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:11:13
   |
LL |     let _ = crate::production();
   |             ^^^^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:12:13
   |
LL |     let _ = Box::leak(Box::new(5));
   |             ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:12:23
   |
LL |     let _ = Box::leak(Box::new(5));
   |                       ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:33:13
   |
LL |     let _ = generated!();
   |             ^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:34:13
   |
LL |     let _ = production_macro!();
   |             ^^^^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:37:26
   |
LL |     let _ = passthrough!(Box::new(1));
   |                          ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:38:18
   |
LL |     let _ = vec![Box::new(1)];
   |                  ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:43:14
   |
LL |     let _ = <crate::Alias<u8>>::new(1);
   |              ^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:44:13
   |
LL |     let _ = <Box<u8> as From<u8>>::from(1);
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:44:14
   |
LL |     let _ = <Box<u8> as From<u8>>::from(1);
   |              ^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:45:23
   |
LL |     let constructor = Box::<u8>::new;
   |                       ^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:46:13
   |
LL |     let _ = constructor(1);
   |             ^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:47:13
   |
LL |     let _ = Vec::into_boxed_slice(vec![1]);
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/ut-tests/cases.inc:48:13
   |
LL |     let _ = String::into_boxed_str(String::new());
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/std_box_in_kernel_tests.rs:31:17
   |
LL |     let probe = Box::new(1);
   |                 ^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/std_box_in_kernel_tests.rs:36:12
   |
LL |     probe: Box<u8>,
   |            ^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/std_box_in_kernel_tests.rs:41:11
   |
LL |     Probe(Box<u8>),
   |           ^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: standard `Box` is not allowed in kernel unit tests
  --> $DIR/std_box_in_kernel_tests.rs:45:16
   |
LL |         probe: Box<u8>,
   |                ^^^^^^^
   |
   = help: use `KBox<T>`, `KBox<[T]>` or `KBox<str>` with a fallible constructor

error: aborting due to 31 previous errors

"#;

    #[test]
    fn ui() {
        crate::testing::run_ui_with_files(
            "std_box_in_kernel_tests",
            SOURCE,
            EXPECTED,
            &[EXTERNAL_MACROS],
            &[
                ("ut-tests/cases.inc", CASES),
                (
                    "not-ut-tests/production.inc",
                    "fn production() -> Box<u8> { Box::new(1) }\n",
                ),
            ],
        );
    }

    #[test]
    fn host_tests_are_excluded() {
        let source = SOURCE
            .replace("--cfg unittest --cfg test", "--cfg test")
            .replace("#![cfg(unittest)]", "");
        crate::testing::run_ui_with_files(
            "std_box_in_kernel_tests_host",
            &source,
            "",
            &[EXTERNAL_MACROS],
            &[
                ("ut-tests/cases.inc", CASES),
                (
                    "not-ut-tests/production.inc",
                    "fn production() -> Box<u8> { Box::new(1) }\n",
                ),
            ],
        );
    }
}
