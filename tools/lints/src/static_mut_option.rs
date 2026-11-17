// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `static_mut_option`: mutable statics whose type resolves to `Option<T>`.
//!
//! Rule: `docs/code-guidelines/CG-004-static-mut-option.md`.

use rustc_hir::{Item, ItemKind, Mutability};
use rustc_lint::{LateContext, LateLintPass};

use crate::static_lock_helpers::is_type;

rustc_session::declare_lint! {
    /// ### What it does
    ///
    /// Reports `static mut` items whose resolved type is `Option<T>`.
    ///
    /// ### Why is this bad?
    ///
    /// Open-coded mutable global options rely on manually maintained
    /// initialization and synchronization invariants.
    ///
    /// ### Known problems
    ///
    /// This lint checks the resolved outer type. A wrapper struct containing
    /// an option is not reported.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// static mut INSTANCE: Option<MyType> = None;
    /// ```
    ///
    /// Use a synchronized one-time initialization primitive instead.
    pub STATIC_MUT_OPTION,
    Deny,
    "open-coded static mut Option state",
    report_in_external_macro
}

rustc_session::declare_lint_pass!(StaticMutOption => [STATIC_MUT_OPTION]);

impl<'tcx> LateLintPass<'tcx> for StaticMutOption {
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        let ItemKind::Static(Mutability::Mut, ..) = item.kind else {
            return;
        };
        let tcx = cx.tcx;
        let declared = tcx.type_of(item.owner_id).instantiate_identity();
        let item_ty = tcx
            .try_normalize_erasing_regions(cx.typing_env(), declared)
            .unwrap_or(declared);
        if !is_type(tcx, item_ty, "core", "Option") {
            return;
        }
        tcx.node_span_lint(STATIC_MUT_OPTION, item.hir_id(), item.span, |diagnostic| {
            diagnostic.primary_message("avoid open-coded `static mut Option<T>` state");
            diagnostic.help(
                "use a synchronized one-time initialization primitive; see \
                 docs/code-guidelines/CG-004-static-mut-option.md",
            );
        });
    }
}

#[cfg(test)]
mod tests {
    const SOURCE: &str = r#"#![allow(dead_code, unused)]

mod flagged {
    static mut DIRECT: Option<u32> = None;

    type MaybeByte = Option<u8>;
    static mut ALIAS: MaybeByte = None;

    fn local() {
        static mut LOCAL: Option<()> = None;
    }
}

mod out_of_scope {
    static IMMUTABLE: Option<u32> = None;
    static mut VALUE: u32 = 0;

    struct Wrapper(Option<u32>);
    static mut WRAPPED: Wrapper = Wrapper(None);
}

fn main() {}
"#;

    const STDERR: &str = r#"error: avoid open-coded `static mut Option<T>` state
  --> $DIR/static_mut_option.rs:4:5
   |
LL |     static mut DIRECT: Option<u32> = None;
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a synchronized one-time initialization primitive; see docs/code-guidelines/CG-004-static-mut-option.md
   = note: `#[deny(static_mut_option)]` on by default

error: avoid open-coded `static mut Option<T>` state
  --> $DIR/static_mut_option.rs:7:5
   |
LL |     static mut ALIAS: MaybeByte = None;
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a synchronized one-time initialization primitive; see docs/code-guidelines/CG-004-static-mut-option.md

error: avoid open-coded `static mut Option<T>` state
  --> $DIR/static_mut_option.rs:10:9
   |
LL |         static mut LOCAL: Option<()> = None;
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: use a synchronized one-time initialization primitive; see docs/code-guidelines/CG-004-static-mut-option.md

error: aborting due to 3 previous errors

"#;

    #[test]
    fn ui() {
        crate::testing::run_ui("static_mut_option", SOURCE, STDERR, &[]);
    }
}
