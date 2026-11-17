// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Detects shifting `Vec` operations inside loops. See CG-009.

use rustc_hir::{Expr, ExprKind, ImplItemKind, ItemKind, Node, TraitItemKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty};

rustc_session::declare_lint! {
    /// ### What it does
    /// Reports `Vec::remove` and `Vec::insert` calls nested in loop bodies.
    /// ### Why is this bad?
    /// Each call shifts elements and repeated shifts can make loops quadratic.
    /// ### Example
    /// `for _ in 0..n { values.remove(0); }`
    pub VEC_SHIFT_IN_LOOP, Deny, "Vec::remove or Vec::insert inside a loop", report_in_external_macro
}
rustc_session::declare_lint_pass!(VecShiftInLoop => [VEC_SHIFT_IN_LOOP]);

fn is_vec(cx: &LateContext<'_>, ty: Ty<'_>) -> bool {
    let ty = ty.peel_refs();
    matches!(ty.kind(), ty::Adt(def, _) if cx.tcx.item_name(def.did()).as_str() == "Vec" && cx.tcx.crate_name(def.did().krate).as_str() == "alloc")
}

fn is_vec_shift_method(cx: &LateContext<'_>, method: rustc_hir::def_id::DefId) -> bool {
    if !matches!(cx.tcx.item_name(method).as_str(), "remove" | "insert") {
        return false;
    }
    let Some(impl_id) = cx.tcx.impl_of_assoc(method) else {
        return false;
    };
    is_vec(cx, cx.tcx.type_of(impl_id).instantiate_identity())
}

impl<'tcx> LateLintPass<'tcx> for VecShiftInLoop {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let method = match expr.kind {
            ExprKind::MethodCall(..) => {
                let Some(method) = cx.typeck_results().type_dependent_def_id(expr.hir_id) else {
                    return;
                };
                method
            }
            ExprKind::Call(callee, _) => {
                let ty::FnDef(method, _) = cx.typeck_results().expr_ty(callee).kind() else {
                    return;
                };
                *method
            }
            _ => return,
        };
        if !is_vec_shift_method(cx, method) {
            return;
        }
        for (_id, node) in cx.tcx.hir_parent_iter(expr.hir_id) {
            match node {
                Node::Expr(parent_expr) => match parent_expr.kind {
                    ExprKind::Loop(..) => {
                        cx.tcx
                            .node_span_lint(VEC_SHIFT_IN_LOOP, expr.hir_id, expr.span, |diag| {
                                diag.primary_message(format!(
                                    "`Vec::{}` shifts elements inside a loop",
                                    cx.tcx.item_name(method)
                                ));
                                diag.help(
                                    "bound the vector or restructure the loop; see \
                                     docs/code-guidelines/CG-009-vec-shift-in-loop.md",
                                );
                            });
                        return;
                    }
                    // Whether a closure runs once per iteration depends on how it is
                    // consumed, so this syntactic check deliberately stops here.
                    ExprKind::Closure(..) => return,
                    _ => {}
                },
                // Do not attribute an inner item to a syntactically enclosing loop.
                Node::Item(item) if matches!(item.kind, ItemKind::Fn { .. }) => return,
                Node::ImplItem(item) if matches!(item.kind, ImplItemKind::Fn { .. }) => return,
                Node::TraitItem(item) if matches!(item.kind, TraitItemKind::Fn { .. }) => return,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ui() {
        crate::testing::run_ui(
            "vec_shift_in_loop",
            r#"#![allow(dead_code)]
    fn main() {
    let mut v = vec![1, 2];
    for _ in 0..2 { v.remove(0); }
    while !v.is_empty() { v.insert(0, 1); v.remove(0); }
    v.remove(0);
    let mut borrowed = vec![1, 2];
    for _ in 0..2 { (&mut borrowed).remove(0); }
    let mut ufcs = vec![1, 2];
    for _ in 0..2 { Vec::remove(&mut ufcs, 0); }
    let mut boxed = Box::new(vec![1, 2]);
    for _ in 0..2 { boxed.remove(0); }
    for _ in 0..1 { let _deferred = || v.remove(0); }
    let mut returned = || v.remove(0);
    let _ = &mut returned;
    for _ in 0..1 { let _local_fn = fn_local; fn fn_local() { let mut x = vec![1]; x.remove(0); } }
    for _ in 0..2 {
        struct S;
        impl S { fn unused(mut v: Vec<u8>) { v.remove(0); } }
    }
    // Bounded vector exception.
    let mut bounded = vec![1, 2];
    #[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
    #[allow(unknown_lints, vec_shift_in_loop, reason = "CG-009-vec-shift-in-loop: vector grows from two to at most four entries")]
    for _ in 0..2 { bounded.insert(0, 1); }
}
"#,
            r###"error: `Vec::remove` shifts elements inside a loop
  --> $DIR/vec_shift_in_loop.rs:4:21
   |
LL |     for _ in 0..2 { v.remove(0); }
   |                     ^^^^^^^^^^^
   |
   = help: bound the vector or restructure the loop; see docs/code-guidelines/CG-009-vec-shift-in-loop.md
   = note: `#[deny(vec_shift_in_loop)]` on by default

error: `Vec::insert` shifts elements inside a loop
  --> $DIR/vec_shift_in_loop.rs:5:27
   |
LL |     while !v.is_empty() { v.insert(0, 1); v.remove(0); }
   |                           ^^^^^^^^^^^^^^
   |
   = help: bound the vector or restructure the loop; see docs/code-guidelines/CG-009-vec-shift-in-loop.md

error: `Vec::remove` shifts elements inside a loop
  --> $DIR/vec_shift_in_loop.rs:5:43
   |
LL |     while !v.is_empty() { v.insert(0, 1); v.remove(0); }
   |                                           ^^^^^^^^^^^
   |
   = help: bound the vector or restructure the loop; see docs/code-guidelines/CG-009-vec-shift-in-loop.md

error: `Vec::remove` shifts elements inside a loop
  --> $DIR/vec_shift_in_loop.rs:8:21
   |
LL |     for _ in 0..2 { (&mut borrowed).remove(0); }
   |                     ^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: bound the vector or restructure the loop; see docs/code-guidelines/CG-009-vec-shift-in-loop.md

error: `Vec::remove` shifts elements inside a loop
  --> $DIR/vec_shift_in_loop.rs:10:21
   |
LL |     for _ in 0..2 { Vec::remove(&mut ufcs, 0); }
   |                     ^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: bound the vector or restructure the loop; see docs/code-guidelines/CG-009-vec-shift-in-loop.md

error: `Vec::remove` shifts elements inside a loop
  --> $DIR/vec_shift_in_loop.rs:12:21
   |
LL |     for _ in 0..2 { boxed.remove(0); }
   |                     ^^^^^^^^^^^^^^^
   |
   = help: bound the vector or restructure the loop; see docs/code-guidelines/CG-009-vec-shift-in-loop.md

error: aborting due to 6 previous errors

"###,
            &[],
        );
    }
}
