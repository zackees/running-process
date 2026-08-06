#![feature(rustc_private)]

extern crate rustc_hir;

use dylint_linting::declare_late_lint;
use rustc_hir::{def::Res, Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass};

declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects direct references to the process-spawn capability outside the
    /// platform-internal crate.
    ///
    /// ### Why is this bad?
    ///
    /// Process creation is a policy boundary. Calling the standard library
    /// directly bypasses containment, ownership, and cancellation policy.
    /// Use a typed capability exported by `running-process-platform-internal`.
    pub RUNNING_PROCESS_PLATFORM_BOUNDARY,
    Deny,
    "process and PTY platform APIs must be reached through the blessed internal crate"
}

impl<'tcx> LateLintPass<'tcx> for RunningProcessPlatformBoundary {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &Expr<'tcx>) {
        let ExprKind::Path(ref qpath) = expr.kind else {
            return;
        };
        let Res::Def(_, def_id) = cx.qpath_res(qpath, expr.hir_id) else {
            return;
        };
        if cx.tcx.def_path_str(def_id) != "std::process::Command" {
            return;
        }
        cx.tcx.dcx().span_err(
            expr.span,
            "direct process creation is forbidden; use the blessed platform-internal capability",
        );
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
