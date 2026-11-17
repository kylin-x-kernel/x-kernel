// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `untracked_runtime_lock`: runtime `ksync` locks initialized with `const_new`.
//!
//! Rule: `docs/code-guidelines/CG-003-runtime-lock-tracking.md`.

use rustc_hir::{Expr, ExprKind, ItemKind, Node};
use rustc_lint::{LateContext, LateLintPass};

use crate::static_lock_helpers::{direct_call_to_type, peel_blocks};

rustc_session::declare_lint! {
    /// ### What it does
    ///
    /// Reports runtime calls to `ksync::Mutex::const_new` and
    /// `ksync::RwLock::const_new` when the tracked `new` constructor can be used.
    ///
    /// ### Why is this bad?
    ///
    /// With lock statistics enabled, `new` tracks the initialization site while
    /// runtime uses of `const_new` lose that tracking.
    ///
    /// ### Known problems
    ///
    /// Const contexts and calls that do not directly use the default raw lock
    /// constructor are outside the lint. Custom raw locks are not tracked here.
    /// A `const fn` call site is reported;
    /// its allow reason must name a real const initializer caller.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// fn make() -> ksync::Mutex<u32> {
    ///     ksync::Mutex::const_new(ksync::RawMutex::new(), 0)
    /// }
    /// ```
    ///
    /// Use `Mutex::new` for runtime construction.
    pub UNTRACKED_RUNTIME_LOCK,
    Deny,
    "runtime lock uses const_new instead of tracked new",
    report_in_external_macro
}

rustc_session::declare_lint_pass!(UntrackedRuntimeLock => [UNTRACKED_RUNTIME_LOCK]);

/// Returns whether this expression is in a const context before crossing a closure.
fn is_const_context<'tcx>(cx: &LateContext<'tcx>, expression: &Expr<'tcx>) -> bool {
    for (_, node) in cx.tcx.hir_parent_iter(expression.hir_id) {
        match node {
            Node::Expr(parent) if matches!(parent.kind, ExprKind::Closure(..)) => return false,
            Node::Expr(parent) if matches!(parent.kind, ExprKind::ConstBlock(..)) => return true,
            Node::AnonConst(_) => return true,
            Node::Item(item) => {
                return matches!(item.kind, ItemKind::Const(..) | ItemKind::Static(..));
            }
            Node::TraitItem(item) => {
                return matches!(item.kind, rustc_hir::TraitItemKind::Const(..));
            }
            Node::ImplItem(item) => {
                return matches!(item.kind, rustc_hir::ImplItemKind::Const(..));
            }
            _ => {}
        }
    }
    false
}

impl<'tcx> LateLintPass<'tcx> for UntrackedRuntimeLock {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expression: &'tcx Expr<'tcx>) {
        if !matches!(expression.kind, ExprKind::Call(..)) {
            return;
        }
        let tcx = cx.tcx;
        let types = cx.typeck_results();
        let mutex = direct_call_to_type(tcx, types, expression, "ksync", "Mutex", "const_new");
        let rwlock = direct_call_to_type(tcx, types, expression, "ksync", "RwLock", "const_new");
        let Some(call) = mutex.or(rwlock) else {
            return;
        };
        // Walking the HIR parents is the expensive step; do it only for the
        // `const_new` calls that can be reported.
        if is_const_context(cx, expression) {
            return;
        }
        let lock_name = if mutex.is_some() { "Mutex" } else { "RwLock" };
        let ExprKind::Call(_, arguments) = expression.kind else {
            return;
        };
        let Some(raw_lock) = arguments.first().map(|argument| peel_blocks(argument)) else {
            return;
        };
        let raw_name = if lock_name == "Mutex" {
            "RawMutex"
        } else {
            "RawRwLock"
        };
        if direct_call_to_type(tcx, types, raw_lock, "ksync", raw_name, "new").is_none() {
            return;
        }

        tcx.node_span_lint(
            UNTRACKED_RUNTIME_LOCK,
            expression.hir_id,
            call,
            |diagnostic| {
                diagnostic.primary_message(format!(
                    "runtime {lock_name} construction uses `const_new` instead of tracked `new`"
                ));
                diagnostic.span_label(
                    call,
                    "use `new` so this runtime initialization site is tracked",
                );
                diagnostic.help(
                    "if this const function has a const-initializer caller, suppress with a \
                     reason naming that caller; see \
                     docs/code-guidelines/CG-003-runtime-lock-tracking.md",
                );
            },
        );
    }
}

#[cfg(test)]
mod tests {
    const SOURCE: &str = r#"#![allow(dead_code, unused)]

mod flagged {
    use ksync::{Mutex, RawMutex, RwLock, RawRwLock};

    fn make_mutex() -> Mutex<u32> { Mutex::const_new(RawMutex::new(), 0) }
    fn make_rwlock() -> RwLock<u32> { RwLock::const_new(RawRwLock::new(), 0) }

    mod aliases {
        use ksync::{Mutex as M, RawMutex as R};
        fn make() -> M<u32> { M::const_new(R::new(), 1) }
    }
}

mod constant_initializers {
    use ksync::{Mutex, RawMutex, RwLock, RawRwLock};
    const A: Mutex<u32> = Mutex::const_new(RawMutex::new(), 0);
    const B: RwLock<u32> = RwLock::const_new(RawRwLock::new(), 0);
}

mod inline_const_initializer {
    use ksync::{Mutex, RawMutex};
    fn make() -> Mutex<u32> {
        const { Mutex::const_new(RawMutex::new(), 0) }
    }
}

mod closures_are_runtime {
    use std::sync::LazyLock;
    use ksync::{Mutex, RawMutex};
    static LOCK: LazyLock<Mutex<u32>> =
        LazyLock::new(|| Mutex::const_new(RawMutex::new(), 0));
    const FACTORY: fn() -> Mutex<u32> = || Mutex::const_new(RawMutex::new(), 1);
}

mod const_fn_exception {
    use ksync::{Mutex, RawMutex};
    #[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
    #[allow(unknown_lints, untracked_runtime_lock,
        reason = "CG-003-runtime-lock-tracking: called from CONST_LOCK")]
    const fn make() -> Mutex<u32> { Mutex::const_new(RawMutex::new(), 0) }
    const CONST_LOCK: Mutex<u32> = make();
}

mod raw_config_exception {
    use ksync::{Mutex, RawMutex, RwLock, RawRwLock, SpinConfig};
    fn make_mutex() -> Mutex<u32> {
        Mutex::const_new(RawMutex::with_config(SpinConfig), 0)
    }
    fn make_rwlock_from_variable() -> RwLock<u32> {
        let raw = RawRwLock::new();
        RwLock::const_new(raw, 0)
    }
    fn make_mutex_from_variable() -> Mutex<u32> {
        let raw = RawMutex::with_config(SpinConfig);
        Mutex::const_new(raw, 0)
    }
}

fn main() {}
"#;

    const STDERR: &str = r#"error: runtime Mutex construction uses `const_new` instead of tracked `new`
  --> $DIR/untracked_runtime_lock.rs:6:37
   |
LL |     fn make_mutex() -> Mutex<u32> { Mutex::const_new(RawMutex::new(), 0) }
   |                                     ^^^^^^^^^^^^^^^^ use `new` so this runtime initialization site is tracked
   |
   = help: if this const function has a const-initializer caller, suppress with a reason naming that caller; see docs/code-guidelines/CG-003-runtime-lock-tracking.md
   = note: `#[deny(untracked_runtime_lock)]` on by default

error: runtime RwLock construction uses `const_new` instead of tracked `new`
  --> $DIR/untracked_runtime_lock.rs:7:39
   |
LL |     fn make_rwlock() -> RwLock<u32> { RwLock::const_new(RawRwLock::new(), 0) }
   |                                       ^^^^^^^^^^^^^^^^^ use `new` so this runtime initialization site is tracked
   |
   = help: if this const function has a const-initializer caller, suppress with a reason naming that caller; see docs/code-guidelines/CG-003-runtime-lock-tracking.md

error: runtime Mutex construction uses `const_new` instead of tracked `new`
  --> $DIR/untracked_runtime_lock.rs:11:31
   |
LL |         fn make() -> M<u32> { M::const_new(R::new(), 1) }
   |                               ^^^^^^^^^^^^ use `new` so this runtime initialization site is tracked
   |
   = help: if this const function has a const-initializer caller, suppress with a reason naming that caller; see docs/code-guidelines/CG-003-runtime-lock-tracking.md

error: runtime Mutex construction uses `const_new` instead of tracked `new`
  --> $DIR/untracked_runtime_lock.rs:32:26
   |
LL |         LazyLock::new(|| Mutex::const_new(RawMutex::new(), 0));
   |                          ^^^^^^^^^^^^^^^^ use `new` so this runtime initialization site is tracked
   |
   = help: if this const function has a const-initializer caller, suppress with a reason naming that caller; see docs/code-guidelines/CG-003-runtime-lock-tracking.md

error: runtime Mutex construction uses `const_new` instead of tracked `new`
  --> $DIR/untracked_runtime_lock.rs:33:44
   |
LL |     const FACTORY: fn() -> Mutex<u32> = || Mutex::const_new(RawMutex::new(), 1);
   |                                            ^^^^^^^^^^^^^^^^ use `new` so this runtime initialization site is tracked
   |
   = help: if this const function has a const-initializer caller, suppress with a reason naming that caller; see docs/code-guidelines/CG-003-runtime-lock-tracking.md

error: aborting due to 5 previous errors

"#;

    #[test]
    fn ui() {
        crate::testing::run_ui(
            "untracked_runtime_lock",
            SOURCE,
            STDERR,
            &[crate::testing::MOCK_KSYNC],
        );
    }
}
