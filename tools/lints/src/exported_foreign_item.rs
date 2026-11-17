// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Detects foreign items exported from a crate. See CG-012.

use rustc_hir::ForeignItem;
use rustc_lint::{LateContext, LateLintPass};

rustc_session::declare_lint! {
    /// ### What it does
    /// Reports foreign declarations reachable outside their crate.
    /// ### Why is this bad?
    /// Raw ABI declarations expose safety and ownership obligations to callers.
    /// ### Example
    /// `pub unsafe extern "C" { pub fn raw_operation(); }`
    pub EXPORTED_FOREIGN_ITEM, Deny, "foreign item is reachable outside its crate", report_in_external_macro
}
rustc_session::declare_lint_pass!(ExportedForeignItem => [EXPORTED_FOREIGN_ITEM]);

impl<'tcx> LateLintPass<'tcx> for ExportedForeignItem {
    fn check_foreign_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx ForeignItem<'tcx>) {
        if !cx
            .tcx
            .effective_visibilities(())
            .is_exported(item.owner_id.def_id)
        {
            return;
        }
        cx.tcx
            .node_span_lint(EXPORTED_FOREIGN_ITEM, item.hir_id(), item.span, |diag| {
                diag.primary_message("foreign declaration is reachable outside its crate");
                diag.help(
                    "keep the extern item private and expose a wrapper; see \
                     docs/code-guidelines/CG-012-private-foreign-items.md",
                );
            });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ui() {
        crate::testing::run_ui(
            "exported_foreign_item",
            r#"#![allow(dead_code)]
unsafe extern "C" { pub fn visible(); }
unsafe extern "C" { fn private(); }
mod hidden { unsafe extern "C" { pub fn module_private(); pub fn reexported(); } }
pub mod exposed { unsafe extern "C" { pub fn reacheable(); } }
pub use hidden::reexported;
fn main() {}
"#,
            r###"error: foreign declaration is reachable outside its crate
  --> $DIR/exported_foreign_item.rs:2:21
   |
LL | unsafe extern "C" { pub fn visible(); }
   |                     ^^^^^^^^^^^^^^^^^
   |
   = help: keep the extern item private and expose a wrapper; see docs/code-guidelines/CG-012-private-foreign-items.md
   = note: `#[deny(exported_foreign_item)]` on by default

error: foreign declaration is reachable outside its crate
  --> $DIR/exported_foreign_item.rs:4:59
   |
LL | mod hidden { unsafe extern "C" { pub fn module_private(); pub fn reexported(); } }
   |                                                           ^^^^^^^^^^^^^^^^^^^^
   |
   = help: keep the extern item private and expose a wrapper; see docs/code-guidelines/CG-012-private-foreign-items.md

error: foreign declaration is reachable outside its crate
  --> $DIR/exported_foreign_item.rs:5:39
   |
LL | pub mod exposed { unsafe extern "C" { pub fn reacheable(); } }
   |                                       ^^^^^^^^^^^^^^^^^^^^
   |
   = help: keep the extern item private and expose a wrapper; see docs/code-guidelines/CG-012-private-foreign-items.md

error: aborting due to 3 previous errors

"###,
            &[],
        );
    }
}
