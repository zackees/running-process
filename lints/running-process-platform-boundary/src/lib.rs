#![feature(rustc_private)]

extern crate rustc_hir;

use dylint_linting::declare_late_lint;
use rustc_hir::{def::Res, Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass};

declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects a raw process API recorded in `raw_platform_apis.toml` outside
    /// `running-process-platform-internal`.
    ///
    /// ### Why is this bad?
    ///
    /// The async migration has one platform choke point. Direct Tokio process
    /// handles and stdio policy bypass typed containment, cleanup, and future
    /// actor ownership rules.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// let _ = std::process::Stdio::piped();
    /// ```
    ///
    /// Use a manifest-listed `running_process_platform_internal` capability
    /// instead.
    pub RP_RAW_PLATFORM_API_OUTSIDE_INTERNAL,
    Deny,
    "raw platform process APIs are restricted to running-process-platform-internal"
}

impl<'tcx> LateLintPass<'tcx> for RpRawPlatformApiOutsideInternal {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &Expr<'tcx>) {
        let ExprKind::Path(ref qpath) = expr.kind else {
            return;
        };
        let Res::Def(_, def_id) = cx.qpath_res(qpath, expr.hir_id) else {
            return;
        };
        let raw_api = match cx.tcx.def_path_str(def_id).as_str() {
            "tokio::process::Command" | "tokio::process::Command::new" => "tokio::process::Command",
            "tokio::process::Child" => "tokio::process::Child",
            path if path.starts_with("std::process::Stdio::") => "std::process::Stdio",
            _ => return,
        };

        cx.tcx.dcx().span_err(
            expr.span,
            format!(
                "forbidden raw platform API `{raw_api}`; use a blessed running_process_platform_internal capability listed in blessed_api.toml"
            ),
        );
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
